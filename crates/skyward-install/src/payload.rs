//! A package that carries its partitions in a `payload.bin`, the way an A/B
//! update does, but which is not an A/B update: the partitions in it are
//! whole images, not a delta an update engine applies. The recovery takes
//! each partition's image out of the payload and writes it into the
//! partition of that name, so the phone is flashed without an update engine
//! and without the images ever touching a filesystem.
//!
//! The payload holds, per partition, a run of operations that put its image
//! together in order: bytes as they are, bytes compressed in one of the ways
//! Android compresses them, or a stretch of nothing. They are read out of the
//! payload as they are needed and written straight into the partition, so a
//! package far larger than the phone's memory is installed all the same.
//!
//! Every operation that carries data is checked against the hash its
//! manifest gives before a byte of it is written, and its stretches are
//! checked against the partition's size first: a payload whose offsets or
//! data do not say what they should stops the install rather than writing
//! something else over a partition.

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use skyward_vab::Slot;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use unpayload::metadata::{Extent, InstallOperation, OperationType, PartitionUpdate};
use unpayload::Payload;

/// How much of an operation's data is read at a time.
const CHUNK: usize = 1 << 20;

/// The most operation data one operation may name: the payload is read a
/// piece at a time, and a length past this is a manifest gone wrong.
const MOST_OPERATION_DATA: u64 = 64 << 20;

/// What a partition of the payload holds, and where its bytes are written.
#[derive(Debug, Clone)]
pub struct Target {
    /// The part of the partition's name in the payload: `system`, `vendor`.
    pub partition: String,
    /// The partition it goes into, by the name the phone gives it.
    pub device: PathBuf,
}

/// The payload of the package this recovery serves, read where it lies.
///
/// The package is a zip served as a file, and the payload in it is the whole
/// of what the package carries, so it is read out of the zip as it is.
pub fn open(package: &str) -> Result<Payload> {
    Payload::open(package).context("read the package's payload")
}

/// The handle the payload's bytes are read through: the package itself,
/// opened again so reading it does not move where the FUSE filesystem
/// serves it from.
pub fn package_file(package: &str) -> Result<File> {
    File::open(package).with_context(|| format!("open {package}"))
}

/// Whether every partition of `payload` is written from the payload alone,
/// with nothing taken from what the phone already holds.
///
/// An A/B update is a delta: most of what it writes is copied or patched
/// from the system already on the phone, and only its own program can put it
/// together. A package made to be flashed whole carries each partition's
/// image entire, so this recovery can write it itself, without that program
/// and without the phone having anything to copy from.
pub fn is_whole_image(payload: &Payload) -> bool {
    let whole = |op: &InstallOperation| {
        matches!(
            OperationType::try_from(op.r#type),
            Ok(OperationType::Replace
                | OperationType::ReplaceBz
                | OperationType::ReplaceXz
                | OperationType::ReplaceZstd
                | OperationType::Zero
                | OperationType::Discard)
        )
    };
    !payload.partitions().is_empty() && payload.partitions().iter().all(|p| p.operations.iter().all(whole))
}

/// The operations that put `update`'s image together, in the order they are
/// written.
pub fn operations(update: &PartitionUpdate) -> &[InstallOperation] {
    &update.operations
}

/// How many bytes the image of `update` takes.
pub fn image_size(update: &PartitionUpdate) -> u64 {
    update.new_partition_info.as_ref().and_then(|info| info.size).unwrap_or(0)
}

/// Write every partition of `payload` into the device its name goes to.
///
/// `targets` says which partition of the phone each partition of the payload
/// goes into, by the payload's own name; those the payload does not hold are
/// left alone. `base` is where the payload itself begins in `package`, which
/// is not always its start: a payload in a zip begins where the zip puts it,
/// and what the payload says of its own data is counted from its own first
/// byte. `progress` hears the bytes written so far, of all there are.
pub fn write_partitions(
    payload: &Payload,
    package: &File,
    base: u64,
    targets: &[Target],
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<String>> {
    let total: u64 = payload
        .partitions()
        .iter()
        .filter(|p| targets.iter().any(|t| t.partition == p.partition_name))
        .map(image_size)
        .sum();
    let mut done = 0u64;
    let mut written = Vec::new();
    for update in payload.partitions() {
        let Some(target) = targets.iter().find(|t| t.partition == update.partition_name) else {
            continue;
        };
        let size = image_size(update);
        let device = target.device.display().to_string();
        make_writable(&target.device);
        write_one(payload, package, base, update, &target.device, size, &mut done, total, progress)
            .with_context(|| format!("write {} to {device}", update.partition_name))?;
        written.push(update.partition_name.clone());
    }
    Ok(written)
}

/// Check every operation of `payload` without writing anything: its data
/// has to lie inside the payload, hash to what the manifest says, and its
/// stretches have to lie inside the image the manifest sizes. Says nothing
/// of the phone: what it checks is the package, and an install that would
/// write it.
pub fn check(payload: &Payload, package: &File, base: u64) -> Result<()> {
    let data_offset = payload.data_offset();
    let data_area = payload.size().checked_sub(data_offset).context("the payload's data begins past its end")?;
    let block = payload.block_size() as u64;
    let mut source = package.try_clone().context("share the package's handle")?;
    for update in payload.partitions() {
        let size = image_size(update);
        for op in operations(update) {
            let kind = OperationType::try_from(op.r#type).with_context(|| format!("operation kind {}", op.r#type))?;
            match kind {
                OperationType::Replace | OperationType::ReplaceBz | OperationType::ReplaceXz | OperationType::ReplaceZstd => {
                    operation_data(&mut source, base, data_offset, data_area, op)?;
                }
                OperationType::Zero | OperationType::Discard => {}
                other => bail!("{} is made by {other:?}, which a whole image does not use", update.partition_name),
            }
            for extent in &op.dst_extents {
                let start = extent.start_block.context("an extent with no start block")? * block;
                let take = extent.num_blocks.context("an extent with no block count")? * block;
                ensure!(
                    start.checked_add(take).is_some_and(|end| size == 0 || end <= size),
                    "{} lays {take} bytes out at {start}, past its image of {size}",
                    update.partition_name
                );
            }
        }
    }
    Ok(())
}

/// `BLKROSET`: marks a block device read-only, or writable again when what
/// follows the request says zero. The request is a plain number the kernel
/// takes as an unsigned long, but the C library of each target names its
/// type differently.
#[cfg(target_os = "android")]
const BLKROSET: libc::c_int = 0x125d;
#[cfg(not(target_os = "android"))]
const BLKROSET: libc::c_ulong = 0x125d;

/// Take the read-only flag off a partition's device, where the kernel holds
/// one.
///
/// A partition whose contents the running system was told not to change is
/// marked read-only, and the mark outlives the system that made it: a
/// partition firmware was written to by a package that set it so stays
/// read-only from then on, and every write into it is refused before the
/// bytes reach the flash. Flashing it again is what this is for, so the mark
/// is taken off first.
fn make_writable(device: &Path) {
    use std::os::fd::AsRawFd;
    let Ok(file) = OpenOptions::new().write(true).open(device) else { return };
    let zero: libc::c_int = 0;
    unsafe { libc::ioctl(file.as_raw_fd(), BLKROSET, &zero) };
}

/// Write `update`'s image into `device`, which holds `size` bytes of it.
#[allow(clippy::too_many_arguments)]
fn write_one(
    payload: &Payload,
    package: &File,
    base: u64,
    update: &PartitionUpdate,
    device: &Path,
    size: u64,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .with_context(|| format!("open {}", device.display()))?;
    let holds = file.seek(SeekFrom::End(0)).with_context(|| format!("find the size of {}", device.display()))?;
    ensure!(size <= holds, "{} holds {holds} bytes, and the image is {size}", device.display());
    file.seek(SeekFrom::Start(0)).with_context(|| format!("rewind {}", device.display()))?;

    let data_offset = payload.data_offset();
    let data_area = payload.size().checked_sub(data_offset).context("the payload's data begins past its end")?;
    let block = payload.block_size() as u64;
    let mut source = package.try_clone().context("share the package's handle")?;
    for op in operations(update) {
        let kind = OperationType::try_from(op.r#type).with_context(|| format!("operation kind {}", op.r#type))?;
        match kind {
            OperationType::Replace | OperationType::ReplaceBz | OperationType::ReplaceXz | OperationType::ReplaceZstd => {
                let data = operation_data(&mut source, base, data_offset, data_area, op)?;
                let wrote = match kind {
                    OperationType::Replace => {
                        let mut from = Cursor::new(data.as_slice());
                        write_extents(&mut from, &mut file, &op.dst_extents, block, holds, &update.partition_name, done, total, progress)
                    }
                    OperationType::ReplaceBz => {
                        let mut from = bzip2::bufread::BzDecoder::new(BufReader::new(Cursor::new(data.as_slice())));
                        write_extents(&mut from, &mut file, &op.dst_extents, block, holds, &update.partition_name, done, total, progress)
                    }
                    OperationType::ReplaceXz => {
                        let mut from = liblzma::bufread::XzDecoder::new(BufReader::new(Cursor::new(data.as_slice())));
                        write_extents(&mut from, &mut file, &op.dst_extents, block, holds, &update.partition_name, done, total, progress)
                    }
                    OperationType::ReplaceZstd => {
                        let mut from = zstd::stream::read::Decoder::new(Cursor::new(data.as_slice())).context("read the compressed operation")?;
                        write_extents(&mut from, &mut file, &op.dst_extents, block, holds, &update.partition_name, done, total, progress)
                    }
                    _ => unreachable!("the operation kinds are matched above"),
                };
                wrote.with_context(|| format!("write {} at the operation's stretches", device.display()))?;
            }
            // A whole image has nothing there to copy from and nothing to
            // keep: the stretch is written as nothing.
            OperationType::Zero | OperationType::Discard => {
                write_zeroes(&mut file, &op.dst_extents, block, holds, &update.partition_name, done, total, progress)?;
            }
            other => bail!("{} is made by {other:?}, which a whole image does not use", update.partition_name),
        }
    }
    file.sync_all().with_context(|| format!("flush {}", device.display()))?;
    Ok(())
}

/// The data of `op`, read where the payload says it is: `base` is where the
/// payload starts in `package`, `data_offset` where its operation data
/// starts in the payload, and `data_area` how much data it has.
///
/// The bytes are checked against the hash the manifest gives before they are
/// handed back, so an offset that points somewhere else stops the install
/// instead of writing another operation's data over a partition.
fn operation_data(package: &mut File, base: u64, data_offset: u64, data_area: u64, op: &InstallOperation) -> Result<Vec<u8>> {
    let at = op.data_offset.context("an operation with no data offset")?;
    let len = op.data_length.context("an operation with no data length")?;
    ensure!(len <= MOST_OPERATION_DATA, "an operation names {len} bytes of data, more than this reads at once");
    ensure!(
        at.checked_add(len).is_some_and(|end| end <= data_area),
        "an operation's data ({at} + {len}) runs past the payload's {data_area} bytes of data"
    );
    let start = base
        .checked_add(data_offset)
        .and_then(|data| data.checked_add(at))
        .context("the operation's data begins past the end of what can be read")?;
    let mut data = vec![0u8; len as usize];
    package.seek(SeekFrom::Start(start)).with_context(|| format!("seek to the operation's data at {start}"))?;
    package.read_exact(&mut data).context("read the operation's data")?;
    if let Some(want) = &op.data_sha256_hash {
        let got = Sha256::digest(&data);
        ensure!(
            got.as_slice() == want.as_slice(),
            "the operation's data does not hash to what the payload says: {} instead of {}",
            hex(&got),
            hex(want)
        );
    }
    Ok(data)
}

/// Write what `from` gives into the stretches `extents` lay out in `out`,
/// and nothing more: every stretch is checked against the partition's
/// `holds` bytes before a byte is written, and the data has to give exactly
/// the bytes the stretches name.
#[allow(clippy::too_many_arguments)]
fn write_extents(
    from: &mut impl Read,
    out: &mut File,
    extents: &[Extent],
    block: u64,
    holds: u64,
    partition: &str,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let mut named = 0u64;
    for extent in extents {
        let start = extent.start_block.context("an extent with no start block")? * block;
        let take = extent.num_blocks.context("an extent with no block count")? * block;
        ensure!(
            start.checked_add(take).is_some_and(|end| end <= holds),
            "{partition} lays {take} bytes out at {start}, past the partition's {holds}"
        );
        named = named.checked_add(take).context("an operation lays out more bytes than can be counted")?;
    }
    let mut buf = vec![0u8; CHUNK];
    let mut left = named;
    for extent in extents {
        let start = extent.start_block.context("an extent with no start block")? * block;
        let mut take = extent.num_blocks.context("an extent with no block count")? * block;
        out.seek(SeekFrom::Start(start)).with_context(|| format!("seek in {partition} at {start}"))?;
        while take > 0 {
            let want = take.min(buf.len() as u64) as usize;
            let n = from.read(&mut buf[..want]).context("read the operation's data")?;
            ensure!(n > 0, "{partition} names {named} bytes, and its data ends {left} bytes early");
            out.write_all(&buf[..n]).with_context(|| format!("write {partition}"))?;
            take -= n as u64;
            left -= n as u64;
            *done += n as u64;
            progress(*done, total);
        }
    }
    let mut extra = [0u8; 1];
    ensure!(
        from.read(&mut extra).context("read the operation's end")? == 0,
        "{partition} names {named} bytes, and its data holds more"
    );
    Ok(())
}

/// Write the stretches `extents` lay out in `out` as nothing, after checking
/// each against the partition's `holds` bytes.
#[allow(clippy::too_many_arguments)]
fn write_zeroes(
    out: &mut File,
    extents: &[Extent],
    block: u64,
    holds: u64,
    partition: &str,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let zeros = vec![0u8; CHUNK];
    for extent in extents {
        let start = extent.start_block.context("an extent with no start block")? * block;
        let mut take = extent.num_blocks.context("an extent with no block count")? * block;
        ensure!(
            start.checked_add(take).is_some_and(|end| end <= holds),
            "{partition} lays {take} bytes out at {start}, past the partition's {holds}"
        );
        out.seek(SeekFrom::Start(start)).with_context(|| format!("seek in {partition} at {start}"))?;
        while take > 0 {
            let n = take.min(zeros.len() as u64) as usize;
            out.write_all(&zeros[..n]).with_context(|| format!("write {partition}"))?;
            take -= n as u64;
            *done += n as u64;
            progress(*done, total);
        }
    }
    Ok(())
}

/// A short lowercase hex form of `bytes`, for saying what differs.
fn hex(bytes: &[u8]) -> String {
    let mut said = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(said, "{byte:02x}");
    }
    said
}

/// The partitions of `targets` that `slot` names on the phone: each
/// partition's device is the phone's, with the slot's suffix.
pub fn for_slot(dir: &Path, partition: &str, slot: Slot) -> PathBuf {
    dir.join(format!("{partition}{}", slot.suffix()))
}

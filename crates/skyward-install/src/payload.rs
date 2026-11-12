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

use anyhow::{bail, ensure, Context, Result};
use skyward_vab::Slot;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use unpayload::metadata::{InstallOperation, OperationType, PartitionUpdate};
use unpayload::Payload;

/// How much of an operation's data is read at a time.
const CHUNK: usize = 1 << 20;

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

    let block = payload.block_size() as u64;
    for op in operations(update) {
        let kind = OperationType::try_from(op.r#type).with_context(|| format!("operation kind {}", op.r#type))?;
        match kind {
            OperationType::Replace | OperationType::ReplaceBz | OperationType::ReplaceXz | OperationType::ReplaceZstd => {
                let at = op.data_offset.context("an operation with no data offset")?;
                let len = op.data_length.context("an operation with no data length")?;
                let data = (base + payload.data_offset() + at, len);
                for extent in &op.dst_extents {
                    let start = extent.start_block.context("an extent with no start block")? * block;
                    let take = extent.num_blocks.context("an extent with no block count")? * block;
                    file.seek(SeekFrom::Start(start)).with_context(|| format!("seek in {}", device.display()))?;
                    let wrote = write_stretch(package, data, kind, &mut file, take, done, total, progress)
                        .with_context(|| format!("write {} at {start}", device.display()))?;
                    ensure!(wrote == take, "{} takes {wrote} bytes where {take} were laid out", update.partition_name);
                }
            }
            // A whole image has nothing there to copy from and nothing to
            // keep: the stretch is written as nothing.
            OperationType::Zero | OperationType::Discard => {
                for extent in &op.dst_extents {
                    let start = extent.start_block.context("an extent with no start block")? * block;
                    let take = extent.num_blocks.context("an extent with no block count")? * block;
                    file.seek(SeekFrom::Start(start)).with_context(|| format!("seek in {}", device.display()))?;
                    let zeros = vec![0u8; CHUNK.min(take as usize)];
                    let mut left = take;
                    while left > 0 {
                        let n = left.min(zeros.len() as u64) as usize;
                        file.write_all(&zeros[..n]).with_context(|| format!("write {}", device.display()))?;
                        left -= n as u64;
                        *done += n as u64;
                        progress(*done, total);
                    }
                }
            }
            other => bail!("{} is made by {other:?}, which a whole image does not use", update.partition_name),
        }
    }
    file.sync_all().with_context(|| format!("flush {}", device.display()))?;
    Ok(())
}

/// Write `take` bytes made from the operation data at `data`, which is
/// `len` bytes of the package at `offset` in the payload, into `out`.
#[allow(clippy::too_many_arguments)]
fn write_stretch(
    package: &File,
    data: (u64, u64),
    kind: OperationType,
    out: &mut File,
    take: u64,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    match kind {
        OperationType::Replace => {
            let mut reader = PayloadReader::new(package, data)?;
            copy_out(&mut reader, out, take, done, total, progress)
        }
        OperationType::ReplaceBz => {
            let reader = PayloadReader::new(package, data)?;
            let mut decoder = bzip2::bufread::BzDecoder::new(BufReader::new(reader));
            copy_out(&mut decoder, out, take, done, total, progress)
        }
        OperationType::ReplaceXz => {
            let reader = PayloadReader::new(package, data)?;
            let mut decoder = liblzma::bufread::XzDecoder::new(BufReader::new(reader));
            copy_out(&mut decoder, out, take, done, total, progress)
        }
        OperationType::ReplaceZstd => {
            let reader = PayloadReader::new(package, data)?;
            let mut decoder = zstd::stream::read::Decoder::new(BufReader::new(reader)).context("read the compressed operation")?;
            copy_out(&mut decoder, out, take, done, total, progress)
        }
        other => bail!("{other:?} is not an operation this writes as it stands"),
    }
}

/// Copy up to `take` bytes of `from` into `out`, and give how many there
/// were.
fn copy_out(
    from: &mut impl Read,
    out: &mut File,
    take: u64,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    let mut buf = vec![0u8; CHUNK];
    let mut wrote = 0u64;
    while wrote < take {
        let want = (take - wrote).min(buf.len() as u64) as usize;
        let n = from.read(&mut buf[..want]).context("read the operation's data")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("write the partition")?;
        wrote += n as u64;
        *done += n as u64;
        progress(*done, total);
    }
    Ok(wrote)
}

/// The bytes of one operation's data, read out of the payload a stretch at a
/// time.
struct PayloadReader {
    file: File,
    at: u64,
    left: u64,
}

impl PayloadReader {
    /// The data at `data` of the payload in `file`, a handle of its own on
    /// the package.
    fn new(file: &File, data: (u64, u64)) -> Result<PayloadReader> {
        Ok(PayloadReader { file: file.try_clone().context("share the package's handle")?, at: data.0, left: data.1 })
    }
}

impl Read for PayloadReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.left) as usize;
        self.file.seek(SeekFrom::Start(self.at))?;
        let n = self.file.read(&mut buf[..want])?;
        self.at += n as u64;
        self.left -= n as u64;
        Ok(n)
    }
}

/// The partitions of `targets` that `slot` names on the phone: each
/// partition's device is the phone's, with the slot's suffix.
pub fn for_slot(dir: &Path, partition: &str, slot: Slot) -> PathBuf {
    dir.join(format!("{partition}{}", slot.suffix()))
}

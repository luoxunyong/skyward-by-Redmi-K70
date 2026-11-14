use crate::snapshots::{self, Merge, SnapshotStatus, Snapshots};
use crate::{snapshot_state, MergeStatus, Slot, SnapshotState, METADATA};
use anyhow::{ensure, Context, Result};
use skyward_block::{devnum, dm, mount, BY_NAME};
use skyward_lp::{Extent, Metadata, Partition, ATTR_DISABLED};
use std::fs;
use std::path::{Path, PathBuf};

/// A logical partition of super, mapped and mounted read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    /// The name in super, such as vendor_dlkm_b; also the device-mapper name.
    pub partition: String,
    pub device: PathBuf,
    pub mount_point: PathBuf,
    pub filesystem: String,
}

#[derive(Debug, Clone, Default)]
pub struct Mounts {
    pub partitions: Vec<Mounted>,
    /// Damaged metadata copies in super that their backups stood in for.
    pub damaged: Vec<String>,
}

/// Where a logical partition goes in recovery. System goes to /mnt/system,
/// since /system holds recovery itself; a partition whose mount point
/// directory is in the ramdisk root uses it, as /vendor or /odm; any other
/// goes under /mnt.
pub fn mount_point(base: &str) -> PathBuf {
    let root = Path::new("/").join(base);
    if base != "system" && fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
        root
    } else {
        Path::new("/mnt").join(base)
    }
}

fn super_device() -> PathBuf {
    Path::new(BY_NAME).join("super")
}

/// The device numbers of super and its extra block devices, in the order
/// the metadata lists them, so an extent's device index resolves to one.
fn block_devices(md: &Metadata, super_dev: &Path) -> Result<Vec<(u32, u32)>> {
    md.block_devices
        .iter()
        .enumerate()
        .map(|(i, d)| if i == 0 { devnum(super_dev) } else { devnum(Path::new(BY_NAME).join(&d.partition_name)) })
        .collect()
}

/// The device-mapper targets that lay partition `p` out over super.
pub(crate) fn targets(p: &Partition, devices: &[(u32, u32)]) -> Vec<dm::Target> {
    p.extents
        .iter()
        .map(|e| match *e {
            Extent::Linear { sectors, device, sector } => dm::Target::Linear { sectors, device: devices[device], offset: sector },
            Extent::Zero { sectors } => dm::Target::Zero { sectors },
        })
        .collect()
}

/// How many bytes partition `p` holds.
fn bytes(p: &Partition) -> u64 {
    p.sectors() * 512
}

/// Map partition `p` from super read-only with device-mapper and mount it
/// over its point. A device that does not mount is removed again.
fn map_and_mount(p: &Partition, devices: &[(u32, u32)], slot: Slot) -> Result<Mounted> {
    let device = dm::create(&p.name, &targets(p, devices), true)?;
    let mount_point = mount_point(base(p, slot));
    let mounted = (|| -> Result<String> {
        fs::create_dir_all(&mount_point).with_context(|| format!("create {}", mount_point.display()))?;
        Ok(mount::mount_ro_over(&device, &mount_point).with_context(|| format!("mount {}", p.name))?.to_string())
    })();
    match mounted {
        Ok(filesystem) => Ok(Mounted { partition: p.name.clone(), device, mount_point, filesystem }),
        Err(e) => Err(match dm::remove(&p.name) {
            Ok(()) => e,
            Err(r) => e.context(format!("removing {} afterwards failed too: {r:#}", p.name)),
        }),
    }
}

/// The slot's partitions that hold data. Copy-on-write partitions such as
/// odm_b-cow end in -cow and stay out.
fn slot_partitions(md: &Metadata, slot: Slot) -> Vec<&Partition> {
    md.partitions
        .iter()
        .filter(|p| p.name.ends_with(slot.suffix()) && !p.extents.is_empty() && p.attributes & ATTR_DISABLED == 0)
        .collect()
}

fn base(p: &Partition, slot: Slot) -> &str {
    &p.name[..p.name.len() - slot.suffix().len()]
}

/// Partitions are mapped only while no update snapshot is outstanding,
/// since a snapshotted partition reads correctly only through its snapshot.
fn ensure_no_snapshot() -> Result<()> {
    let state = snapshot_state()?;
    ensure!(state == SnapshotState::None, "an update snapshot is outstanding ({state:?})");
    Ok(())
}

/// Mount /metadata, then map the slot's logical partitions from super and
/// mount them, all read-only. Where an update went to the slot and is not
/// merged yet, each partition it has a snapshot of is mapped through that
/// snapshot, as the system's own first stage maps it, merging where the
/// merge is under way. A partition an earlier run of the recovery service
/// mounted, and that still shows, is taken as it is.
pub fn mount_partitions(slot: Slot) -> Result<Mounts> {
    let metadata = Path::new(METADATA);
    if !mount::is_mount_point(metadata)? {
        mount::mount_ro(&Path::new(BY_NAME).join("metadata"), metadata)?;
    }
    let super_dev = super_device();
    let md = skyward_lp::read(&super_dev, slot.number())?;
    let devices = block_devices(&md, &super_dev)?;
    let snaps = Snapshots::lock(true)?;
    let through = match &snaps {
        Some(s) if s.needed(slot)? => Some(s),
        _ => None,
    };

    let shown = mounted_partitions(slot)?;
    let mut mounts = Mounts { partitions: Vec::new(), damaged: md.damaged.clone() };
    for p in slot_partitions(&md, slot) {
        if let Some(m) = shown.iter().find(|m| m.partition == p.name) {
            mounts.partitions.push(m.clone());
            continue;
        }
        let mounted = match through.map(|s| Ok::<_, anyhow::Error>((s, s.live(p)?))).transpose()? {
            Some((s, Some(status))) => mount_snapshot(s, &md, &devices, p, &status, slot)?,
            _ => map_and_mount(p, &devices, slot)?,
        };
        mounts.partitions.push(mounted);
    }
    Ok(mounts)
}

/// Start merging the update into `slot`, the slot it went to and the phone
/// runs from, its partitions mapped through their snapshots; see
/// [`snapshots::initiate`]. `tell` tells the bootloader where the update
/// stands.
pub fn start_merge(slot: Slot, tell: &dyn Fn(MergeStatus) -> Result<()>) -> Result<()> {
    let md = skyward_lp::read(&super_device(), slot.number())?;
    let snaps = Snapshots::lock(true)?.context("no update was ever applied")?;
    snapshots::initiate(&snaps, slot, &md, tell)
}

/// Whether every snapshot the slot reads through is mapped as a kernel
/// snapshot, as following a merge needs; `false` where it reads through
/// none.
pub fn snapshots_mapped(slot: Slot) -> Result<bool> {
    let md = skyward_lp::read(&super_device(), slot.number())?;
    let Some(snaps) = Snapshots::lock(false)? else { return Ok(false) };
    if !snaps.needed(slot)? {
        return Ok(false);
    }
    let mut any = false;
    for p in slot_partitions(&md, slot) {
        if snaps.live(p)?.is_some() {
            if !snapshots::is_snapshot(&p.name)? {
                return Ok(false);
            }
            any = true;
        }
    }
    Ok(any)
}

/// Look at the merge into `slot`, cleaning up after each snapshot merged;
/// see [`snapshots::check`].
pub fn check_merge(slot: Slot, tell: &dyn Fn(MergeStatus) -> Result<()>) -> Result<Merge> {
    let super_dev = super_device();
    let md = skyward_lp::read(&super_dev, slot.number())?;
    let devices = block_devices(&md, &super_dev)?;
    let snaps = Snapshots::lock(true)?.context("no update was ever applied")?;
    snapshots::check(&snaps, slot, &md, &devices, tell)
}

/// Map partition `p` through its snapshot `status` and mount it read-only;
/// a snapshot that does not mount is taken apart again.
fn mount_snapshot(snaps: &Snapshots, md: &Metadata, devices: &[(u32, u32)], p: &Partition, status: &SnapshotStatus, slot: Slot) -> Result<Mounted> {
    let device = snapshots::map_snapshot(snaps, md, devices, p, status)?;
    mount_mapped(p, device, slot)
}

/// Mount partition `p`, mapped as its snapshot at `device`, read-only over
/// its point; the snapshot is taken apart again if it does not mount.
fn mount_mapped(p: &Partition, device: PathBuf, slot: Slot) -> Result<Mounted> {
    let mount_point = mount_point(base(p, slot));
    let mounted = (|| -> Result<String> {
        fs::create_dir_all(&mount_point).with_context(|| format!("create {}", mount_point.display()))?;
        Ok(mount::mount_ro_over(&device, &mount_point).with_context(|| format!("mount {}", p.name))?.to_string())
    })();
    match mounted {
        Ok(filesystem) => Ok(Mounted { partition: p.name.clone(), device, mount_point, filesystem }),
        Err(e) => Err(match snapshots::unmap_snapshot(&p.name) {
            Ok(()) => e,
            Err(r) => e.context(format!("taking {} apart afterwards failed too: {r:#}", p.name)),
        }),
    }
}

/// The base names of the slot's logical partitions (such as `system`,
/// `vendor`), in the order super lists them, whether or not they are mounted.
pub fn slot_bases(slot: Slot) -> Result<Vec<String>> {
    let md = skyward_lp::read(&super_device(), slot.number())?;
    Ok(slot_partitions(&md, slot).into_iter().map(|p| base(p, slot).to_string()).collect())
}

/// The slot's logical partitions by base name with the bytes each holds, in
/// the order super lists them.
pub fn slot_sizes(slot: Slot) -> Result<Vec<(String, u64)>> {
    let md = skyward_lp::read(&super_device(), slot.number())?;
    Ok(slot_partitions(&md, slot).into_iter().map(|p| (base(p, slot).to_string(), bytes(p))).collect())
}

/// One of the slot's logical partitions mapped whole under a name of its
/// own, beside the mapping the recovery mounts, for a backup to read it or
/// a restore to write it.
#[derive(Debug)]
pub struct Whole {
    pub name: String,
    pub device: PathBuf,
    pub size: u64,
}

/// Map the slot's logical partition `base_name` whole as `name`, read-only
/// or not, while no update snapshot is outstanding.
pub fn map_whole(slot: Slot, base_name: &str, name: &str, readonly: bool) -> Result<Whole> {
    ensure_no_snapshot()?;
    let super_dev = super_device();
    let md = skyward_lp::read(&super_dev, slot.number())?;
    let devices = block_devices(&md, &super_dev)?;
    let full = format!("{base_name}{}", slot.suffix());
    let p = slot_partitions(&md, slot).into_iter().find(|p| p.name == full).with_context(|| format!("super holds no {full}"))?;
    let device = dm::create(name, &targets(p, &devices), readonly)?;
    Ok(Whole { name: name.into(), device, size: bytes(p) })
}

/// Remove a mapping [`map_whole`] made.
pub fn unmap_whole(whole: &Whole) -> Result<()> {
    dm::remove(&whole.name)
}

/// What [`map_slot`] did to the slot's logical partitions, for
/// [`unmap_slot`] to undo: those it mapped, and those the recovery had
/// mapped read-only already, and still uses, that it made writable.
#[derive(Debug, Default)]
pub struct SlotMaps {
    made: Vec<String>,
    opened: Vec<(String, Vec<dm::Target>)>,
}

/// Map each of the slot's logical partitions writable and under its own
/// name, as recoveries map them for install scripts, which look for them
/// at /dev/block/mapper/<name>; while no update snapshot is outstanding.
/// One the recovery has mapped read-only already is made writable where
/// it is, in use or not.
pub fn map_slot(slot: Slot) -> Result<SlotMaps> {
    ensure_no_snapshot()?;
    let super_dev = super_device();
    let md = skyward_lp::read(&super_dev, slot.number())?;
    let devices = block_devices(&md, &super_dev)?;
    let mut maps = SlotMaps::default();
    for p in slot_partitions(&md, slot) {
        if let Err(e) = map_writable(p, targets(p, &devices), &mut maps) {
            return Err(match unmap_slot(&maps) {
                Ok(()) => e,
                Err(r) => e.context(format!("undoing what was mapped before failed too: {r:#}")),
            });
        }
    }
    Ok(maps)
}

/// Map partition `p` writable with `table`, or make the read-only mapping
/// of it writable, keeping in `maps` what was done.
fn map_writable(p: &Partition, table: Vec<dm::Target>, maps: &mut SlotMaps) -> Result<()> {
    if dm::status(&p.name)?.is_none() {
        dm::create(&p.name, &table, false)?;
        maps.made.push(p.name.clone());
    } else if dm::readonly(&p.name)? {
        dm::reload(&p.name, &table, false)?;
        maps.opened.push((p.name.clone(), table));
    }
    Ok(())
}

/// Undo [`map_slot`]: remove the mappings it made, and make those it made
/// writable read-only again.
pub fn unmap_slot(maps: &SlotMaps) -> Result<()> {
    for name in &maps.made {
        dm::remove(name)?;
    }
    for (name, table) in &maps.opened {
        dm::reload(name, table, true)?;
    }
    Ok(())
}

/// Map and mount one of the slot's logical partitions read-only by its base
/// name (such as `system` or `vendor`): through its snapshot where the slot
/// reads through an update's snapshots, from the snapshot already mapped
/// where it was only unmounted.
pub fn mount_one(slot: Slot, base_name: &str) -> Result<Mounted> {
    let super_dev = super_device();
    let md = skyward_lp::read(&super_dev, slot.number())?;
    let devices = block_devices(&md, &super_dev)?;
    let name = format!("{base_name}{}", slot.suffix());
    let p = slot_partitions(&md, slot)
        .into_iter()
        .find(|p| p.name == name)
        .with_context(|| format!("super holds no {name}"))?;
    let snaps = Snapshots::lock(true)?;
    let status = match &snaps {
        Some(s) if s.needed(slot)? => s.live(p)?.map(|status| (s, status)),
        _ => None,
    };
    match status {
        Some(_) if dm::status(&name)?.is_some() => mount_mapped(p, Path::new("/dev/block").join(dm_node(&name)?), slot),
        Some((s, status)) => mount_snapshot(s, &md, &devices, p, &status, slot),
        None => map_and_mount(p, &devices, slot),
    }
}

/// The node device-mapper device `name` has under /dev/block, as dm-N.
fn dm_node(name: &str) -> Result<String> {
    let (_, minor) = dm::status(name)?.with_context(|| format!("{name} is not mapped"))?;
    Ok(format!("dm-{minor}"))
}

/// Unmount the partition mapped as `name` where its own device is what
/// shows on `point`, and take it apart: its device-mapper device, and where
/// it reads through a snapshot, the snapshot's own devices too. A partition
/// that is not mapped is left as it is.
fn unmap(name: &str, point: &Path) -> Result<()> {
    let Some(dev) = dm::status(name)? else { return Ok(()) };
    if mount::visible(point)?.is_some_and(|v| v.device == dev) {
        mount::umount_over(point)?;
    }
    snapshots::unmap_snapshot(name)
}

/// Unmount one of the slot's logical partitions by its base name and remove
/// its device-mapper device. One that reads through a snapshot is only
/// unmounted: its snapshot stays mapped, for its merge to go on and for it
/// to be mounted again.
pub fn unmount_one(slot: Slot, base_name: &str) -> Result<()> {
    let name = format!("{base_name}{}", slot.suffix());
    let point = mount_point(base_name);
    if !snapshots::is_snapshot(&name)? {
        return unmap(&name, &point);
    }
    let dev = dm::status(&name)?.with_context(|| format!("{name} is not mapped"))?;
    if mount::visible(&point)?.is_some_and(|v| v.device == dev) {
        mount::umount_over(&point)?;
    }
    Ok(())
}

/// The slot's partitions that are mapped and show on their mount points, in
/// the order they were mapped.
pub fn mounted_partitions(slot: Slot) -> Result<Vec<Mounted>> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/sys/block").context("read /sys/block")? {
        let entry = entry?;
        let node = entry.file_name().to_string_lossy().into_owned();
        let Some(minor) = node.strip_prefix("dm-") else { continue };
        let name_file = entry.path().join("dm/name");
        let name = fs::read_to_string(&name_file).with_context(|| format!("read {}", name_file.display()))?;
        let name = name.trim();
        let Some(base) = name.strip_suffix(slot.suffix()) else { continue };
        let device = Path::new("/dev/block").join(&node);
        let mount_point = mount_point(base);
        let Some(shown) = mount::visible(&mount_point)? else { continue };
        if shown.device != devnum(&device)? {
            continue;
        }
        let m = Mounted { partition: name.to_string(), device, mount_point, filesystem: shown.filesystem };
        found.push((minor.parse::<u32>()?, m));
    }
    found.sort_by_key(|(minor, _)| *minor);
    Ok(found.into_iter().map(|(_, m)| m).collect())
}

/// Take the slot's mounted partitions away before an update writes over
/// them: unmount them and remove their device-mapper devices. A partition
/// whose mount point is in `busy` still has files in use; it only leaves
/// the tree, and those files and its device stay until let go. /metadata
/// stays mounted for the update.
pub fn release_partitions(slot: Slot, busy: &[PathBuf]) -> Result<()> {
    for m in mounted_partitions(slot)?.into_iter().rev() {
        // A partition that will not come off is not the end of the move: what
        // can be let go is, and the rest is told of and left. Stopping at the
        // first mount that refuses would leave every partition after it
        // standing too, and the write that follows would find them all.
        if busy.contains(&m.mount_point) {
            if let Err(e) = mount::umount_detach_over(&m.mount_point) {
                eprintln!("install: {} stays mounted: {e:#}", m.mount_point.display());
            }
        } else if let Err(e) = mount::umount_over(&m.mount_point) {
            eprintln!("install: {} stays mounted: {e:#}", m.mount_point.display());
        } else if let Err(e) = snapshots::unmap_snapshot(&m.partition) {
            eprintln!("install: {} stays mapped: {e:#}", m.partition);
        }
    }
    Ok(())
}

/// Undo `mount_partitions`: unmount the slot's partitions where their own
/// device is what shows, remove their device-mapper devices, and unmount
/// /metadata.
pub fn unmount_partitions(slot: Slot) -> Result<()> {
    let md = skyward_lp::read(&super_device(), slot.number())?;
    for p in slot_partitions(&md, slot).into_iter().rev() {
        unmap(&p.name, &mount_point(base(p, slot)))?;
    }
    let metadata = Path::new(METADATA);
    let metadata_dev = devnum(Path::new(BY_NAME).join("metadata"))?;
    if mount::visible(metadata)?.is_some_and(|v| v.device == metadata_dev) {
        mount::umount(metadata)?;
    }
    Ok(())
}

/// Let go of the slot's partitions for a package that lays super out
/// again: every program still running from them is ended first, then each
/// is unmounted and its device-mapper device removed.
///
/// A partition cannot be unmounted while a program is running from it, and
/// the programs this recovery starts from `/vendor` — the trusted
/// environment's daemon and the key and boot services — run from there for
/// as long as the recovery does. What they serve is needed to read the
/// installed system, not to write a new one, so they are ended here and
/// the layout is free to be rewritten.
///
/// The pages pinned for those services are mapped from the same
/// partitions, and a mapping of this recovery's own keeps the mount, and
/// the device-mapper device under it, standing even after the mount is
/// detached. The services the pages were kept for are ended above, so the
/// mappings are let go before the devices are asked to.
pub fn release_whole(slot: Slot) -> Result<()> {
    let md = skyward_lp::read(&super_device(), slot.number())?;
    // The bundles the vendor interface is read through are whole images
    // taken from the slot and mounted over loop devices, and a loop device
    // on a file of a partition keeps that partition from being unmounted
    // just as a program running from it does. They are let go first, then
    // the programs, then the partitions themselves.
    release_bundles();
    let mut ended = false;
    let mut points = Vec::new();
    for p in slot_partitions(&md, slot) {
        let point = mount_point(base(p, slot));
        let dev = skyward_block::dm::status(&p.name).ok().flatten();
        ended |= end_running_from(&point, dev);
        points.push(point);
    }
    if ended {
        // The programs ended above are let go by the kernel as init reaps
        // them; the devices they held are free once that is done.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    release_own_mappings(&points);
    // Every mount on those partitions comes off detached, so a program that
    // the kernel has not yet let go of does not keep the mount, nor the
    // device-mapper device under it, standing.
    //
    // A mount that will not come off is not the end of it: the package
    // rewrites the layout itself, and a partition left standing is one the
    // package's own unmapping will meet, rather than the whole install
    // stopping here. The same goes for a device-mapper device that will not
    // go: what can be let go is, and the rest is told of and left.
    for p in slot_partitions(&md, slot).into_iter().rev() {
        let point = mount_point(base(p, slot));
        if mount::is_mount_point(&point).unwrap_or(false) {
            if let Err(e) = mount::umount_detach(&point) {
                let _ = skyward_sys::kmsg(&format!(
                    "install: {} stays mounted, and the package goes on without it: {e:#}",
                    point.display()
                ));
            }
        }
        if let Err(e) = unmap_for_good(&p.name, &point) {
            // A device something still holds open is taken away by the
            // kernel once that holder lets go, which is what the deferral
            // asks for: the package's own unmapping then finds it gone.
            match skyward_block::dm::remove_deferred(&p.name) {
                Ok(()) => {
                    let mut gone = false;
                    for _ in 0..100 {
                        if skyward_block::dm::status(&p.name).ok().flatten().is_none() {
                            gone = true;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    let _ = skyward_sys::kmsg(&format!(
                        "install: device-mapper device {} {}",
                        p.name,
                        if gone { "was taken away as its last holder let go" } else { "is waiting for its last holder to let go" }
                    ));
                }
                Err(deferred) => {
                    let _ = skyward_sys::kmsg(&format!(
                        "install: device-mapper device {} stays, and the package goes on without it: {e:#}; deferred too: {deferred:#}",
                        p.name
                    ));
                }
            }
        }
    }
    let left: Vec<String> = slot_partitions(&md, slot)
        .into_iter()
        .filter(|p| skyward_block::dm::status(&p.name).ok().flatten().is_some())
        .map(|p| p.name.clone())
        .collect();
    if !left.is_empty() {
        let _ = skyward_sys::kmsg(&format!(
            "install: {} of the slot's device-mapper devices are still there for the package to meet: {}",
            left.len(),
            left.join(", ")
        ));
    }
    let metadata = Path::new(METADATA);
    if mount::is_mount_point(metadata).unwrap_or(false) {
        let _ = mount::umount_detach(metadata);
    }
    Ok(())
}

/// [`unmap`], tried again for a moment: a program that was just ended is
/// let go by the kernel as init reaps it, and a device it held is free
/// only after that.
fn unmap_for_good(name: &str, point: &Path) -> Result<()> {
    let mut last = match unmap(name, point) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        match unmap(name, point) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Let go of this recovery's own mappings of files under `points`, telling
/// how many were let go. Pinning a service's pages maps them from the
/// partition the service runs from, and the mapping outlives the service
/// unless it is dropped here.
fn release_own_mappings(points: &[PathBuf]) -> usize {
    let Ok(maps) = fs::read_to_string("/proc/self/maps") else { return 0 };
    let total = maps.lines().count();
    let mut taken = 0;
    for (start, end) in mappings_under(&maps, points) {
        if unsafe { libc::munmap(start as *mut libc::c_void, (end - start) as usize) } == 0 {
            taken += 1;
        }
    }
    let _ = skyward_sys::kmsg(&format!("install: {taken} of {total} mappings of this recovery are on the slot"));
    taken
}

/// The address ranges of an `/proc/self/maps` text that map files under
/// `points`.
fn mappings_under(maps: &str, points: &[PathBuf]) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some(path) = fields.nth(4) else { continue };
        let path = Path::new(path);
        if !path.is_absolute() || !points.iter().any(|p| path.starts_with(p)) {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else { continue };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16)) else { continue };
        if start < end {
            found.push((start, end));
        }
    }
    found
}

/// End every program whose executable, working directory, open files or
/// mappings lie under `point`, telling whether any was found. The programs
/// ended are let go by the kernel as init reaps them.
fn end_running_from(point: &Path, dev: Option<(u32, u32)>) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else { return false };
    let mut ended = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<libc::pid_t>().ok()) else { continue };
        // The recovery itself, and its own children, are not ended: what
        // runs them is what asks for this.
        if pid <= 1 || pid == unsafe { libc::getpid() } {
            continue;
        }
        let root = entry.path();
        if !runs_from(&root, point, dev) {
            continue;
        }
        ended = true;
        let _ = skyward_sys::kmsg(&format!("install: ending {pid}, which runs from {}", point.display()));
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    ended
}

/// Let go of every bundle the recovery mounted over a loop device: each is
/// unmounted from `/apex`, its loop device taken away, and the mount point
/// left empty. A loop device is what keeps the partition its image lies on
/// from being unmapped, and unmounting the bundle does not take it away.
fn release_bundles() {
    let Ok(entries) = std::fs::read_dir("/sys/block") else { return };
    let loops: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("loop").is_some().then_some(name)
        })
        .collect();
    for name in loops {
        let backing = Path::new("/sys/block").join(&name).join("loop/backing_file");
        let Ok(file) = std::fs::read_to_string(&backing) else { continue };
        let file = file.trim().trim_end_matches(" (deleted)").to_string();
        if file.is_empty() {
            continue;
        }
        // The bundle mounted over it, if it is one: its name is the file's
        // with `.apex` off, under any of the places bundles are mounted.
        let stem = Path::new(&file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        for point in [Path::new("/apex").join(&stem), Path::new("/apex").join(&file)] {
            if mount::is_mount_point(&point).unwrap_or(false) {
                let _ = skyward_sys::kmsg(&format!("install: letting go of the bundle at {}", point.display()));
                if let Err(e) = mount::umount_detach(&point) {
                    let _ = skyward_sys::kmsg(&format!("install: the bundle at {} stays mounted: {e:#}", point.display()));
                }
            }
        }
        let _ = skyward_sys::kmsg(&format!("install: taking away the loop device {name}, which holds {file}"));
        if let Err(e) = detach_loop(&name) {
            let _ = skyward_sys::kmsg(&format!("install: the loop device {name} stays: {e:#}"));
        }
    }
    // What was let go above is reaped by the kernel over a moment.
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// Take a loop device away: `LOOP_CLR_FD` on it, which lets go of the file
/// it was showing.
fn detach_loop(name: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let path = Path::new("/dev/block").join(name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if unsafe { libc::ioctl(file.as_raw_fd(), LOOP_CLR_FD) } == -1 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("take away the loop device {name}"));
    }
    Ok(())
}

/// `LOOP_CLR_FD`: lets go of the file a loop device was showing. The
/// request is a plain number the kernel takes as an unsigned long, but the
/// C library of each target names its type differently.
#[cfg(target_os = "android")]
const LOOP_CLR_FD: libc::c_int = 0x4C01;
#[cfg(not(target_os = "android"))]
const LOOP_CLR_FD: libc::c_ulong = 0x4C01;
/// Whether the process at `root`, a folder of `/proc`, stands on `point`:
/// it runs from there, works there, keeps a file open there, or has a
/// library or a file of it mapped. A process holds a partition by any of
/// these, and while one of them stands the partition cannot be unmounted
/// nor its device-mapper device taken away.
fn runs_from(root: &Path, point: &Path, dev: Option<(u32, u32)>) -> bool {
    let under = |p: &Path| p.starts_with(point);
    if std::fs::read_link(root.join("exe")).is_ok_and(|p| under(&p)) || std::fs::read_link(root.join("cwd")).is_ok_and(|p| under(&p)) {
        return true;
    }
    // The files it has open.
    if let Ok(fds) = std::fs::read_dir(root.join("fd")) {
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|p| under(&p)) {
                return true;
            }
        }
    }
    // What it has mapped: a library of the partition is enough to keep it.
    // The device the mapped file is on says which partition it is when the
    // path it was reached by does not: a service this recovery starts has
    // the partition as its root, so its paths read /bin/qseecomd while the
    // file behind them is the partition's own.
    let Ok(maps) = std::fs::read_to_string(root.join("maps")) else { return false };
    if maps.lines().any(|line| line.split_whitespace().nth(5).is_some_and(|p| under(Path::new(p)))) {
        return true;
    }
    dev.is_some_and(|dev| maps_on_device(&maps, dev))
}

/// Whether an `/proc/pid/maps` text maps a file on `dev`, the device the
/// kernel names there, such as `fe:05`.
fn maps_on_device(maps: &str, dev: (u32, u32)) -> bool {
    let want = format!("{:02x}:{:02x}", dev.0, dev.1);
    maps.lines().any(|line| line.split_whitespace().nth(3) == Some(want.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapping_on_a_partitions_device_is_told_by_its_device() {
        let maps = "\
5e412fa000-5e412fe000 r--p 00000000 fe:05 3003502 /bin/qseecomd
5b747030000-5b7470c000 r--p 00000000 fe:04 8395264 /bin/hwservicemanager
7c91e99000-7c92af2000 ---p 00000000 00:00 0";
        assert!(maps_on_device(maps, (254, 5)), "vendor_a");
        assert!(maps_on_device(maps, (254, 4)), "system_ext_a");
        assert!(!maps_on_device(maps, (254, 2)), "system_a");
    }

    #[test]
    fn mappings_of_the_slot_are_told_from_the_others() {
        let maps = "\
7f0000000000-7f0000001000 r--s 00000000 fd:01 123 /mnt/system/lib64/libc.so
7f0000001000-7f0000002000 r--s 00000000 fd:01 124 /mnt/system_ext/lib64/libfoo.so
7f0000002000-7f0000003000 r--s 00000000 fd:01 125 /vendor/lib64/libbar.so
7f0000003000-7f0000004000 r--s 00000000 fd:01 126 /mnt/product/lib64/libbaz.so
7f0000004000-7f0000005000 r-xp 00000000 00:00 0 /skyward/system-init
7f0000005000-7f0000006000 rw-p 00000000 00:00 0 [heap]";
        let points = vec![
            PathBuf::from("/mnt/system"),
            PathBuf::from("/mnt/system_ext"),
            PathBuf::from("/vendor"),
        ];
        assert_eq!(
            mappings_under(maps, &points),
            vec![
                (0x7f00_0000_0000, 0x7f00_0000_1000),
                (0x7f00_0000_1000, 0x7f00_0000_2000),
                (0x7f00_0000_2000, 0x7f00_0000_3000),
            ]
        );
    }
}

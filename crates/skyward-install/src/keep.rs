//! Keeping this recovery in the recovery partitions an install writes
//! over: this recovery as its partition holds it, and which slots get it
//! back after.

use crate::image;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use skyward_vab::Slot;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

const PARTITION: &str = "recovery";

/// What a slot's recovery partition holds: a digest of all of it, and
/// whether that is Skyward, any build of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub digest: [u8; 32],
    pub skyward: bool,
}

/// This recovery, held from the current slot's recovery partition before
/// an install.
pub struct Kept {
    dir: PathBuf,
    image: Vec<u8>,
    own: [u8; 32],
}

impl Kept {
    /// Hold this recovery from the recovery partition of the `current`
    /// slot, before an install; none when that partition does not hold
    /// Skyward, as when it was started without being flashed.
    pub fn hold(dir: &Path, current: Slot) -> Result<Option<Kept>> {
        let (held, image) = read(&image::device(dir, PARTITION, current))?;
        Ok(held.skyward.then(|| Kept { dir: dir.to_path_buf(), image, own: held.digest }))
    }

    /// The slots whose recovery partition gets this recovery back, by what
    /// the partitions hold once the install is done; `next` is the slot
    /// that starts next.
    pub fn wanted(&self, next: Slot) -> Result<Vec<Slot>> {
        let (a, _) = read(&image::device(&self.dir, PARTITION, Slot::A))?;
        let (b, _) = read(&image::device(&self.dir, PARTITION, Slot::B))?;
        Ok(restore([a, b], self.own, next))
    }

    /// Write this recovery into the recovery partitions of `slots` and read
    /// each back; `progress` hears the bytes so far, of all.
    pub fn write(&self, slots: &[Slot], progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        let devices: Vec<PathBuf> = slots.iter().map(|&slot| image::device(&self.dir, PARTITION, slot)).collect();
        image::flash(&mut Cursor::new(&self.image), self.image.len() as u64, &devices, progress)
    }
}

/// What the partition at `path` holds, read past any pages cached of it,
/// and all of it.
fn read(path: &Path) -> Result<(Held, Vec<u8>)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    image::drop_cache(&file).with_context(|| format!("drop the cached pages of {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).with_context(|| format!("read {}", path.display()))?;
    let skyward = image::is_skyward(&mut Cursor::new(&bytes)).with_context(|| format!("look into {}", path.display()))?;
    Ok((Held { digest: Sha256::digest(&bytes).into(), skyward }, bytes))
}

/// The slots whose recovery partition gets this recovery back once an
/// install is done, by what each holds `after` it, slot A first: `next`,
/// the slot that starts next, unless it already holds this build, whose
/// digest is `own`; and any other slot that holds no Skyward build at all.
///
/// Comparing what a slot held before an install with what it holds after
/// does not tell whether the install wrote over it: a package that flashes
/// the same recovery image as the time before writes the same bytes, so
/// the slot looks untouched and keeps a recovery that is not Skyward for
/// good. The switch asks for Skyward in the recovery partitions, so a slot
/// left holding anything else gets this recovery; one that holds a Skyward
/// build, this or another, is left as it is.
pub fn restore(after: [Held; 2], own: [u8; 32], next: Slot) -> Vec<Slot> {
    [Slot::A, Slot::B]
        .into_iter()
        .filter(|&slot| {
            let k = slot.number() as usize;
            match slot == next {
                true => after[k].digest != own,
                false => !after[k].skyward,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::samples::recovery;

    const SKYWARD: Held = Held { digest: [1; 32], skyward: true };
    const OWN: [u8; 32] = SKYWARD.digest;
    /// Another build of Skyward.
    const OTHER_BUILD: Held = Held { digest: [4; 32], skyward: true };
    const STOCK: Held = Held { digest: [2; 32], skyward: false };
    const OLD: Held = Held { digest: [3; 32], skyward: false };

    #[test]
    fn the_slot_that_starts_next_gets_this_build() {
        assert_eq!(restore([SKYWARD, STOCK], OWN, Slot::B), [Slot::B], "an update or a package that went to slot B");
        assert_eq!(restore([SKYWARD, OLD], OWN, Slot::B), [Slot::B], "an older recovery there, left alone by the install");
        assert_eq!(restore([SKYWARD, OTHER_BUILD], OWN, Slot::B), [Slot::B], "another build of Skyward there");
        assert!(restore([SKYWARD, SKYWARD], OWN, Slot::A).is_empty(), "this recovery is there already");
    }

    #[test]
    fn a_slot_that_holds_no_skyward_gets_it_back_however_the_install_wrote_it() {
        assert_eq!(restore([SKYWARD, STOCK], OWN, Slot::A), [Slot::B], "a slot the install put the stock recovery in");
        assert_eq!(restore([STOCK, STOCK], OWN, Slot::A), [Slot::A, Slot::B], "both written over");
        assert!(
            restore([SKYWARD, OTHER_BUILD], OWN, Slot::A).is_empty(),
            "another build of Skyward is Skyward, and only the slot that starts next takes this one"
        );
        assert_eq!(
            restore([SKYWARD, OTHER_BUILD], OWN, Slot::B),
            [Slot::B],
            "the slot that starts next takes this build, whatever Skyward build it held"
        );
        assert!(
            restore([SKYWARD, SKYWARD], OWN, Slot::B).is_empty(),
            "a package that left both filled with this build alone"
        );
    }

    #[test]
    fn a_package_that_writes_the_same_recovery_again_still_gets_skyward_back() {
        // The package flashes the same recovery image it flashed the time
        // before, so what the slot holds afterwards is byte for byte what it
        // held before: a comparison of the two would call the slot untouched
        // and leave the other recovery in it for good.
        assert_eq!(restore([STOCK, STOCK], OWN, Slot::A), [Slot::A, Slot::B]);
    }

    /// Recovery partitions of `size` bytes holding `a` and `b`.
    fn partitions(a: &[u8], b: &[u8], size: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, image) in [("recovery_a", a), ("recovery_b", b)] {
            let mut bytes = image.to_vec();
            bytes.resize(size, 0);
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn this_recovery_goes_back_where_an_install_wrote_over_it() {
        let (skyward, stock) = (recovery(true), recovery(false));
        let size = skyward.len().max(stock.len()) + 4096;
        let dir = partitions(&skyward, &stock, size);
        let kept = Kept::hold(dir.path(), Slot::A).unwrap().expect("slot A's partition holds Skyward");
        assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::B], "slot B holds a recovery that is not Skyward");
        // A package puts the stock recovery in both slots.
        let mut written = stock.clone();
        written.resize(size, 0);
        std::fs::write(dir.path().join("recovery_a"), &written).unwrap();
        assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::A, Slot::B], "slot B holds the stock recovery too");
        let mut told = Vec::new();
        kept.write(&[Slot::A], &mut |done, total| told.push((done, total))).unwrap();
        assert_eq!(told.last(), Some(&(2 * size as u64, 2 * size as u64)), "written and read back");
        let mut whole = skyward.clone();
        whole.resize(size, 0);
        assert_eq!(std::fs::read(dir.path().join("recovery_a")).unwrap(), whole);
        assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::B], "slot B still holds the stock recovery the package put there");
        assert_eq!(kept.wanted(Slot::B).unwrap(), [Slot::B], "an install that has slot B start next");
    }

    #[test]
    fn nothing_is_kept_when_this_recovery_was_started_without_being_flashed() {
        let dir = partitions(&recovery(false), &recovery(false), 1 << 20);
        assert!(Kept::hold(dir.path(), Slot::B).unwrap().is_none());
        let erased = partitions(&recovery(true), &[], 1 << 20);
        assert!(Kept::hold(erased.path(), Slot::B).unwrap().is_none(), "an erased partition");
        assert!(Kept::hold(erased.path(), Slot::A).unwrap().is_some());
    }
}

//! Checks `keep` against the images a real device build makes: the packed
//! Skyward recovery and the stock one it is built on.

use skyward_install::image;
use skyward_install::keep::Kept;
use skyward_vab::Slot;
use std::io::Cursor;
use std::path::Path;

/// The packed recovery and the stock one under the workspace's target
/// folder, when a device build has left them there. A machine that has not
/// built one skips these checks rather than failing on files it never had.
fn images() -> Option<(Vec<u8>, Vec<u8>)> {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join("target");
    let skyward = std::fs::read(target.join("skyward-vermeer.img")).ok()?;
    let stock = std::fs::read(target.join("stock/vermeer/recovery.img")).ok()?;
    Some((skyward, stock))
}

#[test]
fn the_built_image_reads_back_as_skyward() {
    let Some((bytes, stock)) = images() else { return };
    assert!(image::is_skyward(&mut Cursor::new(&bytes)).expect("read the packed image"), "the packed image is Skyward");
    assert!(!image::is_skyward(&mut Cursor::new(&stock)).expect("read the stock image"), "the stock image is not");
}

#[test]
fn an_install_that_writes_a_recovery_over_both_slots_gets_skyward_into_both() {
    let Some((skyward, stock)) = images() else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("recovery_a"), &skyward).unwrap();
    std::fs::write(dir.path().join("recovery_b"), &stock).unwrap();
    let kept = Kept::hold(dir.path(), Slot::A).unwrap().expect("slot A holds Skyward");
    // A package that writes a recovery of its own over both slots.
    std::fs::write(dir.path().join("recovery_a"), &stock).unwrap();
    std::fs::write(dir.path().join("recovery_b"), &stock).unwrap();
    let slots = kept.wanted(Slot::A).unwrap();
    assert_eq!(slots, [Slot::A, Slot::B], "neither slot holds Skyward any more");
    kept.write(&slots, &mut |_, _| {}).unwrap();
    assert_eq!(std::fs::read(dir.path().join("recovery_a")).unwrap(), skyward);
    assert_eq!(std::fs::read(dir.path().join("recovery_b")).unwrap(), skyward);
    assert!(kept.wanted(Slot::A).unwrap().is_empty(), "nothing left to write");
}

#[test]
fn a_slot_left_holding_the_same_other_recovery_gets_skyward_too() {
    let Some((skyward, stock)) = images() else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("recovery_a"), &skyward).unwrap();
    std::fs::write(dir.path().join("recovery_b"), &stock).unwrap();
    let kept = Kept::hold(dir.path(), Slot::A).unwrap().expect("slot A holds Skyward");
    assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::B], "a slot that was already the stock recovery is written back too");
}

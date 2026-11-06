//! Checks `keep` against the images a real device build makes: the packed
//! Skyward recovery and the stock one it is built on.

use skyward_install::image;
use skyward_install::keep::Kept;
use skyward_vab::Slot;
use std::io::Cursor;

const SKYWARD: &str = "/work/skyward/target/skyward-vermeer.img";
const STOCK: &str = "/work/skyward/target/stock/vermeer/recovery.img";

#[test]
fn the_built_image_reads_back_as_skyward() {
    let bytes = std::fs::read(SKYWARD).expect("the packed image");
    assert!(image::is_skyward(&mut Cursor::new(&bytes)).expect("read the packed image"), "the packed image is Skyward");
    let stock = std::fs::read(STOCK).expect("the stock image");
    assert!(!image::is_skyward(&mut Cursor::new(&stock)).expect("read the stock image"), "the stock image is not");
}

#[test]
fn an_install_that_writes_a_recovery_over_both_slots_gets_skyward_into_both() {
    let skyward = std::fs::read(SKYWARD).expect("the packed image");
    let stock = std::fs::read(STOCK).expect("the stock image");
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
    // The package writes the same recovery image it wrote the time before,
    // so slot B looks untouched between one install and the next. It still
    // holds no Skyward, and the switch asks for Skyward in both.
    let skyward = std::fs::read(SKYWARD).expect("the packed image");
    let stock = std::fs::read(STOCK).expect("the stock image");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("recovery_a"), &skyward).unwrap();
    std::fs::write(dir.path().join("recovery_b"), &stock).unwrap();
    let kept = Kept::hold(dir.path(), Slot::A).unwrap().expect("slot A holds Skyward");
    assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::B], "a slot that was already the stock recovery is written back too");
}

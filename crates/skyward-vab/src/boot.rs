//! The phone's boot control: which slot the bootloader starts next, and
//! what it holds of each slot. The phone's own service does the work, as
//! the bootloader reads it from where only the phone's own code knows to
//! write it.
//!
//! A phone has one of two kinds of boot control: the newer kind, registered
//! with the service directory, or the older kind, a hardware service. The
//! newer kind is taken where the phone describes one, as Android's own
//! update_engine takes it.

use crate::{BootSlots, MergeStatus, Slot};
use anyhow::{bail, Context, Result};
use std::path::Path;
use skyward_binder::driver::{Binder, Handle};
use skyward_binder::hidl::{self, Answer, Message};
use skyward_binder::parcel::{Parcel, Reader};
use skyward_binder::{service, status};

/// The newer kind, as it registers with the service directory.
pub const SERVICE: &str = "android.hardware.boot.IBootControl/default";
const INTERFACE: &str = "android.hardware.boot.IBootControl";

/// The older kind, by the version that also tells which slot starts next.
/// Each of its methods is called by the name of the version it came with,
/// which the service checks.
pub const HARDWARE_SERVICE: &str = "android.hardware.boot@1.2::IBootControl";
const FIRST: &str = "android.hardware.boot@1.0::IBootControl";
/// The version that tells the bootloader where an update stands.
const MERGE: &str = "android.hardware.boot@1.1::IBootControl";
const INSTANCE: &str = "default";

/// The methods, numbered as the newer kind numbers them.
const GET_ACTIVE: u32 = 1;
const GET_MERGE_STATUS: u32 = 4;
const IS_BOOTABLE: u32 = 6;
const IS_SUCCESSFUL: u32 = 7;
const SET_ACTIVE_SLOT: u32 = 9;
const SET_SNAPSHOT_MERGE_STATUS: u32 = 11;

/// The methods, numbered as the older kind numbers them: the first
/// version's in their order, then those each later version added.
const SET_ACTIVE: u32 = 4;
const BOOTABLE: u32 = 6;
const SUCCESSFUL: u32 = 7;
const SET_MERGE_STATUS: u32 = 9;
const MERGE_STATUS: u32 = 10;
const ACTIVE: u32 = 11;

/// How the older kind answers whether something holds of a slot.
const NO: i32 = 0;
const YES: i32 = 1;

/// The phone's boot control, as this program talks to it.
pub enum BootControl<'a> {
    Newer(Handle<'a>),
    Older(Handle<'a>),
}

/// What the running slot can say of the slots when the phone's boot
/// control cannot be asked: which slot this is, and whether each slot has
/// a boot image. The slot that starts next is taken as this one, since
/// nothing else can be asked.
pub fn fallback_slots() -> Result<BootSlots> {
    let current = crate::current_slot()?.context("not an A/B phone")?;
    let dir = Path::new(skyward_block::BY_NAME);
    let bootable = |slot: Slot| {
        ["boot", "init_boot", "vendor_boot"].iter().any(|name| {
            let path = dir.join(format!("{name}{}", slot.suffix()));
            std::fs::metadata(&path).is_ok_and(|m| m.len() > 0)
        })
    };
    Ok(BootSlots {
        active: current,
        bootable: [bootable(Slot::A), bootable(Slot::B)],
        successful: [current == Slot::A, current == Slot::B],
    })
}

/// The slot the bootloader starts next, as the running boot control tells
/// it.
pub fn next_slot() -> Result<Slot> {
    let (binder, hardware) = (Binder::open()?, Binder::hardware()?);
    let control = BootControl::open(&binder, &hardware)?;
    control.active()
}

/// Call `method` of the newer kind with what `args` writes, and read what
/// it answered with `answer`.
fn newer<T>(control: &Handle, method: u32, args: impl FnOnce(&mut Parcel), answer: impl FnOnce(&mut Reader) -> Result<T>) -> Result<T> {
    let mut call = Parcel::call(INTERFACE);
    args(&mut call);
    let reply = control.transact(method, &call)?;
    let mut read = reply.read();
    status::ok(&mut read)?;
    answer(&mut read)
}

/// Call `method` of the older kind, by the name of the version it came
/// with, with what `args` writes, and read what it answered with `answer`.
fn older<T>(
    control: &Handle,
    version: &str,
    method: u32,
    args: impl FnOnce(&mut Message),
    answer: impl FnOnce(&mut Answer) -> Result<T>,
) -> Result<T> {
    let mut call = Message::call(version);
    args(&mut call);
    let reply = control.transact(method, &call)?;
    let mut read = Answer::new(reply.bytes());
    read.status()?;
    answer(&mut read)
}

impl<'a> BootControl<'a> {
    /// Reach the running boot control: the newer kind through `binder`
    /// where the phone describes one, the older kind through `hardware`
    /// otherwise. Say so where it is not running.
    pub fn open(binder: &'a Binder, hardware: &'a Binder) -> Result<BootControl<'a>> {
        let not_running = |name: &str| format!("{name} is not running; the phone's boot control has to be started first");
        if service::declared(binder, SERVICE)? {
            let control = service::find(binder, SERVICE)?.with_context(|| not_running(SERVICE))?;
            return Ok(BootControl::Newer(control));
        }
        let control = hidl::find(hardware, HARDWARE_SERVICE, INSTANCE)?.with_context(|| not_running(HARDWARE_SERVICE))?;
        Ok(BootControl::Older(control))
    }

    /// The slot the bootloader starts next.
    pub fn active(&self) -> Result<Slot> {
        let number = match self {
            BootControl::Newer(c) => newer(c, GET_ACTIVE, |_| {}, |r| r.i32()),
            BootControl::Older(c) => older(c, HARDWARE_SERVICE, ACTIVE, |_| {}, |r| r.i32()),
        }
        .context("ask which slot starts next")?;
        match number {
            0 => Ok(Slot::A),
            1 => Ok(Slot::B),
            other => bail!("the boot control says slot {other} starts next"),
        }
    }

    /// What it holds of the slots now.
    pub fn slots(&self) -> Result<BootSlots> {
        let mut slots = BootSlots { active: self.active()?, bootable: [false; 2], successful: [false; 2] };
        for slot in [Slot::A, Slot::B] {
            let at = slot.number() as usize;
            slots.bootable[at] = self.bootable(slot).with_context(|| format!("ask whether slot {} starts", slot.letter()))?;
            slots.successful[at] = self.successful(slot).with_context(|| format!("ask whether slot {} has started fully", slot.letter()))?;
        }
        Ok(slots)
    }

    fn bootable(&self, slot: Slot) -> Result<bool> {
        match self {
            BootControl::Newer(c) => newer(c, IS_BOOTABLE, |p| p.i32(slot.number() as i32), |r| r.bool()),
            BootControl::Older(c) => older(c, FIRST, BOOTABLE, |m| m.u32(slot.number()), answered),
        }
    }

    fn successful(&self, slot: Slot) -> Result<bool> {
        match self {
            BootControl::Newer(c) => newer(c, IS_SUCCESSFUL, |p| p.i32(slot.number() as i32), |r| r.bool()),
            BootControl::Older(c) => older(c, FIRST, SUCCESSFUL, |m| m.u32(slot.number()), answered),
        }
    }

    /// Where an update stands, as the bootloader is told.
    pub fn merge_status(&self) -> Result<MergeStatus> {
        let said = match self {
            BootControl::Newer(c) => newer(c, GET_MERGE_STATUS, |_| {}, |r| r.i32()),
            BootControl::Older(c) => older(c, MERGE, MERGE_STATUS, |_| {}, |r| r.i32()),
        }
        .context("ask where an update stands")?;
        Ok(match said {
            0 => MergeStatus::None,
            1 => MergeStatus::Unknown,
            2 => MergeStatus::Snapshotted,
            3 => MergeStatus::Merging,
            4 => MergeStatus::Cancelled,
            other => bail!("the boot control says an update stands at {other}"),
        })
    }

    /// Tell the bootloader where an update stands.
    pub fn set_merge_status(&self, status: MergeStatus) -> Result<()> {
        let what = || format!("tell the bootloader an update stands at {status:?}");
        let done = match self {
            BootControl::Newer(c) => newer(c, SET_SNAPSHOT_MERGE_STATUS, |p| p.i32(status as i32), |_| Ok(true)),
            BootControl::Older(c) => older(c, MERGE, SET_MERGE_STATUS, |m| m.u32(status as u32), |r| r.bool()),
        }
        .with_context(what)?;
        if !done {
            bail!("the boot control would not {}", what());
        }
        Ok(())
    }

    /// Have the bootloader start `slot` next. The boot control gives it the
    /// tries a slot gets and takes away any mark that it does not start.
    pub fn set_active(&self, slot: Slot) -> Result<()> {
        let what = || format!("have slot {} start next", slot.letter());
        match self {
            BootControl::Newer(c) => newer(c, SET_ACTIVE_SLOT, |p| p.i32(slot.number() as i32), |_| Ok(())).with_context(what),
            BootControl::Older(c) => {
                // The answer is whether it worked, and why not in text it
                // points at.
                let why = older(c, FIRST, SET_ACTIVE, |m| m.u32(slot.number()), |r| {
                    let done = r.pointed()?;
                    if done.first().is_some_and(|&b| b != 0) {
                        return Ok(None);
                    }
                    let why = r.pointed()?;
                    Ok(Some(String::from_utf8_lossy(why.split(|&b| b == 0).next().unwrap_or_default()).into_owned()))
                })
                .with_context(what)?;
                match why {
                    None => Ok(()),
                    Some(why) => bail!("the boot control would not {}: {why}", what()),
                }
            }
        }
    }
}

/// Whether what the older kind was asked holds of a slot.
fn answered(read: &mut Answer) -> Result<bool> {
    match read.i32()? {
        YES => Ok(true),
        NO => Ok(false),
        other => bail!("the boot control answered {other}"),
    }
}

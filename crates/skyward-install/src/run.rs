//! The installer: a thread that installs one package, telling the screen
//! how it goes, or serves `adb rescue` command after command. A package
//! from the computer comes over USB, taken over for sideload or rescue and
//! handed back to adb after; a file on the phone is read where it is, and
//! a package the system left on a partition through the map of its blocks.

use crate::adb::{self, Connection, Offer, Service};
use crate::asked::Asked;
use crate::blockmap::BlockMap;
use crate::ffs::{Endpoints, Shared, Usb};
use crate::fuse::{self, Fault, LocalFile, Mount};
use crate::image::{self, ImageKind};
use crate::keep::Kept;
use crate::ota::{self, Log, Status};
use crate::package::{self, Build, Package, Phone, Update};
use crate::payload;
use crate::script::{self, Bar, Request};
use crate::signature::{self, Verdict};
use crate::{Command, Event, Failure, Options, Outcome, Reflash, Refusal, Stage, ENGINE_LOG, SCRIPT_LOG};
use anyhow::{anyhow, bail, ensure, Context as _, Result};
use skyward_sys::{kmsg, prop};
use skyward_vab::{Slot, SlotMaps, SnapshotState};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// What runs right before update_engine or an install script starts
/// writing. Either may write over the installed system's partitions, so
/// whatever still runs from them has to move off first.
pub type Detach = Box<dyn FnOnce() -> Result<()> + Send>;

const METADATA: &str = "/metadata";
const DATA: &str = "/data";

/// Where a zip's install script is put to run, as Android's recovery puts
/// it, and the version of the interface the script is told it runs under.
const BINARY: &str = "/tmp/update-binary";
const API_VERSION: &str = "3";
const RETRY: &str = "retry";

/// Scripts written for other recoveries start with the shell they find
/// here, and look for programs where those recoveries keep them. Some name
/// bash in their shebang, which the ramdisk does not hold, so both names
/// lead to the shell it does.
const SBIN_SHELL: &str = "/sbin/sh";
const SBIN_BASH: &str = "/sbin/bash";
const SHELL: &str = "/system/bin/sh";
const SCRIPT_PATH: &str = "/sbin:/system/bin:/system/xbin";

/// How long what an install script leaves running may keep its output open
/// once the script has ended, before it is ended too; and after that, how
/// long its output is waited on before it is let go.
const LINGER: Duration = Duration::from_secs(1);

/// The most often progress is told.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

pub struct Installer {
    events: Receiver<Event>,
    commands: Sender<Command>,
    /// Readable while events wait.
    wake: OwnedFd,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Installer {
    /// Take USB over and wait for `adb sideload`, from a thread of its own.
    /// `detach` runs before an update package or an install script writes;
    /// `options` say what else the install does.
    pub fn start(detach: Detach, options: Options) -> Result<Installer> {
        Installer::spawn(Origin::Computer, detach, options, run)
    }

    /// Install the file at `path`, from a thread of its own; `detach` and
    /// `options` as for [`Installer::start`].
    pub fn open(path: PathBuf, detach: Detach, options: Options) -> Result<Installer> {
        Installer::spawn(Origin::File, detach, options, move |ctx| run_file(ctx, &path))
    }

    /// Install the package the system asked for, from a thread of its own,
    /// with no one at the screen to agree to it: an update package or a zip
    /// with an install script, as Android's own recovery installs one.
    /// `retry` when the install started before and a restart cut it off;
    /// `detach` and `options` as for [`Installer::start`].
    pub fn asked(package: Asked, retry: bool, detach: Detach, options: Options) -> Result<Installer> {
        Installer::spawn(Origin::Asked { retry }, detach, options, move |ctx| match &package {
            Asked::File(path) => run_file(ctx, path),
            Asked::Mapped(map) => run_mapped(ctx, map),
        })
    }

    /// Take USB over and wait for `adb rescue`, from a thread of its own:
    /// packages it sends are installed as for [`Installer::start`], what
    /// the phone is is answered, and erasing all user data is asked of the
    /// screen with [`Event::Wipe`]. The computer stays on for command after
    /// command, until it asks the phone to leave or the screen cancels.
    pub fn rescue(detach: Detach, options: Options) -> Result<Installer> {
        Installer::spawn(Origin::Computer, detach, options, rescue)
    }

    fn spawn(
        origin: Origin,
        detach: Detach,
        options: Options,
        work: impl FnOnce(&mut Context) -> Outcome + Send + 'static,
    ) -> Result<Installer> {
        let (wake, signal) = pipe(libc::O_NONBLOCK)?;
        let (event_tx, events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let mut ctx =
            Context { events: event_tx, signal, commands: command_rx, shared: shared.clone(), detach: Some(detach), options, origin };
        let thread = thread::Builder::new()
            .name("installer".into())
            .spawn(move || {
                let outcome = work(&mut ctx);
                let _ = kmsg(&format!("install: {outcome:?}"));
                ctx.send(Event::Finished(outcome));
            })
            .context("start the installer")?;
        Ok(Installer { events, commands, wake, shared, thread: Some(thread) })
    }

    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// The events since the last call.
    pub fn events(&self) -> Vec<Event> {
        let mut drain = [0u8; 64];
        while unsafe { libc::read(self.wake.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len()) } > 0 {}
        self.events.try_iter().collect()
    }

    /// Write the image the installer holds to `partition` in `slots`.
    pub fn flash(&self, partition: &'static str, slots: Vec<Slot>) {
        // Only a finished installer stops listening, and then there is
        // nothing left to write.
        let _ = self.commands.send(Command::Flash { partition, slots });
    }

    /// Install the update the installer holds.
    pub fn install(&self) {
        let _ = self.commands.send(Command::Install);
    }

    /// Install the system the installer holds with a root of `size` bytes.
    pub fn install_system(&self, size: u64) {
        let _ = self.commands.send(Command::InstallSystem { size });
    }

    /// Tell rescue whether the user data the computer asked to be erased
    /// was.
    pub fn wiped(&self, erased: bool) {
        let _ = self.commands.send(Command::Wiped(erased));
    }

    /// Stop waiting for the computer, for where an image goes, or for an
    /// update to be agreed to.
    pub fn cancel(&self) -> Result<()> {
        let _ = self.commands.send(Command::Cancel);
        if self.shared.cancel() {
            // A read the computer never answers ends when the function goes
            // away.
            skyward_sys::set_prop("sys.usb.config", "none")?;
        }
        Ok(())
    }
}

impl Drop for Installer {
    /// The screen lets the installer go once it has finished.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the installer does not panic");
        }
    }
}

/// Where the package comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// The computer, with `adb sideload` or `adb rescue`.
    Computer,
    /// A file on the phone.
    File,
    /// The system, which asked before the recovery started for a package
    /// on the phone to be installed; `retry` when the install started
    /// before and a restart cut it off.
    Asked { retry: bool },
}

struct Context {
    events: Sender<Event>,
    /// The write end of the wake pipe.
    signal: OwnedFd,
    commands: Receiver<Command>,
    shared: Arc<Shared>,
    detach: Option<Detach>,
    options: Options,
    origin: Origin,
}

impl Context {
    fn send(&self, event: Event) {
        if self.events.send(event).is_ok() {
            // A full pipe already wakes the screen.
            unsafe { libc::write(self.signal.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        }
    }
}

fn run(ctx: &mut Context) -> Outcome {
    let (usb, endpoints) = match Usb::open(ctx.shared.clone()) {
        Ok(usb) => usb,
        Err(e) => return failed(e.context("set USB up for sideload")),
    };
    ctx.send(Event::Waiting);
    let mut conn = Connection::new(endpoints, banner(adb::SIDELOAD));
    ctx.shared.accepting(true);
    let accepted = conn.accept();
    ctx.shared.accepting(false);
    let (outcome, conn) = match accepted {
        // Cancelled as the computer started sending.
        Ok(_) if ctx.shared.cancelled() => {
            finish(&mut conn, adb::DONE);
            (Outcome::Cancelled, conn)
        }
        Ok(offer) => receive(ctx, conn, offer, false),
        Err(adb::Error::Interrupted) => (Outcome::Cancelled, conn),
        Err(adb::Error::OldHost) => (Outcome::Refused(Refusal::OldHost), conn),
        Err(e) => (failed(anyhow!(e).context("wait for adb sideload")), conn),
    };
    if let Err(e) = usb.close(conn.into_link()) {
        let _ = kmsg(&format!("install: USB not handed back to adb: {e:#}"));
    }
    // Cancelling while the transfer starts cuts it off.
    if ctx.shared.cancelled() && matches!(outcome, Outcome::Failed(_)) {
        return Outcome::Cancelled;
    }
    outcome
}

/// What installing came to while the package was still served.
enum Done {
    Decided(Outcome),
    /// update_engine failed with this code.
    Engine(i32),
    /// The install script ended with this status; `wipe_cache` when it
    /// asked for the cache partition to be erased after.
    Script { code: i32, wipe_cache: bool },
    Error(anyhow::Error),
}

/// Install the package the computer offers. `rescue` tells the computer
/// how the install went, as rescue does, where sideload only says the
/// package was read.
fn receive(ctx: &mut Context, mut conn: Connection<Endpoints>, offer: Offer, rescue: bool) -> (Outcome, Connection<Endpoints>) {
    let _ = kmsg(&format!("install: {} bytes offered in blocks of {}", offer.size, offer.block_size));
    let ending = |outcome: &Outcome| match (rescue, outcome) {
        (true, Outcome::Installed { .. } | Outcome::Flashed { .. } | Outcome::Ran { .. } | Outcome::SystemInstalled { .. }) | (false, _) => adb::DONE,
        (true, _) => adb::FAILED,
    };
    if let Err(e) = offer.check() {
        let outcome = failed(anyhow!(e));
        finish(&mut conn, ending(&outcome));
        return (outcome, conn);
    }
    ctx.send(Event::Checking { size: offer.size });
    let mounted = match fuse::mount(offer) {
        Ok(mounted) => mounted,
        Err(e) => {
            let outcome = failed(e);
            finish(&mut conn, ending(&outcome));
            return (outcome, conn);
        }
    };
    let mount = mounted.serve(conn);
    let done = install(ctx, &mount, offer.size).unwrap_or_else(Done::Error);
    let (mut conn, served) = mount.unmount();
    let outcome = conclude(done, served, ctx.origin);
    finish(&mut conn, ending(&outcome));
    (outcome, conn)
}

/// Install the file at `path`, read where it is.
fn run_file(ctx: &mut Context, path: &Path) -> Outcome {
    let (file, offer) = match LocalFile::open(path) {
        Ok(opened) => opened,
        Err(e) => return Outcome::Failed(Failure::Unreadable(format!("{e:#}"))),
    };
    let _ = kmsg(&format!("install: {}, {} bytes", path.display(), offer.size));
    serve_local(ctx, file, offer, path)
}

/// Install the package the system left on a partition, read through the
/// map of its blocks at `map`.
fn run_mapped(ctx: &mut Context, map: &Path) -> Outcome {
    let (blocks, offer) = match BlockMap::read(map).and_then(|m| m.open()) {
        Ok(opened) => opened,
        Err(e) => return Outcome::Failed(Failure::Unreadable(format!("{e:#}"))),
    };
    let _ = kmsg(&format!("install: the package {} maps, {} bytes", map.display(), offer.size));
    serve_local(ctx, blocks, offer, map)
}

/// Serve a package the phone holds, which `path` names, and install it.
fn serve_local<S: fuse::Source + Send + 'static>(ctx: &mut Context, source: S, offer: Offer, path: &Path) -> Outcome {
    if offer.size == 0 {
        return Outcome::Refused(Refusal::Unknown);
    }
    if let Err(e) = offer.check() {
        return failed(anyhow!(e).context(format!("serve {}", path.display())));
    }
    ctx.send(Event::Checking { size: offer.size });
    let mounted = match fuse::mount(offer) {
        Ok(mounted) => mounted,
        Err(e) => return failed(e),
    };
    let mount = mounted.serve(source);
    let done = install(ctx, &mount, offer.size).unwrap_or_else(Done::Error);
    let (_, served) = mount.unmount();
    conclude(done, served, ctx.origin)
}

/// Tell the computer the package has been read, so `adb sideload` ends; in
/// rescue, how installing it went.
fn finish(conn: &mut Connection<Endpoints>, message: &[u8; 8]) {
    if let Err(e) = conn.finish_with(message) {
        let _ = kmsg(&format!("install: the computer did not take the end of the transfer: {e}"));
    }
}

/// Serve `adb rescue` until the computer asks the phone to leave, the cable
/// is pulled for good, or the screen cancels: each command is carried out
/// and answered, and the screen hears how each install ended. Gives how
/// the last install ended, or that none was made.
fn rescue(ctx: &mut Context) -> Outcome {
    let (usb, endpoints) = match Usb::open(ctx.shared.clone()) {
        Ok(usb) => usb,
        Err(e) => return failed(e.context("set USB up for rescue")),
    };
    ctx.send(Event::Waiting);
    let mut conn = Connection::new(endpoints, banner(adb::RESCUE));
    let mut last = Outcome::Cancelled;
    let ended = loop {
        ctx.shared.accepting(true);
        let accepted = conn.accept_service();
        ctx.shared.accepting(false);
        let (service, remote) = match accepted {
            Ok(opened) => opened,
            Err(adb::Error::Interrupted) => break Ok(()),
            Err(e) => break Err(anyhow!(e).context("wait for adb rescue")),
        };
        let _ = kmsg(&format!("rescue: the computer asks for {service:?}"));
        let served = match service {
            Service::RescueInstall(offer) => match conn.take(remote) {
                Ok(()) => {
                    let (outcome, back) = receive(ctx, conn, offer, true);
                    conn = back;
                    ctx.send(Event::Done(outcome.clone()));
                    last = outcome;
                    // The screen may have left the page meanwhile.
                    if ctx.shared.cancelled() {
                        break Ok(());
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Service::RescueGetprop(name) => conn.take(remote).and_then(|()| conn.answer(rescue_answer(&name).as_bytes())),
            Service::RescueWipe { target, size } if target == "userdata" => {
                ctx.send(Event::Wipe);
                let erased = match ctx.commands.recv() {
                    Ok(Command::Wiped(erased)) => erased,
                    Ok(Command::Cancel) | Err(_) => break Ok(()),
                    Ok(other) => break Err(anyhow!("rescue waiting for user data to be erased was told {other:?}")),
                };
                conn.take(remote).and_then(|()| conn.answer(&adb::rescue_wiped(erased, size)))
            }
            Service::Reboot(target) => {
                // Taken and closed at once, as the phone leaves.
                if let Err(e) = conn.take(remote).and_then(|()| conn.end()) {
                    let _ = kmsg(&format!("rescue: the computer's reboot was not answered: {e}"));
                }
                ctx.send(Event::Reboot(target));
                break Ok(());
            }
            // What rescue does not serve is refused, as minadbd refuses it.
            _ => conn.send(&adb::Packet::new(adb::CLSE, 0, remote, [])),
        };
        if let Err(e) = served {
            let _ = kmsg(&format!("rescue: the computer went away mid-command: {e}"));
        }
    };
    if let Err(e) = usb.close(conn.into_link()) {
        let _ = kmsg(&format!("rescue: USB not handed back to adb: {e:#}"));
    }
    match ended {
        Ok(()) => last,
        Err(e) => failed(e),
    }
}

/// What rescue answers for `name`: the phone's properties it may give, and
/// its charge.
fn rescue_answer(name: &str) -> String {
    adb::rescue_getprop(name, |key| match key {
        adb::BATTERY_LEVEL => skyward_info::battery_now().ok().flatten().map(|b| b.level.to_string()),
        key => prop(key),
    })
}

/// The outcome, given how serving the package went: a package that stopped
/// being readable explains whatever failed after.
fn conclude(done: Done, served: Result<Option<Fault>>, origin: Origin) -> Outcome {
    match (done, served) {
        (Done::Decided(outcome), served) => {
            if let Err(e) = served {
                let _ = kmsg(&format!("install: after the package was read: {e:#}"));
            }
            outcome
        }
        // A script that ended well read what it needed, whatever it tried
        // to read past.
        (Done::Script { code: 0, wipe_cache }, served) => {
            if !matches!(served, Ok(None)) {
                let _ = kmsg(&format!("install: the package stopped being readable while the script ran: {served:?}"));
            }
            Outcome::Ran { wipe_cache }
        }
        (_, Err(e)) => failed(e.context("serve the package")),
        (_, Ok(Some(Fault::Transfer(why)))) => Outcome::Failed(match origin {
            Origin::Computer => Failure::Disconnected,
            Origin::File | Origin::Asked { .. } => Failure::Unreadable(why),
        }),
        (_, Ok(Some(Fault::Changed(_)))) => Outcome::Failed(Failure::Changed),
        (Done::Engine(code), Ok(None)) => Outcome::Failed(Failure::Engine(code)),
        (Done::Script { code, .. }, Ok(None)) => Outcome::Failed(Failure::Script(code)),
        (Done::Error(e), Ok(None)) => failed(e),
    }
}

/// Write the partitions a package carries whole, where it carries them so;
/// `None` when it is a delta the phone's own update engine puts together.
fn write_whole_payload<S>(ctx: &mut Context, mount: &Mount<S>, update: &Update) -> Result<Option<Vec<String>>> {
    let payload = match payload::open(fuse::PACKAGE) {        Ok(payload) => payload,
        Err(e) => {
            let _ = kmsg(&format!("install: the package's payload does not read ({e:#}); the update engine takes it"));
            return Ok(None);
        }
    };
    if !payload::is_whole_image(&payload) {
        return Ok(None);
    }
    let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
    let dir = Path::new(skyward_block::BY_NAME);
    let targets: Vec<payload::Target> = payload
        .partitions()
        .iter()
        .map(|p| payload::Target { partition: p.partition_name.clone(), device: image::device(dir, &p.partition_name, slot) })
        .filter(|t| t.device.try_exists().unwrap_or(false))
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    let held: Vec<String> = targets.iter().map(|t| t.partition.clone()).collect();
    let _ = kmsg(&format!("install: the package carries {held:?} whole, and this writes them"));
    if ctx.shared.cancelled() {
        return Ok(Some(Vec::new()));
    }
    ctx.send(Event::Installing { version: update.version().map(str::to_string) });
    let kept = hold(ctx, slot)?;
    // Moving off the installed system before it is written over: a mount
    // that will not come off is told of and gone on with, since the
    // partitions this package writes are named by it, not found through
    // what is mounted.
    if let Err(e) = detach_once(ctx) {
        let _ = kmsg(&format!("install: not everything moved off the installed system: {e:#}"));
    }
    let wrote = payload::write_partitions(
        &payload,
        &payload::package_file(fuse::PACKAGE)?,
        update.payload_offset,
        &targets,
        &mut |done, total| ctx.send(Event::Progress(Stage::Writing { done, total })),
    )?;
    if ctx.options.avb {
        if let Err(e) = disable_avb(slot) {
            let _ = kmsg(&format!("install: verified boot not turned off: {e:#}"));
        }
    }
    if let Some(kept) = &kept {
        reflash(ctx, kept, skyward_vab::boot::next_slot().context("ask which slot starts next"), slot);
    }
    Ok(Some(wrote))
}

fn install<S>(ctx: &mut Context, mount: &Mount<S>, size: u64) -> Result<Done> {
    let file = File::open(fuse::PACKAGE).context("open the package")?;
    let package = package::identify(BufReader::with_capacity(1 << 16, file))?;
    let _ = kmsg(&format!("install: {}", describe(&package)));
    match package {
        Package::Update(update) => {
            // A package whose partitions are whole images is written here,
            // without the phone's update engine: such a package is made to
            // be flashed outright, and what the engine is for — putting a
            // delta together from the system already on the phone — is not
            // what it holds.
            if let Some(written) = write_whole_payload(ctx, mount, &update)? {
                return Ok(Done::Decided(Outcome::Wrote { partitions: written }));
            }
            update_system(ctx, mount, &update)
        }
        // The system asks for packages that install themselves, as Android's
        // own recovery takes nothing else; an image needs someone to say
        // where it goes.
        Package::Image(_) if matches!(ctx.origin, Origin::Asked { .. }) => Ok(Done::Decided(Outcome::Refused(Refusal::Image))),
        Package::Image(kind) => flash_image(ctx, kind, size),
        Package::Script => run_script(ctx, size, || mount.pulled()),
        Package::System(contents) => install_system(ctx, &contents),
        Package::Unknown => Ok(Done::Decided(Outcome::Refused(Refusal::Unknown))),
    }
}

/// Install the system in the package beside the installed one, once the
/// user has picked how much room its root takes: its root goes into the
/// data partition, which has to be open, and nothing of the installed
/// system is written.
fn install_system(ctx: &Context, contents: &skyward_systems::package::Contents) -> Result<Done> {
    let refused = |r| Ok(Done::Decided(Outcome::Refused(r)));
    if matches!(ctx.origin, Origin::Asked { .. }) {
        return refused(Refusal::SystemAsked);
    }
    if let Some(refusal) = short_of_charge(ctx.origin)? {
        return refused(refusal);
    }
    let state = skyward_vab::snapshot_state()?;
    if state != SnapshotState::None {
        return refused(Refusal::Pending(state));
    }
    if !skyward_block::mount::is_mount_point(Path::new(DATA))? {
        return refused(Refusal::DataClosed);
    }
    if let Some(other) = skyward_systems::install::state()?.system {
        return refused(Refusal::SystemInstalled(other.name));
    }
    let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
    let room = skyward_systems::install::room_now(slot)?;
    let sizes = skyward_systems::install::sizes(contents.least(), room);
    if sizes.is_empty() {
        return refused(Refusal::NoRoom { least: contents.least(), room });
    }
    // The screen may have left the page before it heard the package came.
    if ctx.shared.cancelled() {
        return Ok(Done::Decided(Outcome::Cancelled));
    }
    let name = contents.manifest.name.clone();
    ctx.send(Event::System { name: name.clone(), version: contents.manifest.version.clone(), sizes: sizes.clone() });
    let size = match ctx.commands.recv() {
        Ok(Command::InstallSystem { size }) => size,
        Ok(Command::Cancel) | Err(_) => return Ok(Done::Decided(Outcome::Cancelled)),
        Ok(other) => bail!("a system waiting for its size was told {other:?}"),
    };
    ensure!(sizes.contains(&size), "{size} bytes is not among the sizes offered, {sizes:?}");
    let program = Path::new("/").join(skyward_config::ramdisk::SYSTEM_INIT);
    let program = fs::read(&program).with_context(|| format!("read {}", program.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::with_capacity(1 << 20, File::open(fuse::PACKAGE).context("open the package")?)).context("read the package")?;
    let mut told: Option<Instant> = None;
    skyward_systems::install::install(&mut zip, contents, size, &program, slot, &mut |step| {
        let stage = match step {
            skyward_systems::install::Step::Allocating { done, total } => Stage::Allocating { done, total },
            skyward_systems::install::Step::Writing { done, total } => Stage::Root { done, total },
            skyward_systems::install::Step::Finishing => Stage::Storing,
        };
        let last = matches!(stage, Stage::Storing) || matches!(stage, Stage::Allocating { done, total } | Stage::Root { done, total } if done == total);
        if last || told.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
            ctx.send(Event::Progress(stage));
            told = Some(Instant::now());
        }
    })?;
    let _ = kmsg(&format!("install: {name} installed beside the installed system, with {size} bytes for its root"));
    Ok(Done::Decided(Outcome::SystemInstalled { name }))
}

/// Whether the install waits for the user to agree before it writes. A
/// file a single tap picked always does, and so does an update from the
/// computer that erases all user data; what the system asked for it agreed
/// to itself, as with Android's own recovery.
fn waits(origin: Origin, wipe: bool) -> bool {
    match origin {
        Origin::File => true,
        Origin::Computer => wipe,
        Origin::Asked { .. } => false,
    }
}

/// Why there is too little charge to install, if there is: an install the
/// system asked for that a restart cut off goes on whatever the charge, as
/// Android's own recovery has it, the charge having been enough to start.
fn short_of_charge(origin: Origin) -> Result<Option<Refusal>> {
    match origin {
        Origin::Asked { retry: true } => Ok(None),
        _ => battery(),
    }
}

/// Put the installed system's boot set back into the boot partitions where
/// the second system's is, before anything writes them: Skyward is the only
/// one to write over the boot set it keeps track of, so that it never puts
/// back one older than what is there.
fn installed_boot_set() -> Result<()> {
    if skyward_systems::install::ensure_installed().context("put the installed system's boot set back")? {
        let _ = kmsg("install: the installed system's boot set went back into the boot partitions first");
    }
    Ok(())
}

/// Move off the installed system before the first write, where nothing has
/// yet: rescue may install one package after another.
fn detach_once(ctx: &mut Context) -> Result<()> {
    match ctx.detach.take() {
        Some(detach) => detach().context("move off the installed system"),
        None => Ok(()),
    }
}

fn describe(package: &Package) -> String {
    match package {
        Package::Update(u) => format!("an update to {}", u.version().unwrap_or("an unnamed version")),
        Package::Image(kind) => format!("an image of {kind:?}"),
        Package::Script => "a zip with an install script".into(),
        Package::System(c) => format!("the system {} {}, to install beside the installed one", c.manifest.name, c.manifest.version),
        Package::Unknown => "a file of no known kind".into(),
    }
}

fn update_system<S>(ctx: &mut Context, mount: &Mount<S>, update: &Update) -> Result<Done> {
    let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
    if let Err(refusal) = package::check(update, &phone(slot)?) {
        return Ok(Done::Decided(Outcome::Refused(refusal)));
    }
    if let Some(refusal) = short_of_charge(ctx.origin)? {
        return Ok(Done::Decided(Outcome::Refused(refusal)));
    }
    let state = skyward_vab::snapshot_state()?;
    if state != SnapshotState::None {
        return Ok(Done::Decided(Outcome::Refused(Refusal::Pending(state))));
    }
    // The screen may have left the page before it heard the package came.
    if ctx.shared.cancelled() {
        return Ok(Done::Decided(Outcome::Cancelled));
    }
    let version = update.version().map(str::to_string);
    let wipe = update.wipes();
    if waits(ctx.origin, wipe) {
        ctx.send(Event::Ready { version: version.clone(), wipe });
        match ctx.commands.recv() {
            Ok(Command::Install) => {}
            Ok(Command::Cancel) | Err(_) => return Ok(Done::Decided(Outcome::Cancelled)),
            Ok(other) => bail!("an update waiting to be agreed to was told {other:?}"),
        }
    }
    ctx.send(Event::Installing { version });
    let kept = hold(ctx, slot)?;
    installed_boot_set()?;
    detach_once(ctx)?;
    // update_engine mounts /metadata itself, as in the stock recovery, keeps
    // the update's state there, and unmounts it when done. The recovery
    // reads the state from it read-only again afterwards.
    let metadata = Path::new(METADATA);
    if skyward_block::mount::is_mount_point(metadata)? {
        skyward_block::mount::umount(metadata)?;
    }
    let code = apply(ctx, mount, update);
    if let Err(e) = remount_metadata() {
        let _ = kmsg(&format!("install: /metadata not mounted again: {e:#}"));
    }
    Ok(match code? {
        0 => {
            if let Some(kept) = &kept {
                reflash(ctx, kept, Ok(slot.other()), slot);
            }
            Done::Decided(Outcome::Installed { slot: slot.other(), wipe })
        }
        code => Done::Engine(code),
    })
}

/// This recovery, held before an install writes, when the screen asked to
/// keep it in the recovery partitions; none when it was started without
/// being flashed.
fn hold(ctx: &Context, current: Slot) -> Result<Option<Kept>> {
    if !ctx.options.keep {
        return Ok(None);
    }
    let kept = Kept::hold(Path::new(skyward_block::BY_NAME), current).context("hold this recovery from its partition")?;
    if kept.is_none() {
        let _ = kmsg("install: the recovery partition does not hold Skyward, so there is none to keep");
    }
    Ok(kept)
}

/// Write this recovery back into the recovery partitions the install wrote
/// over and into that of `next`, the slot that starts next, telling the
/// screen how far it is and then how it went.
/// Write this recovery back into the recovery partitions the install wrote
/// over and into that of `next`, the slot that starts next, telling the
/// screen how far it is and then how it went.
///
/// A boot control that cannot be asked — the phone's own boot service is
/// gone, as it is once a package has written over the partitions it runs
/// from — leaves `next` unknown, and `current` stands in for it: the slot
/// this recovery runs from is the one that starts next unless something
/// said otherwise, and writing this recovery into both slots is better than
/// writing it into neither.
fn reflash(ctx: &Context, kept: &Kept, next: Result<Slot>, current: Slot) {
    let next = match next {
        Ok(slot) => slot,
        Err(e) => {
            let _ = kmsg(&format!("install: which slot starts next is not known ({e:#}); this one is taken instead"));
            current
        }
    };
    let slots = match kept.wanted(next) {
        Ok(slots) if slots.is_empty() => return,
        Ok(slots) => slots,
        Err(e) => return ctx.send(Event::Reflashed(Reflash::Failed(format!("{e:#}")))),
    };
    let mut told: Option<Instant> = None;
    let written = kept.write(&slots, &mut |done, total| {
        if done == total || told.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
            ctx.send(Event::Progress(Stage::Reflashing { done, total }));
            told = Some(Instant::now());
        }
    });
    let _ = kmsg(&format!("install: this recovery written back to {slots:?}: {written:?}"));
    ctx.send(Event::Reflashed(match written {
        Ok(()) => Reflash::Written(slots),
        Err(e) => Reflash::Failed(format!("{e:#}")),
    }));
}

fn remount_metadata() -> Result<()> {
    let metadata = Path::new(METADATA);
    if !skyward_block::mount::is_mount_point(metadata)? {
        skyward_block::mount::mount_ro(&Path::new(skyward_block::BY_NAME).join("metadata"), metadata)?;
    }
    Ok(())
}

/// The phone, and the system on it as its mounted partitions tell.
fn phone(slot: Slot) -> Result<Phone> {
    let props = skyward_info::installed_props(slot)?;
    let installed = match (props.get("ro.build.date.utc"), props.get("ro.build.version.security_patch")) {
        (Some(time), Some(patch)) => Some(Build {
            timestamp: time.parse().with_context(|| format!("ro.build.date.utc {time:?}"))?,
            security_patch: patch.clone(),
        }),
        _ => None,
    };
    Ok(Phone {
        device: prop("ro.product.device").context("ro.product.device is unset")?,
        serial: prop("ro.serialno").unwrap_or_default(),
        installed,
    })
}

/// Why there is too little charge to install, if there is.
fn battery() -> Result<Option<Refusal>> {
    Ok(skyward_info::battery_now()?.as_ref().and_then(crate::battery_refusal))
}

/// A line update_engine wrote that matters here.
enum Line {
    Status(Status),
    Log(Log),
}

/// Run update_engine on the package, telling the screen how far it is,
/// and give its exit code.
fn apply<S>(ctx: &Context, mount: &Mount<S>, update: &Update) -> Result<i32> {
    let (status_read, status_write) = pipe(0)?;
    let status_fd = status_write.as_raw_fd();
    let mut command = std::process::Command::new(ota::PROGRAM);
    command
        .args(ota::arguments(fuse::PACKAGE, update, status_fd))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The program keeps its end of the status pipe across exec.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(status_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().with_context(|| format!("start {}", ota::PROGRAM))?;
    drop(status_write);
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut log = File::create(ENGINE_LOG).with_context(|| format!("create {ENGINE_LOG}"))?;

    let (tx, rx) = mpsc::channel();
    let status_tx = tx.clone();
    let status = thread::spawn(move || {
        lines(File::from(status_read), |line| {
            if let Some(s) = ota::status(line) {
                let _ = status_tx.send(Line::Status(s));
            }
            Ok(())
        })
    });
    let logged = thread::spawn(move || {
        lines(stderr, |line| {
            if let Some(l) = ota::log(line) {
                let _ = tx.send(Line::Log(l));
            }
            writeln!(log, "{line}")
        })
    });

    let total = update.payload_size;
    let mut stage = Stage::Writing { done: 0, total };
    ctx.send(Event::Progress(stage.clone()));
    let mut told = Instant::now();
    let (mut partitions, mut verifying, mut finishing, mut error) = (0, None, false, None);
    loop {
        match rx.recv_timeout(PROGRESS_EVERY) {
            Ok(Line::Status(Status::Step(step))) => finishing = step == 2,
            Ok(Line::Status(Status::Error(code))) => error = Some(code),
            Ok(Line::Log(Log::Partition(_))) => partitions += 1,
            Ok(Line::Log(Log::Overwriting)) => ctx.send(Event::Overwriting),
            Ok(Line::Log(Log::Hashing { index, partition })) => verifying = Some((index, partition)),
            Err(RecvTimeoutError::Timeout) => {}
            // Both outputs closed: the program has ended.
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = if finishing {
            Stage::Finishing
        } else if let Some((index, partition)) = &verifying {
            Stage::Verifying { index: *index, count: partitions.max(index + 1), partition: partition.clone() }
        } else {
            Stage::Writing { done: mount.served().saturating_sub(update.payload_offset).min(total), total }
        };
        let moved_on = std::mem::discriminant(&now) != std::mem::discriminant(&stage);
        if now != stage && (moved_on || told.elapsed() >= PROGRESS_EVERY) {
            stage = now;
            ctx.send(Event::Progress(stage.clone()));
            told = Instant::now();
        }
    }
    let ended = child.wait().context("wait for update_engine")?;
    status.join().expect("the status reader does not panic").context("read update_engine's status")?;
    logged.join().expect("the log reader does not panic").context("read update_engine's log")?;
    match (ended.code(), error) {
        (_, Some(code)) => Ok(code),
        (Some(code), None) => Ok(code),
        (None, None) => bail!("update_engine was ended by {ended}"),
    }
}

/// Hand each line of `from` to `each` without its line ending, until the
/// writer closes it.
fn lines(from: impl Read, mut each: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
    let mut reader = BufReader::new(from);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        each(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']))?;
    }
}

fn flash_image(ctx: &Context, kind: ImageKind, size: u64) -> Result<Done> {
    let dir = Path::new(skyward_block::BY_NAME);
    let partitions = image::partitions(kind, dir)?;
    if partitions.is_empty() {
        return Ok(Done::Decided(Outcome::Refused(Refusal::NoPartition)));
    }
    ctx.send(Event::Image { kind, size, partitions: partitions.clone() });
    let (partition, slots) = match ctx.commands.recv() {
        Ok(Command::Flash { partition, slots }) => (partition, slots),
        Ok(Command::Cancel) | Err(_) => return Ok(Done::Decided(Outcome::Cancelled)),
        Ok(other) => bail!("an image waiting for its partition was told {other:?}"),
    };
    ensure!(partitions.contains(&partition), "{partition} is not among {partitions:?}");
    ensure!(!slots.is_empty(), "no slot chosen for {partition}");
    if [skyward_systems::store::BOOT, skyward_systems::store::INIT_BOOT].contains(&partition) {
        installed_boot_set()?;
    }
    let devices: Vec<_> = slots.iter().map(|&slot| image::device(dir, partition, slot)).collect();
    let mut file = BufReader::with_capacity(1 << 20, File::open(fuse::PACKAGE).context("open the package")?);
    let mut told: Option<Instant> = None;
    image::flash(&mut file, size, &devices, &mut |done, total| {
        if done == total || told.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
            ctx.send(Event::Progress(Stage::Flashing { done, total }));
            told = Some(Instant::now());
        }
    })?;
    Ok(Done::Decided(Outcome::Flashed { partition, slots }))
}

/// Run the install script of the zip being served, `size` bytes, once the
/// user agrees, or at once where the system asked for it, as Android's
/// recovery runs one, among what the recoveries such scripts are written
/// for give them: their shell at /sbin, and the slot's partitions mapped
/// writable and unmounted. Where the screen asked for it, the zip is first
/// checked to be signed with one of the phone's update keys.
fn run_script(ctx: &mut Context, size: u64, served: impl Fn() -> u64) -> Result<Done> {
    if let Some(refusal) = short_of_charge(ctx.origin)? {
        return Ok(Done::Decided(Outcome::Refused(refusal)));
    }
    if ctx.options.signed {
        match verify_signature(ctx, size)? {
            Verdict::Signed => {}
            Verdict::Unsigned => return Ok(Done::Decided(Outcome::Refused(Refusal::Unsigned))),
            Verdict::Unverified => return Ok(Done::Decided(Outcome::Refused(Refusal::Unverified))),
            Verdict::Stopped => return Ok(Done::Decided(Outcome::Cancelled)),
        }
    }
    // The screen may have left the page before it heard the package came.
    if ctx.shared.cancelled() {
        return Ok(Done::Decided(Outcome::Cancelled));
    }
    // A script may write over any partition, so whoever is at the screen
    // agrees to it first; the system agreed to what it asked for.
    if !matches!(ctx.origin, Origin::Asked { .. }) {
        ctx.send(Event::Script);
        match ctx.commands.recv() {
            Ok(Command::Install) => {}
            Ok(Command::Cancel) | Err(_) => return Ok(Done::Decided(Outcome::Cancelled)),
            Ok(other) => bail!("a script waiting to be agreed to was told {other:?}"),
        }
    }
    let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
    extract_script()?;
    shell_at_sbin()?;
    bootctl_shim()?;
    let kept = hold(ctx, slot)?;
    installed_boot_set()?;
    // A package that lays super out again maps its partitions itself, and
    // cannot while this recovery holds them: it asks for the layout to be
    // rewritten, which takes every mapping off first, and a mapping left
    // standing fails that and stops the whole install. So a package of that
    // kind is given the slot unmapped and does the mapping its own way —
    // and nothing on the slot is held in memory for it either, since what
    // is held keeps its partition from being unmapped at all.
    let remakes_super = lays_super_out_again();
    if remakes_super {
        // The mounts this recovery keeps to read the installed system are
        // let go entirely, their device-mapper devices with them, before
        // anything holds a file on them.
        if let Err(e) = skyward_vab::release_whole(slot) {
            let _ = kmsg(&format!("install: the slot's partitions could not all be let go: {e:#}"));
        }
        let _ = kmsg("install: the package lays super out again, so nothing of the slot is held");
    } else {
        detach_once(ctx)?;
    }
    let mapped = match (skyward_vab::snapshot_state()?, remakes_super) {
        (SnapshotState::None, false) => skyward_vab::map_slot(slot)?,
        (SnapshotState::None, true) => {
            let _ = kmsg("install: the package lays super out again, so the slot stays unmapped for it");
            SlotMaps::default()
        }
        (state, _) => {
            let _ = kmsg(&format!("install: the slot's partitions stay unmapped while an update is in flight ({state:?})"));
            SlotMaps::default()
        }
    };
    let mounted = skyward_block::mount::mount_ids()?;
    let ran = run_binary(ctx, size, &served);
    if ctx.options.avb {
        if let Err(e) = disable_avb(slot) {
            let _ = kmsg(&format!("install: verified boot not turned off: {e:#}"));
        }
    }
    if let Err(e) = clean_up(&mounted, &mapped) {
        let _ = kmsg(&format!("install: not all the script left behind was taken away: {e:#}"));
    }
    // What the script wrote over goes back however it ended, and so does
    // the recovery of the slot that starts next, which a package that
    // installs to the other slot switches to.
    if let Some(kept) = &kept {
        reflash(ctx, kept, skyward_vab::boot::next_slot().context("ask which slot starts next"), slot);
    }
    let (code, wipe_cache) = ran?;
    Ok(Done::Script { code, wipe_cache })
}

/// Check the zip being served, `size` bytes, was signed with one of the
/// phone's update keys, telling the screen how much of it the check has
/// read; the screen leaving the page stops it.
fn verify_signature(ctx: &Context, size: u64) -> Result<Verdict> {
    let certificates = File::open(signature::CERTIFICATES).with_context(|| format!("open {}", signature::CERTIFICATES))?;
    let keys = signature::keys(BufReader::new(certificates)).with_context(|| format!("read {}", signature::CERTIFICATES))?;
    let package = File::open(fuse::PACKAGE).context("open the package")?;
    let mut told: Option<Instant> = None;
    let verdict = signature::verify(package, size, &keys, |done, total| {
        if done == total || told.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
            ctx.send(Event::Progress(Stage::Signature { done, total }));
            told = Some(Instant::now());
        }
        !ctx.shared.cancelled()
    })?;
    let _ = kmsg(&format!("install: the zip's signature: {verdict:?}"));
    Ok(verdict)
}

/// Put the zip's install script where it runs from, for anyone to run.
fn extract_script() -> Result<()> {
    let zip = File::open(fuse::PACKAGE).context("open the package")?;
    let mut zip = zip::ZipArchive::new(BufReader::new(zip)).context("read the zip directory")?;
    let mut script = zip.by_name(package::UPDATE_BINARY).context("open the install script")?;
    match fs::remove_file(BINARY) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("take away the {BINARY} an earlier install left")),
    }
    let mut out = OpenOptions::new().write(true).create_new(true).open(BINARY).with_context(|| format!("make {BINARY}"))?;
    io::copy(&mut script, &mut out).with_context(|| format!("write {BINARY}"))?;
    out.set_permissions(fs::Permissions::from_mode(0o755)).with_context(|| format!("make {BINARY} runnable"))
}

/// Whether the package being served lays super out again, its install
/// script asking for a new layout rather than writing into the one the
/// phone has: such a package maps the slot's partitions itself, and cannot
/// while this recovery holds them.
///
/// The script says so by name: it asks for the layout to be rewritten, or
/// maps a partition of super to write into it, both of which recoveries
/// that install these packages give it.
fn lays_super_out_again() -> bool {
    let read = || -> Result<String> {
        let zip = File::open(fuse::PACKAGE).context("open the package")?;
        let mut zip = zip::ZipArchive::new(BufReader::new(zip)).context("read the zip directory")?;
        let mut script = zip.by_name(package::UPDATER_SCRIPT).context("open the updater script")?;
        let mut text = String::new();
        script.take(1 << 20).read_to_string(&mut text).context("read the updater script")?;
        Ok(text)
    };
    match read() {
        Ok(text) => {
            let remake = text.contains("update_dynamic_partitions");
            let maps = text.contains("map_partition(");
            if remake || maps {
                let _ = kmsg(&format!(
                    "install: the package lays super out again ({}{})",
                    if remake { "update_dynamic_partitions" } else { "" },
                    if maps { " map_partition" } else { "" }
                ));
            }
            remake || maps
        }
        // A package with no script of that kind is installed the way every
        // other one is: the slot mapped for it.
        Err(_) => false,
    }
}

/// Give scripts written for other recoveries the bootctl they set the
/// active slot with. The package writes both slots and this recovery sets
/// the slot its system goes to itself, so the call only has to succeed.
fn bootctl_shim() -> Result<()> {
    let body = b"#!/system/bin/sh\nexit 0\n";
    for name in ["/sbin/bootctl"] {
        let path = Path::new(name);
        match fs::symlink_metadata(path) {
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("look at {name}")),
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("make {}", parent.display()))?;
        }
        fs::write(path, body).with_context(|| format!("write {name}"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).with_context(|| format!("make {name} runnable"))?;
    }
    Ok(())
}

/// Turn off verified boot's checks in `slot`'s top-level vbmeta: the two
/// flags fastboot sets for --disable-verity --disable-verification. The
/// signature no longer matches afterwards, which an unlocked bootloader
/// takes.
fn disable_avb(slot: Slot) -> Result<()> {
    let path = Path::new("/dev/block/by-name").join(format!("vbmeta{}", slot.suffix()));
    let mut file = OpenOptions::new().read(true).write(true).open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut head = [0u8; 124];
    file.read_exact(&mut head).with_context(|| format!("read {}", path.display()))?;
    ensure!(&head[..4] == b"AVB0", "{} is not a vbmeta image", path.display());
    let flags = u32::from_be_bytes(head[120..124].try_into().expect("four flag bytes")) | 3;
    file.seek(SeekFrom::Start(120)).with_context(|| format!("seek {}", path.display()))?;
    file.write_all(&flags.to_be_bytes()).with_context(|| format!("write {}", path.display()))?;
    file.sync_all().with_context(|| format!("sync {}", path.display()))?;
    let _ = kmsg(&format!("install: {} flags are now {flags}", path.display()));
    Ok(())
}

/// Give scripts written for other recoveries the shell they start with.
fn shell_at_sbin() -> Result<()> {
    for name in [SBIN_SHELL, SBIN_BASH] {
        let link = Path::new(name);
        match fs::symlink_metadata(link) {
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("look at {name}")),
        }
        let dir = link.parent().expect("the link is in a folder");
        fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
        symlink(SHELL, link).with_context(|| format!("link {name} to {SHELL}"))?;
    }
    Ok(())
}

/// A line one of the install script's outputs gave.
enum Said {
    Request(Request),
    /// A line on the request pipe asking for nothing this recovery knows.
    Unknown(String),
    /// A line the script, or a program it ran, wrote to its output.
    Output(String),
}

/// A process group, all that is still in it ended once this is let go.
struct Group(libc::pid_t);

impl Drop for Group {
    fn drop(&mut self) {
        // A group that has ended already is no one's to end.
        unsafe { libc::kill(-self.0, libc::SIGKILL) };
    }
}

/// Whether a line the script printed names a step of its work: a package
/// that writes a partition at a time says `正在刷入system分区` before each,
/// in whatever words, and counting those lines is how far along it is.
fn names_a_step(line: &str) -> bool {
    // The words the recoveries' packages use for writing a partition, in
    // the languages these packages are written in.
    let marks = ["刷入", "刷寫", "写入", "寫入", "flashing", "Flashing", "writing", "Writing"];
    let partitions = ["分区", "分割區", "partition", "Partition"];
    marks.iter().any(|m| line.contains(m)) && partitions.iter().any(|p| line.contains(p))
}

/// Run the install script: show what it prints and how far it says it is,
/// keep all it asks for and writes in the log, and give how it ended and
/// whether it asked for the cache partition to be erased after. What it
/// leaves running is ended with it.
///
/// `served` gives how many bytes of the package have been read so far: a
/// script that names no progress of its own is shown how far it is by what
/// it has had read, which is the work it waits on.
fn run_binary(ctx: &Context, size: u64, served: &impl Fn() -> u64) -> Result<(i32, bool)> {
    let mut log = File::create(SCRIPT_LOG).with_context(|| format!("create {SCRIPT_LOG}"))?;
    let (requests_read, requests_write) = pipe(0)?;
    let (output_read, output_write) = pipe(0)?;
    let fd = requests_write.as_raw_fd();
    let mut command = std::process::Command::new(BINARY);
    command.args([API_VERSION, &fd.to_string(), fuse::PACKAGE]);
    // A script started again after a restart cut it off is told so, as
    // Android's own recovery tells it, to pick up where it was.
    if ctx.origin == (Origin::Asked { retry: true }) {
        command.arg(RETRY);
    }
    command
        .current_dir("/")
        .env("PATH", SCRIPT_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_write.try_clone().context("share the script's output")?))
        .stderr(Stdio::from(output_write));
    // The script keeps its end of the request pipe across exec, makes files
    // as Android's recovery has it make them, and leads a process group of
    // its own, for what it leaves running to be ended with it.
    unsafe {
        command.pre_exec(move || {
            libc::umask(0o022);
            if libc::setpgid(0, 0) == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().with_context(|| format!("start {BINARY}"))?;
    let group = Group(child.id() as libc::pid_t);
    ctx.send(Event::Running);
    // The outputs close once only the script and what it runs hold them.
    drop(command);
    drop(requests_write);

    let (tx, rx) = mpsc::channel();
    let output_tx = tx.clone();
    // Neither reader is waited for: what the script leaves running may
    // hold its outputs open past its end.
    thread::spawn(move || {
        lines(File::from(requests_read), |line| {
            let said = script::parse(line).map_or_else(|| Said::Unknown(line.to_string()), Said::Request);
            let _ = tx.send(said);
            Ok(())
        })
    });
    thread::spawn(move || {
        lines(File::from(output_read), |line| {
            let _ = output_tx.send(Said::Output(line.to_string()));
            Ok(())
        })
    });

    let started = Instant::now();
    let mut bar = Bar::default();
    // The share the script itself says it is at, where it says one; a
    // package that flashes a partition at a time names none, and its steps
    // and the bytes it has pulled are what the page goes by instead.
    let thousandths = |bar: &Bar| bar.share(started.elapsed()).map(|share| (share * 1000.0).round() as u16);
    let (mut shown, mut told) = (None, Instant::now());
    // The steps the script said it took, counted from the lines it writes as
    // it goes: a package that flashes a partition at a time says so with a
    // line for each, and that count is the only measure of how far it is
    // where it names no progress of its own. The bytes read tell nothing of
    // it: a zip is opened at its end first and each file read from wherever
    // it lies, so what has been read reaches the whole package at once.
    let mut steps = 0u64;
    let mut told_steps = 0u64;
    let mut told_pulled = 0u64;
    let mut wipe_cache = false;
    let mut ended = None;
    let mut others_ended = false;
    loop {
        match rx.recv_timeout(PROGRESS_EVERY) {
            // An empty line only ends the one before it, in the recoveries
            // these scripts are written for.
            Ok(Said::Request(Request::Print(text))) if text.is_empty() => {}
            Ok(Said::Request(Request::Print(text))) => {
                writeln!(log, "{text}").context("write the log")?;
                if names_a_step(&text) {
                    steps += 1;
                }
                ctx.send(Event::Print(text));
            }
            Ok(Said::Request(Request::WipeCache)) => {
                writeln!(log, "[asked for the cache partition to be erased after]").context("write the log")?;
                wipe_cache = true;
            }
            Ok(Said::Request(Request::Log(text))) => writeln!(log, "{text}").context("write the log")?,
            Ok(Said::Request(Request::Other(what))) => writeln!(log, "[asked for {what}]").context("write the log")?,
            Ok(Said::Request(moving)) => bar.hear(&moving, started.elapsed()),
            Ok(Said::Unknown(line)) => writeln!(log, "[unknown request] {line}").context("write the log")?,
            Ok(Said::Output(line)) => writeln!(log, "{line}").context("write the log")?,
            Err(RecvTimeoutError::Timeout) => {}
            // Every output has closed: the script, and all it ran, are done.
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // How far it is, told as it moves on: every step it takes, and every
        // stretch of the package it pulls out, at most this often. A script
        // that reads a partition for a minute without a word of its own
        // still pulls bytes through the whole of it, so the page keeps
        // moving while it does.
        if told.elapsed() >= PROGRESS_EVERY {
            let share = thousandths(&bar).or_else(|| (steps > 0).then_some(u16::MAX));
            let pulled = served();
            let moved = share.is_some_and(|now| shown != Some(now)) || steps != told_steps || pulled != told_pulled;
            if moved {
                shown = share;
                told_steps = steps;
                told_pulled = pulled;
                told = Instant::now();
                ctx.send(Event::Progress(Stage::Script {
                    thousandths: share.unwrap_or(0),
                    steps,
                    pulled,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                }));
            }
        }
        if ended.is_none() {
            ended = child.try_wait().context("see whether the install script has ended")?.map(|status| (status, Instant::now()));
        }
        if let Some((_, at)) = ended {
            if at.elapsed() >= LINGER && !others_ended {
                unsafe { libc::kill(-group.0, libc::SIGKILL) };
                others_ended = true;
            }
            if at.elapsed() >= LINGER * 2 {
                writeln!(log, "[a program the script ran still holds its output, and is left]").context("write the log")?;
                break;
            }
        }
    }
    // Where the script got last, which telling the screen at most so often
    // may have held back.
    if let Some(last) = thousandths(&bar).filter(|&last| shown != Some(last)) {
        ctx.send(Event::Progress(Stage::Script {
            thousandths: last,
            steps,
            pulled: served(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        }));
    }
    let status = match ended {
        Some((status, _)) => status,
        None => child.wait().context("wait for the install script")?,
    };
    writeln!(log, "[ended: {status}]").context("write the log")?;
    Ok((status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)), wipe_cache))
}

/// Take away what the script mounted and left mounted, the latest first,
/// and undo how the slot's partitions were mapped for it; and mount
/// /metadata again as the recovery keeps it, for a script that took it
/// away.
fn clean_up(before: &[(i32, PathBuf)], mapped: &SlotMaps) -> Result<()> {
    let kept: HashSet<i32> = before.iter().map(|(id, _)| *id).collect();
    for (_, point) in skyward_block::mount::mount_ids()?.into_iter().rev().filter(|(id, _)| !kept.contains(id)) {
        skyward_block::mount::umount_detach(&point)?;
    }
    skyward_vab::unmap_slot(mapped)?;
    remount_metadata()
}

/// What the phone tells the computer it is.
fn banner(mode: &str) -> Vec<u8> {
    let p = |name| prop(name).unwrap_or_default();
    adb::banner(mode, &p("ro.product.name"), &p("ro.product.model"), &p("ro.product.device"))
}

fn failed(e: anyhow::Error) -> Outcome {
    Outcome::Failed(Failure::Error(format!("{e:#}")))
}

/// A pipe, both ends closed on exec, with `flags` added.
fn pipe(flags: libc::c_int) -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | flags) } != 0 {
        return Err(io::Error::last_os_error()).context("make a pipe");
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

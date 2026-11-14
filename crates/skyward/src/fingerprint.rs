//! The phone's fingerprint sensor, for opening the recovery's own lock.
//!
//! The templates never leave the secure world. The daemon that talks to it
//! is started when the lock first needs it; this program only asks for an
//! authentication and takes the answer, storing no fingerprint and no token.

use anyhow::{Context, Result};
use skyward_binder::driver::Binder;
use skyward_binder::hidl::{self, Answer, Message, Out};
use skyward_config::Service;
use std::ffi::CString;
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

const SERVICE: &str = "android.hardware.biometrics.fingerprint@2.1::IBiometricsFingerprint";
const INSTANCE: &str = "default";
const CALLBACK: &str = "android.hardware.biometrics.fingerprint@2.1::IBiometricsFingerprintClientCallback";
const BASE: &str = "android.hidl.base@1.0::IBase";
const SET_NOTIFY: u32 = 1;
const SET_ACTIVE_GROUP: u32 = 9;
const AUTHENTICATE: u32 = 10;
const ACTIVE_GROUP: &str = "/data/vendor_de/0/fpdata";
const DAEMON: &str = "/vendor/bin/hw/mfp-daemon";

/// A fingerprint attempt running on its own thread. The page learns how it
/// ended from [`Worker::ended`] when the wake pipe says it has.
pub struct Worker {
    events: Receiver<bool>,
    wake: OwnedFd,
}

impl Worker {
    pub fn start() -> Result<Worker> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(std::io::Error::last_os_error()).context("make the fingerprint worker's wake pipe");
        }
        let (wake, signal) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (tx, events) = mpsc::channel();
        thread::Builder::new()
            .name("fingerprint".into())
            .spawn(move || {
                let opened = authenticate().unwrap_or_else(|e| {
                    let _ = skyward_sys::kmsg(&format!("fingerprint: {e:#}"));
                    false
                });
                let _ = tx.send(opened);
                unsafe { libc::write(signal.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
            })
            .context("start the fingerprint worker")?;
        Ok(Worker { events, wake })
    }

    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// How the attempt ended, once it has; none while it still waits for a
    /// finger.
    pub fn ended(&self) -> Option<bool> {
        self.events.try_recv().ok()
    }
}

/// Ask the sensor for a finger and say whether one the phone knows came.
fn authenticate() -> Result<bool> {
    prepare()?;
    let binder = Binder::hardware().context("open the hardware driver")?;
    let service = hidl::find(&binder, SERVICE, INSTANCE)?.context("the fingerprint service is not running")?;
    let done = AtomicBool::new(false);
    let matched = AtomicBool::new(false);
    let mut serve = |code: u32, data: &[u8]| -> Result<Vec<u8>> {
        let mut read = Answer::new(data);
        let token = read.string()?;
        if token == CALLBACK {
            // The sensor's own calls, numbered from one as its interface
            // names them; onAuthenticated is three, onError is four.
            match code {
                3 => {
                    let fid = read.u64()?;
                    let _gid = read.u32()?;
                    let _hat = read.bytes()?;
                    if fid != 0 {
                        matched.store(true, Ordering::Release);
                    }
                    done.store(true, Ordering::Release);
                }
                4 => done.store(true, Ordering::Release),
                _ => {}
            }
            return Ok(Out::status().bytes_written().to_vec());
        }
        // The base interface every HIDL object serves, which the daemon
        // asks before it takes the callback: what it serves, and a hash
        // chain whose values it does not compare here.
        match code {
            1 => {
                let mut out = Out::new();
                out.strings(&[CALLBACK, BASE]);
                Ok(out.bytes_written().to_vec())
            }
            2 => {
                let mut out = Out::new();
                out.text(CALLBACK);
                Ok(out.bytes_written().to_vec())
            }
            3 => {
                let mut out = Out::new();
                out.hashes(&[[0u8; 32], [0u8; 32]]);
                Ok(out.bytes_written().to_vec())
            }
            _ => Ok(Out::status().bytes_written().to_vec()),
        }
    };

    let mut call = Message::call(SERVICE);
    call.object(1, 2);
    let reply = service.transact_with(SET_NOTIFY, &call, &mut serve).context("setNotify")?;
    Answer::new(reply.bytes()).status().context("setNotify refused")?;
    drop(reply);

    let mut call = Message::call(SERVICE);
    call.u32(0);
    call.text(ACTIVE_GROUP);
    let reply = service.transact_with(SET_ACTIVE_GROUP, &call, &mut serve).context("setActiveGroup")?;
    let status = Answer::new(reply.bytes()).i32().context("setActiveGroup answer")?;
    drop(reply);
    if status != 0 {
        return Ok(false);
    }

    let mut call = Message::call(SERVICE);
    call.u64(0);
    call.u32(0);
    let reply = service.transact_with(AUTHENTICATE, &call, &mut serve).context("authenticate")?;
    let status = Answer::new(reply.bytes()).i32().context("authenticate answer")?;
    drop(reply);
    if status != 0 {
        return Ok(false);
    }
    if !done.load(Ordering::Acquire) {
        binder.serve_until(&done, &mut serve).context("wait for the finger")?;
    }
    Ok(matched.load(Ordering::Acquire))
}

/// Give the daemon the files and nodes it expects, and start it where it is
/// not running. Its init script does this in the installed system; the
/// recovery does it here.
fn prepare() -> Result<()> {
    let dirs: [&str; 8] = [
        "/data/vendor/misc/mi_fp",
        "/data/vendor/goodix",
        "/data/vendor/fingerprint",
        "/data/vendor/fpc",
        "/data/vendor/fpdump",
        "/persist/mi_fp",
        "/persist/goodix",
        "/persist/fpc",
    ];
    for dir in dirs {
        if let Err(e) = fs::create_dir_all(dir) {
            let _ = skyward_sys::kmsg(&format!("fingerprint: make {dir}: {e}"));
            continue;
        }
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o770));
        if let Ok(at) = CString::new(dir) {
            unsafe { libc::chown(at.as_ptr(), 1000, 1000) };
        }
    }
    if let Ok(at) = CString::new("/dev/goodix_fp") {
        unsafe {
            libc::chmod(at.as_ptr(), 0o666);
            libc::chown(at.as_ptr(), 1000, 1000);
        }
    }
    let _ = fs::create_dir_all("/dev/mi_display");
    if !Path::new("/dev/mi_display/disp_feature").exists() {
        if let Ok(at) = CString::new("/dev/mi_display/disp_feature") {
            unsafe {
                libc::mknod(at.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(474, 0));
                libc::chmod(at.as_ptr(), 0o666);
            }
        }
    }
    if crate::services::running(DAEMON).is_none() {
        let service = Service {
            path: DAEMON.into(),
            paths: Vec::new(),
            args: Vec::new(),
            libraries: vec![
                "/odm/lib64".into(),
                "/vendor/lib64".into(),
                "/mnt/system/system/lib64".into(),
                "/mnt/system_ext/lib64".into(),
                "/system/lib64".into(),
                "/apex/com.android.vndk.v33/lib64".into(),
            ],
            oneshot: false,
            ready_property: None,
            ready_service: None,
            registers: Vec::new(),
            description: None,
            ready_hardware_service: Some("android.hardware.biometrics.fingerprint@2.1::IBiometricsFingerprint/default".into()),
            when: None,
            user: Some("system".into()),
            groups: vec!["system".into(), "drmrpc".into(), "diag".into(), "input".into(), "uhid".into()],
            capabilities: vec!["SYS_NICE".into()],
        };
        crate::services::start(&[service], None).context("start the fingerprint daemon")?;
    }
    Ok(())
}

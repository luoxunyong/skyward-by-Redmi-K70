//! The hardware service directory and the manifest the phone's own HIDL
//! services need to register with it in recovery.
//!
//! `libhidlbase` decides whether a phone has a hardware service directory
//! by looking for `/system/bin/hwservicemanager`. The recovery keeps the
//! installed system at `/mnt/system`, so that name is missing even while
//! the real program runs; without the name every HIDL service registers as
//! a passthrough and the boot control is never reachable.
//!
//! The installed vendor manifest is kept as fragments under `manifest/`.
//! The recovery's parser does not join them into the top-level document
//! the services are checked against, so the fragments are copied, the
//! services this recovery starts are written into a top-level document,
//! and the copy is put where the parser looks.

use anyhow::{Context, Result};
use std::ffi::CString;
use std::fs;
use std::path::Path;

const VENDOR: &str = "/vendor/etc/vintf";
const COPY: &str = "/tmp/vintf";
const MANAGER: &str = "/mnt/system/system/bin/hwservicemanager";
const LINK: &str = "/system/bin/hwservicemanager";

const MANIFEST: &str = r#"<manifest version="1.0" type="device">
    <hal format="hidl">
        <name>android.hardware.boot</name>
        <transport>hwbinder</transport>
        <fqname>@1.2::IBootControl/default</fqname>
    </hal>
    <hal format="hidl">
        <name>android.hardware.gatekeeper</name>
        <transport>hwbinder</transport>
        <fqname>@1.0::IGatekeeper/default</fqname>
    </hal>
    <hal format="hidl">
        <name>vendor.xiaomi.hardware.fx.tunnel</name>
        <transport>hwbinder</transport>
        <version>1.0</version>
        <interface><name>IMiFxTunnel</name><instance>default</instance></interface>
    </hal>
    <hal format="hidl">
        <name>vendor.xiaomi.hardware.fingerprintextension</name>
        <transport>hwbinder</transport>
        <version>1.0</version>
        <interface><name>IXiaomiFingerprint</name><instance>default</instance></interface>
    </hal>
    <hal format="hidl">
        <name>com.fingerprints.extension</name>
        <transport>hwbinder</transport>
        <version>3.0</version>
        <interface><name>IFingerprintSensorTest</name><instance>default</instance></interface>
        <interface><name>IFingerprintNavigation</name><instance>default</instance></interface>
    </hal>
    <hal format="hidl">
        <name>android.hardware.biometrics.fingerprint</name>
        <transport>hwbinder</transport>
        <version>2.1</version>
        <interface><name>IBiometricsFingerprint</name><instance>default</instance></interface>
    </hal>
</manifest>
"#;

/// Make the installed system's hardware services reachable from the
/// recovery: the name `libhidlbase` looks for, and a manifest that names
/// the services this recovery starts.
pub fn prepare() -> Result<()> {
    if !Path::new(LINK).exists() {
        std::os::unix::fs::symlink(MANAGER, LINK).with_context(|| format!("link {LINK} to {MANAGER}"))?;
    }
    // Already put in place on this boot: the copy is the mount's source, so
    // copying over it again would empty the directory it serves.
    if mounted(VENDOR) {
        return Ok(());
    }
    if Path::new(COPY).exists() {
        fs::remove_dir_all(COPY).with_context(|| format!("clear {COPY}"))?;
    }
    fs::create_dir_all(COPY).with_context(|| format!("make {COPY}"))?;
    copy_dir(Path::new(VENDOR), Path::new(COPY))?;
    fs::write(Path::new(COPY).join("manifest.xml"), MANIFEST).with_context(|| format!("write {COPY}/manifest.xml"))?;
    let source = CString::new(COPY).expect("no NUL in the copy's path");
    let target = CString::new(VENDOR).expect("no NUL in the vendor's path");
    let made = unsafe { libc::mount(source.as_ptr(), target.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) };
    if made != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("mount {COPY} over {VENDOR}"));
    }
    Ok(())
}

fn mounted(at: &str) -> bool {
    fs::read_to_string("/proc/self/mounts").is_ok_and(|mounts| mounts.lines().any(|line| line.split_whitespace().nth(1) == Some(at)))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry.with_context(|| format!("read {}", from.display()))?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type().with_context(|| format!("read the kind of {}", entry.path().display()))?;
        if kind.is_dir() {
            fs::create_dir_all(&target).with_context(|| format!("make {}", target.display()))?;
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).with_context(|| format!("copy {} to {}", entry.path().display(), target.display()))?;
        }
    }
    Ok(())
}

use anyhow::{anyhow, bail, Context, Result};
use skyward_config::DeviceConfig;
use skyward_sys::prop;
use skyward_vab::{Slot, SnapshotState};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Version of this build, shown on the About page.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const CPU: &str = "/sys/devices/system/cpu";
const POWER_SUPPLY: &str = "/sys/class/power_supply";
const SYS_BLOCK: &str = "/sys/class/block";

/// What the recovery shows about the device. A source that may legitimately
/// be absent (no battery driver loaded, build properties missing from the
/// recovery ramdisk) is an `Option`; a source that exists but cannot be read
/// or parsed is an error.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub model: String,
    pub codename: String,
    pub soc: Option<skyward_config::Names>,
    pub cpu: Cpu,
    /// `MemTotal` in KiB.
    pub ram_kib: u64,
    /// Panel resolution as width × height in pixels.
    pub panel: Option<(u32, u32)>,
    /// Marketed screen diagonal in inches.
    pub screen: Option<f32>,
    pub os_version: Option<String>,
    /// The maker's name for the system the version belongs to.
    pub os_name: Option<OsName>,
    pub android: Option<String>,
    /// The locale the installed system is built to start in, such as
    /// `zh-CN`.
    pub locale: Option<String>,
    pub security_patch: Option<PatchLevel>,
    pub kernel: String,
    pub storage: Storage,
    pub battery: Option<Battery>,
    pub slot: Option<Slot>,
    pub snapshot: SnapshotState,
    pub boot: Boot,
}

/// The makers' names for their systems, read from the build properties the
/// installed system carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsName {
    HyperOs,
    Miui,
    ColorOs,
    Flyme,
    OriginOs,
    RedMagicOs,
    MyOs,
    NebulaAios,
    HarmonyOs,
    HarmonyOsNext,
    MagicOs,
}

/// `ro.build.version.security_patch`, a calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchLevel {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl FromStr for PatchLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('-').collect();
        let [y, m, d] = parts.as_slice() else { bail!("security patch {s:?} is not YYYY-MM-DD") };
        let level = PatchLevel { year: parse(y, "patch year")?, month: parse(m, "patch month")?, day: parse(d, "patch day")? };
        if !(1..=12).contains(&level.month) || !(1..=31).contains(&level.day) {
            bail!("security patch {s:?} is not a date");
        }
        Ok(level)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cpu {
    pub cores: u32,
    /// Highest `cpuinfo_max_freq` across cores in kHz; absent until the
    /// cpufreq driver is loaded.
    pub max_khz: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Storage {
    /// Size of the userdata partition in bytes.
    pub total: u64,
    /// Free bytes on /data, counted like `File.getFreeSpace()`, while /data
    /// is mounted.
    pub free: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charge {
    Charging,
    Discharging,
    NotCharging,
    Full,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Battery {
    pub level: u8,
    pub charge: Charge,
    /// Tenths of a degree Celsius.
    pub temp: Option<i32>,
    /// Kernel `POWER_SUPPLY_HEALTH` text such as `Good`.
    pub health: Option<String>,
    pub cycles: Option<u32>,
    /// Design capacity in µAh.
    pub design: Option<u64>,
    /// Present full-charge capacity in µAh.
    pub full: Option<u64>,
    /// Voltage in µV.
    pub voltage: Option<u64>,
    /// A charger charging it quickly, as Xiaomi's Qualcomm phones tell
    /// beside their power supplies.
    pub quick: bool,
    /// A charger or a computer powers the phone, by cable or wirelessly: a
    /// power supply besides the battery is online.
    pub plugged: bool,
}

impl Battery {
    /// Whether it charges, or is full and topped up.
    pub fn charging(&self) -> bool {
        matches!(self.charge, Charge::Charging | Charge::Full)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// From `androidboot.vbmeta.device_state`.
    pub unlocked: Option<bool>,
    /// `androidboot.verifiedbootstate`: green, yellow, orange or red.
    pub verified: Option<String>,
}

pub fn collect(cfg: &DeviceConfig) -> Result<DeviceInfo> {
    let d = &cfg.display;
    let unlocked = match skyward_block::boot_param("androidboot.vbmeta.device_state")?.as_deref() {
        None => None,
        Some("locked") => Some(false),
        Some("unlocked") => Some(true),
        Some(other) => bail!("androidboot.vbmeta.device_state {other:?}"),
    };
    let system = SystemProps::current()?;
    let os_name = system.os_name();
    let os_version = system.os_version(&cfg.device.os_version_prop, os_name);
    Ok(DeviceInfo {
        // The system names the exact edition; the configured model covers a
        // system whose partitions are not mounted.
        model: system.get("ro.product.marketname").unwrap_or_else(|| cfg.device.model.clone()),
        codename: cfg.device.codename.clone(),
        soc: cfg.device.soc.clone(),
        cpu: cpu(Path::new(CPU))?,
        ram_kib: mem_total(&read("/proc/meminfo")?)?,
        panel: (d.width > 0 && d.height > 0).then_some((d.width, d.height)),
        screen: d.diagonal,
        os_version,
        os_name,
        android: system.get("ro.build.version.release"),
        locale: system.get("ro.product.locale"),
        security_patch: system.get("ro.build.version.security_patch").map(|s| s.parse()).transpose()?,
        kernel: kernel_release()?,
        storage: storage()?,
        battery: battery(Path::new(POWER_SUPPLY))?,
        slot: skyward_vab::current_slot()?,
        snapshot: skyward_vab::snapshot_state()?,
        boot: Boot { unlocked, verified: skyward_block::boot_param("androidboot.verifiedbootstate")? },
    })
}

/// Build properties of the system the About page describes.
enum SystemProps {
    /// Android is running and keeps them in its property area.
    Running,
    /// Recovery is running; they come from the build.prop files of the
    /// installed system's mounted partitions, none when nothing is mounted.
    Installed(HashMap<String, String>),
}

impl SystemProps {
    fn current() -> Result<SystemProps> {
        if !skyward_config::ramdisk::in_recovery() {
            return Ok(SystemProps::Running);
        }
        let props = match skyward_vab::current_slot()? {
            Some(slot) => installed_props(slot)?,
            None => HashMap::new(),
        };
        Ok(SystemProps::Installed(props))
    }

    fn get(&self, name: &str) -> Option<String> {
        match self {
            SystemProps::Running => prop(name),
            SystemProps::Installed(props) => props.get(name).cloned(),
        }
    }

    /// Whether any property name `f` accepts is set.
    fn any_key(&self, f: impl Fn(&str) -> bool) -> bool {
        match self {
            SystemProps::Running => false,
            SystemProps::Installed(props) => props.keys().any(|k| f(k)),
        }
    }

    /// Which maker's system the properties describe, by the properties each
    /// writes, or else by the name in its display build.
    fn os_name(&self) -> Option<OsName> {
        // The makers that replace a Xiaomi system are read first: their
        // builds can still carry Xiaomi's properties from the vendor side,
        // and a port may keep the maker's properties in a my_* partition.
        if self.get("ro.build.version.oplusrom").is_some()
            || self.get("ro.build.version.opporom").is_some()
            || self.get("ro.oppo.version").is_some()
            || self.any_key(|k| {
                k.starts_with("ro.oplus.") || k.starts_with("ro.oppo.") || k.contains("oplusrom") || k.contains("opporom")
            })
        {
            return Some(OsName::ColorOs);
        }
        if self.get("ro.build.flyme.version").is_some()
            || self.get("ro.flyme.version").is_some()
            || self.get("ro.flyme.os.version").is_some()
            || self.any_key(|k| k.starts_with("ro.flyme.") || k.starts_with("ro.meizu."))
        {
            return Some(OsName::Flyme);
        }
        if self.get("ro.build.version.magic").is_some()
            || self.get("ro.build.version.magicrom").is_some()
            || self.get("ro.honor.os.version").is_some()
            || self.any_key(|k| k.starts_with("ro.honor."))
        {
            return Some(OsName::MagicOs);
        }
        if self.get("ro.build.version.harmony").is_some()
            || self.get("ro.build.version.harmonyos").is_some()
            || self.get("hw_sc.build.platform.version").is_some()
            || self.any_key(|k| k.contains("harmony"))
        {
            return Some(if self.harmony_is_next() { OsName::HarmonyOsNext } else { OsName::HarmonyOs });
        }
        if self.get("ro.vivo.os.name").is_some()
            || self.get("ro.vivo.os.version").is_some()
            || self.get("ro.vivo.os.build.display.id").is_some()
            || self.any_key(|k| k.starts_with("ro.vivo.") || k.starts_with("ro.iqoo."))
        {
            return Some(OsName::OriginOs);
        }
        if self.get("ro.build.nubia.rom.name").is_some()
            || self.get("ro.nubia.rom.version").is_some()
            || self.get("ro.build.nubia.rom.version").is_some()
            || self.any_key(|k| k.starts_with("ro.nubia."))
        {
            let rom = self.get("ro.build.nubia.rom.name").unwrap_or_default().to_ascii_lowercase();
            return Some(if rom.contains("myos") { OsName::MyOs } else { OsName::RedMagicOs });
        }
        if self.get("ro.nebula.os.version").is_some()
            || self.get("ro.build.version.nebula").is_some()
            || self.any_key(|k| k.contains("nebula"))
        {
            return Some(OsName::NebulaAios);
        }
        if self.get("ro.mi.os.version.incremental").is_some()
            || self.get("ro.mi.os.version.name").is_some()
            || self.get("ro.mi.os.version.code").is_some()
            || self.any_key(|k| k.starts_with("ro.mi.os."))
        {
            return Some(OsName::HyperOs);
        }
        if self.get("ro.miui.ui.version.name").is_some()
            || self.get("ro.miui.ui.version.code").is_some()
            || self.any_key(|k| k.starts_with("ro.miui."))
        {
            return Some(OsName::Miui);
        }
        let display = self.get("ro.build.display.id").or_else(|| self.get("ro.build.description")).unwrap_or_default();
        let display = display.to_ascii_lowercase();
        if display.contains("harmonyos next") || display.contains("harmonyosnext") {
            Some(OsName::HarmonyOsNext)
        } else if display.contains("harmonyos") {
            Some(OsName::HarmonyOs)
        } else if display.contains("magicos") {
            Some(OsName::MagicOs)
        } else if display.contains("originos") {
            Some(OsName::OriginOs)
        } else if display.contains("redmagic") {
            Some(OsName::RedMagicOs)
        } else if display.contains("myos") {
            Some(OsName::MyOs)
        } else if display.contains("星云") || display.contains("nebula") {
            Some(OsName::NebulaAios)
        } else if display.contains("coloros") {
            Some(OsName::ColorOs)
        } else if display.contains("flyme") {
            Some(OsName::Flyme)
        } else if display.contains("miui") {
            Some(OsName::Miui)
        } else if display.contains("hyperos") {
            Some(OsName::HyperOs)
        } else {
            None
        }
    }

    /// Whether the HarmonyOS build is the one from 5 on, which Huawei calls
    /// HarmonyOS NEXT: its version starts at 5, or its name says NEXT.
    fn harmony_is_next(&self) -> bool {
        let display = self.get("ro.build.display.id").or_else(|| self.get("ro.build.description")).unwrap_or_default();
        if display.to_ascii_lowercase().contains("next") {
            return true;
        }
        self.get("ro.build.version.harmony")
            .or_else(|| self.get("ro.build.version.harmonyos"))
            .or_else(|| self.get("hw_sc.build.platform.version"))
            .and_then(|v| v.split(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|n| n.parse::<u32>().ok()))
            .is_some_and(|major| major >= 5)
    }

    /// The version as the maker names it for `name`, or the configured
    /// property where the maker is not known. A port that names none of
    /// these still shows the build's own name, so the row is never empty
    /// when the system tells of a version at all.
    fn os_version(&self, configured: &str, name: Option<OsName>) -> Option<String> {
        let version = match name {
            Some(OsName::Miui) => self.get("ro.build.version.incremental").or_else(|| self.get("ro.miui.ui.version.name")),
            Some(OsName::ColorOs) => self
                .get("ro.build.version.oplusrom")
                .or_else(|| self.get("ro.build.version.oplusrom.display"))
                .or_else(|| self.get("ro.build.version.opporom")),
            Some(OsName::Flyme) => self
                .get("ro.build.flyme.version")
                .or_else(|| self.get("ro.flyme.version"))
                .or_else(|| self.get("ro.flyme.os.version")),
            Some(OsName::HyperOs) => self
                .get(configured)
                .or_else(|| self.get("ro.mi.os.version.incremental"))
                .or_else(|| self.get("ro.mi.os.version.name"))
                .or_else(|| self.get("ro.mi.os.version.code")),
            Some(OsName::OriginOs) => self
                .get("ro.vivo.os.version")
                .or_else(|| self.get("ro.vivo.os.build.display.id"))
                .or_else(|| self.get("ro.vivo.os.name")),
            Some(OsName::RedMagicOs) | Some(OsName::MyOs) => self
                .get("ro.nubia.rom.version")
                .or_else(|| self.get("ro.build.nubia.rom.version"))
                .or_else(|| self.get("ro.build.nubia.rom.name")),
            Some(OsName::NebulaAios) => self.get("ro.nebula.os.version").or_else(|| self.get("ro.build.version.nebula")),
            Some(OsName::HarmonyOs) | Some(OsName::HarmonyOsNext) => self
                .get("ro.build.version.harmony")
                .or_else(|| self.get("ro.build.version.harmonyos"))
                .or_else(|| self.get("hw_sc.build.platform.version")),
            Some(OsName::MagicOs) => self
                .get("ro.build.version.magic")
                .or_else(|| self.get("ro.build.version.magicrom"))
                .or_else(|| self.get("ro.honor.os.version")),
            None => self.get(configured),
        };
        version.or_else(|| self.get("ro.build.display.id")).or_else(|| self.get("ro.build.version.incremental"))
    }
}

/// The order init reads build.prop files in at boot; later files override
/// earlier ones. OPPO's my_* partitions come last, as they do on its phones.
const PROP_ORDER: [&str; 16] = [
    "system",
    "system_ext",
    "system_dlkm",
    "vendor",
    "vendor_dlkm",
    "odm_dlkm",
    "odm",
    "product",
    "my_product",
    "my_stock",
    "my_heytap",
    "my_carrier",
    "my_manifest",
    "my_preload",
    "my_bigball",
    "my_company",
];

/// Every `build.prop` a partition holds, wherever its maker puts it: at the
/// root, in `etc`, or under one of the partition's own top-level trees
/// (`system`, `vendor`, OPPO's `my_product`, and so on). Symlinked trees are
/// left alone, so a recovered system's absolute links cannot lead out of it.
fn prop_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![root.to_path_buf()];
    let mut entries = fs::read_dir(root)
        .with_context(|| format!("read {}", root.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for path in entries {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            dirs.push(path);
        }
    }
    let mut files = Vec::new();
    let mut push = |file: PathBuf| {
        if fs::metadata(&file).is_ok_and(|m| m.is_file()) {
            files.push(file);
        }
    };
    for dir in dirs {
        push(dir.join("build.prop"));
        let etc = dir.join("etc");
        if fs::symlink_metadata(&etc).is_ok_and(|m| m.is_dir()) {
            push(etc.join("build.prop"));
        }
    }
    Ok(files)
}

/// What init names the properties the bootloader passes.
const BOOT_PROPS: &str = "ro.boot.";

/// The installed system's build properties from its mounted partitions, in
/// init's order. Partitions init does not list, such as Xiaomi's mi_ext,
/// come after the ones it does. Init has the properties the bootloader
/// passes before it reads any of these, and a file may import another named
/// after them, as Xiaomi's odm imports what each hardware version sets; so
/// they come first here too, the same in a recovery as in the system.
pub fn installed_props(slot: Slot) -> Result<HashMap<String, String>> {
    let base = |m: &skyward_vab::Mounted| -> String {
        m.partition.strip_suffix(slot.suffix()).expect("mounted partitions carry the slot suffix").to_string()
    };
    let mut parts = skyward_vab::mounted_partitions(slot)?;
    parts.sort_by_key(|m| PROP_ORDER.iter().position(|b| *b == base(m)).unwrap_or(PROP_ORDER.len()));
    let mut props: HashMap<String, String> = skyward_sys::props(BOOT_PROPS).into_iter().collect();
    for m in &parts {
        if base(m) == "vendor" {
            let path = m.mount_point.join("default.prop");
            if fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                rsproperties::load_properties_from_file(&path, None, "u:r:init:s0", &mut props)
                    .with_context(|| format!("read {}", path.display()))?;
            }
        }
        for path in prop_files(&m.mount_point)? {
            rsproperties::load_properties_from_file(&path, None, "u:r:init:s0", &mut props)
                .with_context(|| format!("read {}", path.display()))?;
        }
    }
    Ok(props)
}

fn read(path: impl AsRef<Path>) -> Result<String> {
    let p = path.as_ref();
    Ok(fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?.trim().to_string())
}

/// `None` when the file does not exist; any other failure is an error.
fn read_opt(path: impl AsRef<Path>) -> Result<Option<String>> {
    let p = path.as_ref();
    match fs::read_to_string(p) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

fn parse<T: FromStr>(s: &str, what: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    s.parse().map_err(|e| anyhow!("{what}: {s:?}: {e}"))
}

fn parse_opt<T: FromStr>(path: impl AsRef<Path>, what: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    read_opt(path)?.map(|s| parse(&s, what)).transpose()
}

fn cpu(root: &Path) -> Result<Cpu> {
    let ids = cpulist(&read(root.join("possible"))?)?;
    let mut max_khz: Option<u32> = None;
    for id in &ids {
        let path = root.join(format!("cpu{id}/cpufreq/cpuinfo_max_freq"));
        if let Some(khz) = parse_opt::<u32>(&path, "cpuinfo_max_freq")? {
            max_khz = Some(max_khz.map_or(khz, |m| m.max(khz)));
        }
    }
    Ok(Cpu { cores: ids.len() as u32, max_khz })
}

/// Kernel cpulist syntax: comma-separated ids and inclusive ranges (`0-3,6`).
fn cpulist(s: &str) -> Result<Vec<u32>> {
    let mut ids = Vec::new();
    for part in s.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (parse(a, "cpulist")?, parse(b, "cpulist")?);
                if a > b {
                    bail!("cpulist range {part:?} runs backwards");
                }
                ids.extend(a..=b);
            }
            None => ids.push(parse(part, "cpulist")?),
        }
    }
    Ok(ids)
}

fn mem_total(meminfo: &str) -> Result<u64> {
    let line = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .ok_or_else(|| anyhow!("MemTotal missing from /proc/meminfo"))?;
    let kib = line["MemTotal:".len()..]
        .trim()
        .strip_suffix("kB")
        .ok_or_else(|| anyhow!("MemTotal is not in kB: {line:?}"))?;
    parse(kib.trim(), "MemTotal")
}

fn kernel_release() -> Result<String> {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return Err(std::io::Error::last_os_error()).context("uname");
    }
    let release = unsafe { CStr::from_ptr(u.release.as_ptr()) };
    Ok(release.to_str().context("kernel release is not UTF-8")?.to_string())
}

fn storage() -> Result<Storage> {
    let link = Path::new(skyward_block::BY_NAME).join("userdata");
    let dev = fs::canonicalize(&link).with_context(|| format!("resolve {}", link.display()))?;
    let name = dev.file_name().ok_or_else(|| anyhow!("{} has no device name", dev.display()))?;
    let sectors: u64 = parse(&read(Path::new(SYS_BLOCK).join(name).join("size"))?, "block size")?;
    let free = if is_mountpoint(Path::new("/data"))? { Some(free_bytes("/data")?) } else { None };
    Ok(Storage { total: sectors * 512, free })
}

/// A directory is a mountpoint when it lives on a different filesystem from
/// its parent.
fn is_mountpoint(path: &Path) -> Result<bool> {
    let parent = path.parent().ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let here = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let up = fs::metadata(parent).with_context(|| format!("stat {}", parent.display()))?;
    Ok(here.dev() != up.dev())
}

fn free_bytes(path: &str) -> Result<u64> {
    let c = CString::new(path)?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {path}"));
    }
    // `fsblkcnt_t` is 32 bits on some hosts and 64 bits on Android.
    #[allow(clippy::useless_conversion)]
    let blocks = u64::from(s.f_bfree);
    Ok(blocks * s.f_frsize)
}

/// The battery as it is now.
pub fn battery_now() -> Result<Option<Battery>> {
    battery(Path::new(POWER_SUPPLY))
}

/// The first power supply whose type is `Battery`; `None` while no fuel-gauge
/// driver is loaded.
fn battery(root: &Path) -> Result<Option<Battery>> {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
    };
    let mut dirs: Vec<_> = entries.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
    dirs.sort();
    let mut first = None;
    let mut plugged = false;
    for dir in dirs {
        match read_opt(dir.join("type"))?.as_deref() {
            Some("Battery") => {
                first.get_or_insert(dir);
            }
            Some(_) => plugged |= online(&dir)?,
            None => {}
        }
    }
    let Some(dir) = first else { return Ok(None) };
    let charge = match read(dir.join("status"))?.as_str() {
        "Charging" => Charge::Charging,
        "Discharging" => Charge::Discharging,
        "Not charging" => Charge::NotCharging,
        "Full" => Charge::Full,
        "Unknown" => Charge::Unknown,
        other => bail!("{}: status {other:?}", dir.display()),
    };
    Ok(Some(Battery {
        level: parse(&read(dir.join("capacity"))?, "capacity")?,
        charge,
        temp: parse_opt(dir.join("temp"), "temp")?,
        health: read_opt(dir.join("health"))?,
        cycles: parse_opt(dir.join("cycle_count"), "cycle_count")?,
        design: parse_opt(dir.join("charge_full_design"), "charge_full_design")?,
        full: parse_opt(dir.join("charge_full"), "charge_full")?,
        voltage: parse_opt(dir.join("voltage_now"), "voltage_now")?,
        // How quickly the charger charges: 0 normally, then fast, flash,
        // turbo and super, which the stock SystemUI all counts quick.
        quick: parse_opt::<u8>(root.with_file_name("qcom-battery").join("quick_charge_type"), "quick_charge_type")?.is_some_and(|k| (1..=4).contains(&k)),
        plugged,
    }))
}

/// Whether the power supply in `dir` says a charger or a computer is
/// attached to it.
fn online(dir: &Path) -> Result<bool> {
    let path = dir.join("online");
    said_online(fs::read_to_string(&path)).with_context(|| format!("read {}", path.display()))
}

/// What a power supply's `online` file says, as reading it went. A supply
/// without one has no say, and one whose driver cannot tell yet, as
/// Qualcomm's USB-C connector cannot early in a boot, answers "try again":
/// either is taken as offline, and the next reading asks again.
fn said_online(read: std::io::Result<String>) -> std::io::Result<bool> {
    match read {
        Ok(s) => Ok(s.trim() == "1"),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::WouldBlock) => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> SystemProps {
        SystemProps::Installed(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    #[test]
    fn each_makers_system_is_named_by_its_own_properties() {
        assert_eq!(props(&[("ro.mi.os.version.incremental", "OS3.0.1")]).os_name(), Some(OsName::HyperOs));
        assert_eq!(props(&[("ro.miui.ui.version.name", "V14.0.5.0")]).os_name(), Some(OsName::Miui));
        assert_eq!(props(&[("ro.build.version.oplusrom", "V15.0.0")]).os_name(), Some(OsName::ColorOs));
        assert_eq!(props(&[("ro.build.flyme.version", "Flyme 10")]).os_name(), Some(OsName::Flyme));
        assert_eq!(props(&[("ro.vivo.os.name", "OriginOS 5")]).os_name(), Some(OsName::OriginOs));
        assert_eq!(props(&[("ro.build.nubia.rom.name", "REDMAGIC OS 10.0")]).os_name(), Some(OsName::RedMagicOs));
        assert_eq!(props(&[("ro.build.nubia.rom.name", "MyOS 14")]).os_name(), Some(OsName::MyOs));
        assert_eq!(props(&[("ro.build.display.id", "星云AIOS 1.0")]).os_name(), Some(OsName::NebulaAios));
        assert_eq!(props(&[("ro.build.version.harmony", "4.2.0")]).os_name(), Some(OsName::HarmonyOs));
        assert_eq!(props(&[("ro.build.version.harmony", "5.0.0")]).os_name(), Some(OsName::HarmonyOsNext));
        assert_eq!(props(&[("ro.build.version.harmony", "7.0.0")]).os_name(), Some(OsName::HarmonyOsNext));
        assert_eq!(props(&[("ro.build.version.magic", "9.0.0")]).os_name(), Some(OsName::MagicOs));
        assert_eq!(
            props(&[("ro.mi.os.version.incremental", "OS3.0.1"), ("ro.build.version.oplusrom", "V15.0.0")]).os_name(),
            Some(OsName::ColorOs),
            "a Xiaomi property left in vendor does not hide ColorOS"
        );
        assert_eq!(props(&[("ro.build.display.id", "ColorOS 15")]).os_name(), Some(OsName::ColorOs));
        assert_eq!(props(&[]).os_name(), None);
    }

    #[test]
    fn a_port_is_named_by_the_system_it_carries() {
        // A ColorOS port on a Xiaomi phone keeps Xiaomi's properties in the
        // vendor side; ColorOS is still what runs.
        let port = props(&[
            ("ro.mi.os.version.incremental", "OS2.0.215.0.VNKCNXM"),
            ("ro.miui.ui.version.name", "V816"),
            ("ro.build.version.oplusrom", "V15.0.0"),
        ]);
        assert_eq!(port.os_name(), Some(OsName::ColorOs));

        // Some OPPO properties only live in my_product, away from the named ones.
        let port = props(&[("ro.oplus.product.series", "Reno_series"), ("ro.miui.ui.version.name", "V14")]);
        assert_eq!(port.os_name(), Some(OsName::ColorOs));

        // A Flyme port on a Xiaomi phone.
        let port = props(&[("ro.mi.os.version.incremental", "OS2.0.215.0.VNKCNXM"), ("ro.flyme.version", "10.5.0.0A")]);
        assert_eq!(port.os_name(), Some(OsName::Flyme));

        // A HyperOS port still carries MIUI's own property, but is HyperOS.
        let port = props(&[("ro.mi.os.version.name", "OS2.0"), ("ro.miui.ui.version.name", "V816")]);
        assert_eq!(port.os_name(), Some(OsName::HyperOs));
    }

    #[test]
    fn the_system_version_is_shown_even_when_a_port_names_it_differently() {
        let port = props(&[("ro.build.version.oplusrom", "V15.0.0")]);
        assert_eq!(port.os_version("ro.mi.os.version.incremental", Some(OsName::ColorOs)).as_deref(), Some("V15.0.0"));

        let port = props(&[("ro.build.display.id", "Flyme 10.5.0.0A")]);
        assert_eq!(
            port.os_version("ro.mi.os.version.incremental", Some(OsName::Flyme)).as_deref(),
            Some("Flyme 10.5.0.0A")
        );

        let port = props(&[
            ("ro.mi.os.version.incremental", "OS2.0.215.0.VNKCNXM"),
            ("ro.build.version.incremental", "OS2.0.215.0.VNKCNXM"),
        ]);
        assert_eq!(
            port.os_version("ro.mi.os.version.incremental", Some(OsName::HyperOs)).as_deref(),
            Some("OS2.0.215.0.VNKCNXM")
        );

        let port = props(&[("ro.vivo.os.version", "5.0.0")]);
        assert_eq!(port.os_version("x", Some(OsName::OriginOs)).as_deref(), Some("5.0.0"));
        let port = props(&[("ro.nubia.rom.version", "10.0.0")]);
        assert_eq!(port.os_version("x", Some(OsName::RedMagicOs)).as_deref(), Some("10.0.0"));
        let port = props(&[("ro.build.version.harmony", "5.0.0")]);
        assert_eq!(port.os_version("x", Some(OsName::HarmonyOsNext)).as_deref(), Some("5.0.0"));
        let port = props(&[("ro.build.version.magic", "9.0.0")]);
        assert_eq!(port.os_version("x", Some(OsName::MagicOs)).as_deref(), Some("9.0.0"));

        // A system that names no version at all still shows what it calls itself.
        let port = props(&[("ro.build.display.id", "Unknown 1.0")]);
        assert_eq!(port.os_version("ro.mi.os.version.incremental", None).as_deref(), Some("Unknown 1.0"));
    }

    #[test]
    fn the_property_files_are_found_wherever_a_maker_puts_them() {
        let root = std::env::temp_dir().join(format!("skyward-props-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("my_product/etc")).unwrap();
        std::fs::create_dir_all(root.join("system/etc")).unwrap();
        std::fs::write(root.join("build.prop"), "ro.build.display.id=ColorOS 15\n").unwrap();
        std::fs::write(root.join("my_product/build.prop"), "ro.oplus.product.series=Reno_series\n").unwrap();
        std::fs::write(root.join("my_product/etc/build.prop"), "ro.oplus.x=1\n").unwrap();
        std::fs::write(root.join("system/build.prop"), "ro.build.version.oplusrom=V15.0.0\n").unwrap();
        std::fs::write(root.join("system/etc/build.prop"), "ro.oplus.y=1\n").unwrap();
        let found: Vec<String> = prop_files(&root)
            .unwrap()
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            found,
            vec![
                "build.prop",
                "my_product/build.prop",
                "my_product/etc/build.prop",
                "system/build.prop",
                "system/etc/build.prop"
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cpulist_ranges_and_singles() {
        assert_eq!(cpulist("0-7").unwrap(), (0..8).collect::<Vec<_>>());
        assert_eq!(cpulist("0-3,6,8-9").unwrap(), vec![0, 1, 2, 3, 6, 8, 9]);
        assert_eq!(cpulist("0").unwrap(), vec![0]);
        assert!(cpulist("3-1").is_err());
        assert!(cpulist("").is_err());
    }

    #[test]
    fn mem_total_from_meminfo() {
        // Values reported by a Xiaomi 17 Ultra.
        let text = "MemTotal:       15472464 kB\nMemFree:         2381608 kB\n";
        assert_eq!(mem_total(text).unwrap(), 15_472_464);
        assert!(mem_total("MemFree: 1 kB\n").is_err());
    }

    #[test]
    fn patch_level_parses_dates_only() {
        let p: PatchLevel = "2026-10-01".parse().unwrap();
        assert_eq!(p, PatchLevel { year: 2026, month: 10, day: 1 });
        assert!("2026-13-01".parse::<PatchLevel>().is_err());
        assert!("2026-10".parse::<PatchLevel>().is_err());
        assert!("October 2026".parse::<PatchLevel>().is_err());
    }

    #[test]
    fn kernel_release_is_readable() {
        assert!(!kernel_release().unwrap().is_empty());
    }

    #[test]
    fn a_battery_charged_quickly_says_so_beside_the_power_supplies() {
        // The 17 Ultra's in the recovery, on a charger it charges slowly on.
        let class = tempfile::tempdir().unwrap();
        let supplies = class.path().join("power_supply");
        let bat = supplies.join("battery");
        fs::create_dir_all(&bat).unwrap();
        for (name, value) in [("type", "Battery"), ("status", "Charging"), ("capacity", "96"), ("temp", "377")] {
            fs::write(bat.join(name), format!("{value}\n")).unwrap();
        }
        let b = battery(&supplies).unwrap().expect("a battery");
        assert_eq!((b.level, b.temp, b.quick), (96, Some(377), false), "a phone that does not tell charges as it may");
        let qcom = class.path().join("qcom-battery");
        fs::create_dir_all(&qcom).unwrap();
        fs::write(qcom.join("quick_charge_type"), "0\n").unwrap();
        assert!(!battery(&supplies).unwrap().expect("a battery").quick);
        fs::write(qcom.join("quick_charge_type"), "3\n").unwrap();
        assert!(battery(&supplies).unwrap().expect("a battery").quick, "turbo charging is quick");
    }

    #[test]
    fn the_phone_is_plugged_in_while_a_power_supply_besides_the_battery_is_online() {
        // The 13 Ultra's power supplies in the recovery.
        let class = tempfile::tempdir().unwrap();
        let supplies = class.path().join("power_supply");
        let supply = |name: &str, files: &[(&str, &str)]| {
            let dir = supplies.join(name);
            fs::create_dir_all(&dir).unwrap();
            for (file, value) in files {
                fs::write(dir.join(file), format!("{value}\n")).unwrap();
            }
        };
        supply("battery", &[("type", "Battery"), ("status", "Discharging"), ("capacity", "80")]);
        supply("usb", &[("type", "USB"), ("online", "0")]);
        supply("wireless", &[("type", "Wireless"), ("online", "0")]);
        supply("ucsi-source-psy-soc:qcom,pmic_glink:qcom,ucsi1", &[("type", "USB"), ("online", "0")]);
        assert!(!battery(&supplies).unwrap().expect("a battery").plugged, "on its battery alone");
        supply("usb", &[("type", "USB_PD"), ("online", "1")]);
        supply("ucsi-source-psy-soc:qcom,pmic_glink:qcom,ucsi1", &[("online", "1")]);
        assert!(battery(&supplies).unwrap().expect("a battery").plugged, "on a charger's cable");
        supply("usb", &[("type", "USB"), ("online", "0")]);
        supply("ucsi-source-psy-soc:qcom,pmic_glink:qcom,ucsi1", &[("online", "0")]);
        supply("wireless", &[("online", "1")]);
        assert!(battery(&supplies).unwrap().expect("a battery").plugged, "on a wireless charger");
    }

    #[test]
    fn a_power_supply_that_cannot_tell_yet_is_taken_as_offline() {
        use std::io::Error;
        assert!(said_online(Ok("1\n".into())).unwrap());
        assert!(!said_online(Ok("0\n".into())).unwrap());
        assert!(!said_online(Err(Error::from(ErrorKind::WouldBlock))).unwrap(), "Qualcomm's USB-C connector early in a boot");
        assert!(!said_online(Err(Error::from(ErrorKind::NotFound))).unwrap(), "a supply without the file");
        assert!(said_online(Err(Error::from(ErrorKind::PermissionDenied))).is_err(), "anything else is an error");
    }
}

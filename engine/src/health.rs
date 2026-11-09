//! Whether a drive itself says it is failing.
//!
//! Hard drives and SSDs keep a SMART self-assessment that the system can
//! ask for. Most USB card readers don't pass it through, and memory cards
//! have none, so for them the answer is "unknown"; read errors and slow
//! reads during a scan are the better sign there.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DriveStatus {
    Good,
    Failing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveHealth {
    pub status: DriveStatus,
    /// A sentence to show the user.
    pub detail: String,
}

impl DriveHealth {
    fn new(status: DriveStatus, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }
}

const FAILING: &str = "The drive reports that it is failing. Copy everything off it, or \
                       image it, before doing anything else.";
const GOOD: &str = "The drive reports no problems.";
const NOT_REPORTED: &str = "This drive does not report its health. Most card readers don't.";

/// Asks the system about the drive at `device` (`/dev/rdisk4`, `/dev/sdb`).
/// Uses `diskutil` on macOS and `smartctl` (smartmontools) on Linux.
pub fn check(device: &Path) -> DriveHealth {
    platform::check(device)
}

/// Reads the `SMARTStatus` key of `diskutil info -plist` output.
pub fn from_diskutil_plist(xml: &str) -> DriveHealth {
    match plist_string(xml, "SMARTStatus").as_deref() {
        Some("Verified") => DriveHealth::new(DriveStatus::Good, GOOD),
        Some("Failing") => DriveHealth::new(DriveStatus::Failing, FAILING),
        Some(_) => DriveHealth::new(DriveStatus::Unknown, NOT_REPORTED),
        None => DriveHealth::new(
            DriveStatus::Unknown,
            "The system did not say how healthy this drive is.",
        ),
    }
}

/// Finds `<key>KEY</key><string>VALUE</string>` in an XML property list.
/// diskutil's output is flat enough that this needs no plist parser.
fn plist_string(xml: &str, key: &str) -> Option<String> {
    let tag = format!("<key>{key}</key>");
    let rest = xml[xml.find(&tag)? + tag.len()..].trim_start();
    let rest = rest.strip_prefix("<string>")?;
    Some(rest[..rest.find("</string>")?].trim().to_string())
}

/// Reads `smartctl -H -j` output.
pub fn from_smartctl_json(json: &str) -> DriveHealth {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return DriveHealth::new(DriveStatus::Unknown, "smartctl gave no answer.");
    };
    match v["smart_status"]["passed"].as_bool() {
        Some(true) => DriveHealth::new(DriveStatus::Good, GOOD),
        Some(false) => DriveHealth::new(DriveStatus::Failing, FAILING),
        None => {
            // smartctl explains why it has no verdict: a USB bridge it can't
            // talk through, missing permissions, and so on.
            let why = v["smartctl"]["messages"]
                .as_array()
                .and_then(|m| m.iter().find_map(|m| m["string"].as_str()));
            match why {
                Some(why) => {
                    DriveHealth::new(DriveStatus::Unknown, format!("{NOT_REPORTED} ({why})"))
                }
                None => DriveHealth::new(DriveStatus::Unknown, NOT_REPORTED),
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// diskutil and smartctl know disks by their block node, not the raw one.
    pub fn block_node(device: &Path) -> String {
        device
            .to_string_lossy()
            .replacen("/dev/rdisk", "/dev/disk", 1)
    }

    pub fn check(device: &Path) -> DriveHealth {
        let dev = block_node(device);
        match std::process::Command::new("diskutil")
            .args(["info", "-plist", &dev])
            .output()
        {
            Ok(out) => from_diskutil_plist(&String::from_utf8_lossy(&out.stdout)),
            Err(e) => DriveHealth::new(DriveStatus::Unknown, format!("diskutil failed: {e}")),
        }
    }

    /// What diskutil says about the drive: SMART status, solid state,
    /// protocol, name and size.
    pub fn system_report(device: &Path) -> SmartReport {
        let dev = block_node(device);
        let args: [&std::ffi::OsStr; 3] = ["info".as_ref(), "-plist".as_ref(), dev.as_ref()];
        match run("diskutil".as_ref(), &args) {
            Ok(xml) => parse_diskutil(&xml),
            Err(_) => SmartReport::default(),
        }
    }

    pub const INSTALL_HINT: &str = "For full details (temperature, hours in use, damaged \
                                    sectors), install smartmontools: brew install smartmontools";
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub fn block_node(device: &Path) -> String {
        device.to_string_lossy().into_owned()
    }

    pub fn check(device: &Path) -> DriveHealth {
        // smartctl's exit code is a bit mask that is non-zero for a failing
        // drive too, so the JSON is read whatever it is.
        match std::process::Command::new("smartctl")
            .args(["-H", "-j"])
            .arg(device)
            .output()
        {
            Ok(out) => from_smartctl_json(&String::from_utf8_lossy(&out.stdout)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DriveHealth::new(
                DriveStatus::Unknown,
                "Install smartmontools to check drive health.",
            ),
            Err(e) => DriveHealth::new(DriveStatus::Unknown, format!("smartctl failed: {e}")),
        }
    }

    /// What sysfs says about the drive: name, size, how it is attached and
    /// whether it spins. It only fills gaps; smartctl knows better.
    pub fn system_report(device: &Path) -> SmartReport {
        let mut r = SmartReport::default();
        // /dev/disk/by-id/... links lead to the real node.
        let dev = device
            .canonicalize()
            .unwrap_or_else(|_| device.to_path_buf());
        let Some(name) = dev.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return r;
        };
        let base = Path::new("/sys/class/block").join(&name);
        let read = |rel: &str| {
            std::fs::read_to_string(base.join(rel))
                .ok()
                .and_then(|s| clean(&s))
        };
        let real = base.canonicalize().unwrap_or_default();
        r.usb = real.to_string_lossy().contains("/usb");
        r.capacity = read("size")
            .and_then(|s| s.parse::<u64>().ok())
            .and_then(|n| n.checked_mul(512))
            .filter(|&n| n > 0);
        let vendor = read("device/vendor").filter(|v| v != "ATA");
        r.model = match (vendor, read("device/model")) {
            (Some(v), Some(m)) if !m.starts_with(&v) => Some(format!("{v} {m}")),
            (_, Some(m)) => Some(m),
            (v, None) => v,
        };
        r.serial = read("device/serial");
        r.firmware = read("device/firmware_rev").or_else(|| read("device/rev"));
        if name.starts_with("nvme") {
            r.protocol = Some("NVMe".into());
            r.media = Some(Media::Ssd);
        } else if name.starts_with("mmcblk") {
            r.bus = Some("SD card".into());
        } else if base.join("loop/backing_file").exists() {
            r.bus = Some("Disk image (loop device)".into());
            r.notes.push(
                "This is a disk image attached as a loop device; images have no SMART data.".into(),
            );
        } else if !r.usb {
            // USB sticks often claim to spin; only trust this off USB.
            r.media = match read("queue/rotational").as_deref() {
                Some("0") => Some(Media::Ssd),
                Some("1") => Some(Media::Hdd),
                _ => None,
            };
        }
        r
    }

    pub const INSTALL_HINT: &str = "For full details (temperature, hours in use, damaged \
                                    sectors), install smartmontools (the smartctl command).";
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;

    pub fn block_node(device: &Path) -> String {
        device.to_string_lossy().into_owned()
    }

    pub fn check(_: &Path) -> DriveHealth {
        DriveHealth::new(
            DriveStatus::Unknown,
            "Checking drive health is not supported on this system.",
        )
    }

    pub fn system_report(_: &Path) -> SmartReport {
        SmartReport::default()
    }

    pub const INSTALL_HINT: &str = "Install smartmontools for drive health details.";
}

// ---------------------------------------------------------------------------
// The full report

/// Overall judgement of a drive's health.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Nothing wrong reported.
    Good,
    /// Early signs of wear or damage: back it up soon.
    Watch,
    /// The drive is failing: copy everything off it first.
    Failing,
    /// The drive reports nothing about its health.
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Media {
    Ssd,
    Hdd,
}

/// Everything Reperio could learn about a drive's health, from the system
/// and from smartctl, in one shape whatever the drive. Each value is
/// `None` when nothing reported it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SmartReport {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    /// Bytes.
    pub capacity: Option<u64>,
    /// How it is attached, in words: "SATA", "NVMe", "USB (SATA inside)".
    pub interface: Option<String>,
    pub media: Option<Media>,
    /// Degrees Celsius.
    pub temperature: Option<i64>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    /// Sectors that went bad and were replaced by spares.
    pub reallocated_sectors: Option<u64>,
    /// Sectors that could not be read and wait to be replaced.
    pub pending_sectors: Option<u64>,
    /// Sectors that could not be read even with error correction.
    pub uncorrectable_sectors: Option<u64>,
    /// SSD wear: how much of its rated endurance is used, in percent (it
    /// can pass 100).
    pub percentage_used: Option<u64>,
    /// NVMe spare blocks left, in percent, and the level at which the
    /// drive itself calls that critical.
    pub available_spare: Option<u64>,
    pub available_spare_threshold: Option<u64>,
    /// NVMe: errors the drive could not correct, over its life.
    pub media_errors: Option<u64>,
    /// The drive's own pass/fail self-assessment.
    pub smart_passed: Option<bool>,
    pub verdict: Verdict,
    /// Why the verdict is what it is, in plain words.
    pub reasons: Vec<String>,
    /// Anything else worth knowing: missing tools, adapters that hide SMART.
    pub notes: Vec<String>,
    /// "ATA", "NVMe" or "SCSI": what the drive itself speaks.
    #[serde(skip)]
    protocol: Option<String>,
    /// How it is attached when that is not USB: "SD card", "PCI Express".
    #[serde(skip)]
    bus: Option<String>,
    #[serde(skip)]
    usb: bool,
    /// NVMe critical warning bits.
    #[serde(skip)]
    critical_warning: Option<u64>,
    /// Attributes past their failure threshold: (name, failing now).
    #[serde(skip)]
    failed_attributes: Vec<(String, bool)>,
    /// Whether any SMART data at all came through.
    #[serde(skip)]
    has_smart: bool,
}

/// Longest smartctl or diskutil is given before it is stopped: a dying
/// drive behind a USB bridge can hang a command for minutes.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
/// More output than any real report; anything past it is ignored.
const MAX_OUTPUT: u64 = 4 << 20;
/// Attributes read from one report. ATA has at most 30; this only bounds
/// odd input.
const MAX_ATTRIBUTES: usize = 256;

/// Gathers everything about the drive at `device`: diskutil on macOS,
/// sysfs on Linux, and smartctl wherever it is installed. Never fails:
/// what cannot be learned is left out, and the notes say why.
///
/// smartctl is only asked to read the drive's SMART data (`-a`), which
/// changes nothing on it; no self-tests are started.
pub fn report(device: &Path) -> SmartReport {
    let mut r = platform::system_report(device);
    let dev = platform::block_node(device);
    match find_smartctl() {
        None => r.notes.push(platform::INSTALL_HINT.into()),
        Some(smartctl) => {
            let ask = |extra: &[&str]| {
                let mut args: Vec<&std::ffi::OsStr> = vec!["-a".as_ref(), "-j".as_ref()];
                args.extend(extra.iter().map(|a| std::ffi::OsStr::new(*a)));
                args.push(dev.as_ref());
                run(smartctl.as_os_str(), &args).map(|out| parse_smartctl(&out))
            };
            match ask(&[]) {
                Ok(mut s) => {
                    // USB adapters often hand SMART over only when SAT
                    // pass-through is asked for explicitly.
                    if !s.has_smart && (r.usb || s.usb) {
                        if let Ok(mut sat) = ask(&["-d", "sat"]) {
                            if sat.has_smart {
                                sat.usb = true;
                                s = sat;
                            }
                        }
                    }
                    s.fill_from(r);
                    r = s;
                }
                Err(e) => r.notes.push(format!("smartctl could not be run: {e}")),
            }
        }
    }
    r.finish();
    r
}

/// smartctl, from PATH or where Homebrew and distributions put it: apps
/// started from the Finder don't see Homebrew's PATH.
fn find_smartctl() -> Option<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/sbin",
        "/sbin",
    ] {
        dirs.push(d.into());
    }
    dirs.into_iter()
        .map(|d| d.join("smartctl"))
        .find(|p| p.is_file())
}

/// Runs a command and returns its standard output, giving up after
/// [`COMMAND_TIMEOUT`]. smartctl's exit code is a bit mask that is
/// non-zero for a failing drive too, so it is not looked at.
fn run(program: &std::ffi::OsStr, args: &[&std::ffi::OsStr]) -> std::io::Result<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let out = child.stdout.take();
    // Read on another thread, so a full pipe never blocks the command
    // while it is waited for.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = out {
            let _ = out.take(MAX_OUTPUT).read_to_end(&mut buf);
        }
        buf
    });
    let began = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if began.elapsed() < COMMAND_TIMEOUT => {
                std::thread::sleep(std::time::Duration::from_millis(30))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let buf = reader.join().unwrap_or_default();
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// A count from smartctl's JSON: a non-negative integer, or one at the
/// start of a string ("1234h+05m"). Negative, fractional or absurd values
/// are no answer.
fn count(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0 && *f < 1e15)
                .map(|f| f as u64)
        }),
        Value::String(s) => leading_number(s),
        _ => None,
    }
}

fn leading_number(s: &str) -> Option<u64> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .take(15)
        .collect();
    digits.parse().ok()
}

/// A text value, trimmed, without control characters, of sensible length.
fn text(v: &Value) -> Option<String> {
    clean(v.as_str()?)
}

fn clean(s: &str) -> Option<String> {
    let s: String = s.chars().filter(|c| !c.is_control()).take(120).collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// A temperature in Celsius, if it is one a drive could have.
fn celsius(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
        .filter(|t| (-40..=150).contains(t))
}

/// A sector count from an attribute's raw value. Some drives pack other
/// numbers into the high bytes; a count that large can't be real.
fn sector_count(raw: u64) -> Option<u64> {
    (raw <= u64::from(u32::MAX)).then_some(raw)
}

/// One ATA SMART attribute, as smartctl lists it.
struct Attribute<'a> {
    id: u64,
    /// Normalised value: counts down towards the threshold as it worsens.
    value: Option<u64>,
    raw: Option<u64>,
    raw_string: Option<&'a str>,
}

/// Reads `smartctl -a -j` output into a report, without a verdict (see
/// [`SmartReport::finish`]). Any of it may be missing or odd.
pub fn parse_smartctl(json: &str) -> SmartReport {
    let mut r = SmartReport::default();
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        r.notes
            .push("smartctl gave no answer that could be read.".into());
        return r;
    };
    if !v.is_object() {
        return r;
    }
    r.model = text(&v["model_name"]).or_else(|| {
        match (text(&v["scsi_vendor"]), text(&v["scsi_product"])) {
            (Some(a), Some(b)) => Some(format!("{a} {b}")),
            (a, b) => a.or(b),
        }
    });
    r.serial = text(&v["serial_number"]);
    r.firmware = text(&v["firmware_version"]).or_else(|| text(&v["scsi_revision"]));
    r.capacity = count(&v["user_capacity"]["bytes"])
        .or_else(|| count(&v["nvme_total_capacity"]))
        .filter(|&n| n > 0);
    r.protocol = text(&v["device"]["protocol"]);
    let dtype = text(&v["device"]["type"])
        .unwrap_or_default()
        .to_lowercase();
    let info = text(&v["device"]["info_name"]).unwrap_or_default();
    let messages: Vec<&Value> = v["smartctl"]["messages"]
        .as_array()
        .map(|m| m.iter().take(16).collect())
        .unwrap_or_default();
    r.usb = info.contains("[USB")
        || dtype.starts_with("usb")
        || dtype.starts_with("snt")
        || messages
            .iter()
            .any(|m| m["string"].as_str().is_some_and(|s| s.contains("USB")));
    match count(&v["rotation_rate"]) {
        Some(0) => r.media = Some(Media::Ssd),
        Some(_) => r.media = Some(Media::Hdd),
        None => {}
    }
    if r.protocol.as_deref() == Some("NVMe") {
        r.media = Some(Media::Ssd);
    }
    r.smart_passed = v["smart_status"]["passed"].as_bool();
    r.temperature = celsius(&v["temperature"]["current"]);
    r.power_on_hours = count(&v["power_on_time"]["hours"]);
    r.power_cycles = count(&v["power_cycle_count"]);

    // The NVMe health log.
    let log = &v["nvme_smart_health_information_log"];
    if log.is_object() {
        r.has_smart = true;
        r.critical_warning = count(&log["critical_warning"]);
        r.temperature = r.temperature.or_else(|| celsius(&log["temperature"]));
        r.available_spare = count(&log["available_spare"]).filter(|&n| n <= 100);
        r.available_spare_threshold =
            count(&log["available_spare_threshold"]).filter(|&n| n <= 100);
        r.percentage_used = count(&log["percentage_used"]).filter(|&n| n <= 255);
        r.power_cycles = r.power_cycles.or_else(|| count(&log["power_cycles"]));
        r.power_on_hours = r.power_on_hours.or_else(|| count(&log["power_on_hours"]));
        r.media_errors = count(&log["media_errors"]);
    }

    // ATA attributes.
    let mut attrs = Vec::new();
    let table = v["ata_smart_attributes"]["table"].as_array();
    for a in table.into_iter().flatten().take(MAX_ATTRIBUTES) {
        let Some(id) = count(&a["id"]) else { continue };
        let failed = a["when_failed"].as_str().unwrap_or("");
        if matches!(failed, "now" | "past" | "In_the_past") && r.failed_attributes.len() < 8 {
            let name = text(&a["name"]).unwrap_or_else(|| format!("Attribute {id}"));
            r.failed_attributes
                .push((name.replace('_', " "), failed == "now"));
        }
        attrs.push(Attribute {
            id,
            value: count(&a["value"]),
            raw: count(&a["raw"]["value"]),
            raw_string: a["raw"]["string"].as_str(),
        });
    }
    if !attrs.is_empty() {
        r.has_smart = true;
    }
    let attr = |id: u64| attrs.iter().find(|a| a.id == id);
    let raw = |id: u64| attr(id).and_then(|a| a.raw).and_then(sector_count);
    r.reallocated_sectors = raw(5);
    r.pending_sectors = raw(197);
    r.uncorrectable_sectors = raw(198).or_else(|| raw(187));
    r.power_on_hours = r
        .power_on_hours
        .or_else(|| attr(9).and_then(|a| a.raw_string.and_then(leading_number)));
    r.power_cycles = r.power_cycles.or_else(|| raw(12));
    // The low byte is the current temperature; the others can hold the
    // lowest and highest seen.
    r.temperature = r.temperature.or_else(|| {
        [194, 190]
            .iter()
            .find_map(|&id| attr(id)?.raw)
            .map(|t| (t & 0xFF) as i64)
            .filter(|t| (1..=150).contains(t))
    });

    // SSD wear: the device statistics say it directly; otherwise the
    // maker's life-left attribute, whose normalised value counts down
    // from 100.
    let stat = v["ata_device_statistics"]["pages"]
        .as_array()
        .and_then(|pages| {
            pages.iter().take(32).find_map(|p| {
                p["table"].as_array()?.iter().take(64).find_map(|e| {
                    (e["name"].as_str()? == "Percentage Used Endurance Indicator")
                        .then(|| count(&e["value"]))
                        .flatten()
                })
            })
        });
    r.percentage_used = r.percentage_used.or(stat.filter(|&n| n <= 255));
    if r.percentage_used.is_none() && r.media == Some(Media::Ssd) {
        r.percentage_used = [231, 233, 177, 202, 169]
            .iter()
            .find_map(|&id| attr(id)?.value.filter(|&n| n <= 100))
            .map(|left| 100 - left);
    }
    if r.smart_passed.is_some() {
        r.has_smart = true;
    }

    // What smartctl says when it could not get the data is worth passing on.
    for m in &messages {
        if m["severity"].as_str() == Some("error") && r.notes.len() < 3 {
            if let Some(s) = text(&m["string"]) {
                r.notes.push(format!("smartctl: {s}"));
            }
        }
    }
    r
}

/// Reads `diskutil info -plist` output into a report, without a verdict.
pub fn parse_diskutil(xml: &str) -> SmartReport {
    let mut r = SmartReport {
        model: plist_string(xml, "MediaName").and_then(|s| clean(&s)),
        capacity: plist_integer(xml, "TotalSize")
            .or_else(|| plist_integer(xml, "Size"))
            .filter(|&n| n > 0),
        media: plist_bool(xml, "SolidState").map(|ssd| if ssd { Media::Ssd } else { Media::Hdd }),
        ..Default::default()
    };
    match plist_string(xml, "BusProtocol").as_deref() {
        Some("USB") => r.usb = true,
        Some("SATA") => r.protocol = Some("ATA".into()),
        Some("PCI-Express") | Some("PCI") => r.bus = Some("PCI Express".into()),
        Some("Apple Fabric") => r.bus = Some("Built in (Apple Fabric)".into()),
        Some("Secure Digital") => r.bus = Some("SD card".into()),
        Some(other) => r.bus = clean(other),
        None => {}
    }
    match plist_string(xml, "SMARTStatus").as_deref() {
        Some("Verified") => r.smart_passed = Some(true),
        Some("Failing") => r.smart_passed = Some(false),
        _ => {}
    }
    r.has_smart = r.smart_passed.is_some();
    // Recent macOS also lists an NVMe drive's health log under this key.
    // Wide counters come as low and high halves; the low one is plenty.
    if let Some(at) = xml.find("<key>SMARTDeviceSpecificKeysMayVaryNotGuaranteed</key>") {
        let d = &xml[at..];
        let d = &d[..d.find("</dict>").unwrap_or(d.len())];
        let get = |k: &str| plist_integer(d, k);
        r.available_spare = get("AVAILABLE_SPARE").filter(|&n| n <= 100);
        r.available_spare_threshold = get("AVAILABLE_SPARE_THRESHOLD").filter(|&n| n <= 100);
        r.percentage_used = get("PERCENTAGE_USED").filter(|&n| n <= 255);
        r.power_on_hours = get("POWER_ON_HOURS_0");
        r.power_cycles = get("POWER_CYCLES_0");
        r.media_errors = get("MEDIA_ERRORS_0");
        r.critical_warning = get("CRITICAL_WARNING");
        // In kelvin, as NVMe reports it.
        r.temperature = get("TEMPERATURE")
            .map(|k| k as i64 - 273)
            .filter(|t| (-40..=150).contains(t));
        if r.available_spare.is_some() || r.percentage_used.is_some() {
            r.has_smart = true;
            r.protocol.get_or_insert_with(|| "NVMe".into());
        }
    }
    r
}

/// Finds `<key>KEY</key><integer>N</integer>`.
fn plist_integer(xml: &str, key: &str) -> Option<u64> {
    let tag = format!("<key>{key}</key>");
    let rest = xml[xml.find(&tag)? + tag.len()..].trim_start();
    let rest = rest.strip_prefix("<integer>")?;
    rest[..rest.find("</integer>")?].trim().parse().ok()
}

/// Finds `<key>KEY</key><true/>` or `<false/>`.
fn plist_bool(xml: &str, key: &str) -> Option<bool> {
    let tag = format!("<key>{key}</key>");
    let rest = xml[xml.find(&tag)? + tag.len()..].trim_start();
    if rest.starts_with("<true/>") {
        Some(true)
    } else if rest.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

impl SmartReport {
    /// Fills what this report lacks from `other` (what the system said).
    fn fill_from(&mut self, other: SmartReport) {
        macro_rules! fill {
            ($($f:ident),*) => { $( if self.$f.is_none() { self.$f = other.$f; } )* };
        }
        fill!(
            model,
            serial,
            firmware,
            capacity,
            media,
            temperature,
            power_on_hours,
            power_cycles,
            reallocated_sectors,
            pending_sectors,
            uncorrectable_sectors,
            percentage_used,
            available_spare,
            available_spare_threshold,
            media_errors,
            protocol,
            bus,
            critical_warning
        );
        // A failing self-assessment from either side stands.
        self.smart_passed = match (self.smart_passed, other.smart_passed) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (a, b) => a.or(b),
        };
        self.usb |= other.usb;
        self.has_smart |= other.has_smart;
        self.notes.extend(other.notes);
    }

    /// Names the interface and decides the verdict, with its reasons.
    pub fn finish(&mut self) {
        let inner = match self.protocol.as_deref() {
            Some("ATA") => Some("SATA"),
            Some("NVMe") => Some("NVMe"),
            Some("SCSI") => Some("SCSI"),
            _ => None,
        };
        self.interface = if self.usb {
            Some(match inner {
                Some(p) if p != "SCSI" => format!("USB ({p} inside)"),
                _ => "USB".to_string(),
            })
        } else {
            match (&self.bus, inner) {
                (Some(bus), Some(p)) if bus.starts_with("PCI") => Some(p.to_string()),
                (Some(bus), _) => Some(bus.clone()),
                (None, p) => p.map(str::to_string),
            }
        };
        let (verdict, reasons) = self.assess();
        self.verdict = verdict;
        self.reasons = reasons;
    }

    fn assess(&self) -> (Verdict, Vec<String>) {
        use Verdict::*;
        let mut verdict = Good;
        let mut reasons = Vec::new();
        let mut say = |level: Verdict, why: String| {
            if level == Failing || verdict == Failing {
                verdict = Failing;
            } else if level == Watch {
                verdict = Watch;
            }
            reasons.push(why);
        };
        let sectors = |n: u64| {
            if n == 1 {
                "1 sector".to_string()
            } else {
                format!("{n} sectors")
            }
        };
        if self.smart_passed == Some(false) {
            say(
                Failing,
                "The drive's own health check says it is failing.".into(),
            );
        }
        for (name, now) in &self.failed_attributes {
            if *now {
                say(
                    Failing,
                    format!("“{name}” is past the point its maker calls failing."),
                );
            } else {
                say(
                    Watch,
                    format!("“{name}” was past its failure point at some time."),
                );
            }
        }
        if let Some(n) = self.pending_sectors.filter(|&n| n > 0) {
            say(
                Failing,
                format!(
                    "{} could not be read and are waiting to be replaced.",
                    sectors(n)
                ),
            );
        }
        if let Some(n) = self.uncorrectable_sectors.filter(|&n| n > 0) {
            say(
                Failing,
                format!(
                    "{} could not be read even with error correction.",
                    sectors(n)
                ),
            );
        }
        if let Some(n) = self.reallocated_sectors.filter(|&n| n > 0) {
            // A few replaced sectors are common on old drives; many mean
            // the surface is going.
            let level = if n >= 100 { Failing } else { Watch };
            say(
                level,
                format!("{} went bad and were replaced with spares.", sectors(n)),
            );
        }
        let warning = self.critical_warning.unwrap_or(0);
        if warning & 0x01 != 0 {
            say(Failing, "The SSD is running out of spare blocks.".into());
        }
        if warning & 0x04 != 0 {
            say(Failing, "The SSD says its reliability is degraded.".into());
        }
        if warning & 0x08 != 0 {
            say(
                Failing,
                "The SSD has made itself read-only to protect your data.".into(),
            );
        }
        if warning & 0x02 != 0 {
            say(Watch, "The SSD has reached a critical temperature.".into());
        }
        if warning & 0x10 != 0 {
            say(Watch, "The SSD's power-loss protection has failed.".into());
        }
        if let Some(spare) = self.available_spare {
            let limit = self.available_spare_threshold.unwrap_or(10);
            if spare <= limit && warning & 0x01 == 0 {
                say(
                    Failing,
                    format!("Only {spare}% of the SSD's spare blocks are left."),
                );
            } else if spare > limit && spare < 50 {
                say(
                    Watch,
                    format!("Only {spare}% of the SSD's spare blocks are left."),
                );
            }
        }
        if let Some(used) = self.percentage_used.filter(|&u| u >= 90) {
            say(
                Watch,
                if used >= 100 {
                    format!("The SSD has passed its rated lifetime of writes ({used}% used).")
                } else {
                    format!(
                        "The SSD is near the end of its rated lifetime of writes ({used}% used)."
                    )
                },
            );
        }
        if let Some(n) = self.media_errors.filter(|&n| n > 0) {
            say(
                Watch,
                if n == 1 {
                    "The SSD has had 1 data error it could not correct.".into()
                } else {
                    format!("The SSD has had {n} data errors it could not correct.")
                },
            );
        }
        let hot = if self.media == Some(Media::Hdd) {
            60
        } else {
            75
        };
        if let Some(t) = self.temperature.filter(|&t| t >= hot) {
            say(Watch, format!("It is running hot ({t} °C)."));
        }

        if reasons.is_empty() {
            if self.has_smart {
                reasons.push(match self.smart_passed {
                    Some(true)
                        if self.reallocated_sectors.is_some()
                            || self.critical_warning.is_some() =>
                    {
                        "The drive's own health check passed, and it reports no damage.".into()
                    }
                    Some(true) => "The drive's own health check passed.".into(),
                    _ => "The drive reports no damage.".into(),
                });
            } else {
                verdict = Unknown;
                reasons.push(
                    "This drive does not report its health. Most memory cards, card readers \
                     and many USB adapters don't; a surface test shows how well it reads."
                        .into(),
                );
            }
        }
        (verdict, reasons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_diskutil_smart_status() {
        let plist = |status: &str| {
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\n\
                 <key>RemovableMedia</key>\n<false/>\n\
                 <key>SMARTStatus</key>\n\t<string>{status}</string>\n\
                 <key>SolidState</key><true/></dict></plist>"
            )
        };
        assert_eq!(
            from_diskutil_plist(&plist("Verified")).status,
            DriveStatus::Good
        );
        assert_eq!(
            from_diskutil_plist(&plist("Failing")).status,
            DriveStatus::Failing
        );
        assert_eq!(
            from_diskutil_plist(&plist("Not Supported")).status,
            DriveStatus::Unknown
        );
        assert_eq!(from_diskutil_plist("").status, DriveStatus::Unknown);
    }

    #[test]
    fn reads_smartctl_json() {
        let passed = r#"{"smartctl":{"exit_status":0},"smart_status":{"passed":true}}"#;
        let failed = r#"{"smartctl":{"exit_status":8},"smart_status":{"passed":false}}"#;
        let bridge = r#"{"smartctl":{"messages":[{"string":"/dev/sdb: Unknown USB bridge [0x14cd:0x1212]","severity":"error"}],"exit_status":1}}"#;
        assert_eq!(from_smartctl_json(passed).status, DriveStatus::Good);
        assert_eq!(from_smartctl_json(failed).status, DriveStatus::Failing);
        let unknown = from_smartctl_json(bridge);
        assert_eq!(unknown.status, DriveStatus::Unknown);
        assert!(unknown.detail.contains("Unknown USB bridge"));
        assert_eq!(from_smartctl_json("").status, DriveStatus::Unknown);
    }

    /// `smartctl -a -j` from a SATA hard drive, trimmed to what matters.
    const SATA_HDD: &str = r#"{
      "json_format_version": [1, 0],
      "smartctl": {"version": [7, 3], "exit_status": 0},
      "device": {"name": "/dev/sda", "info_name": "/dev/sda [SAT]", "type": "sat", "protocol": "ATA"},
      "model_family": "Western Digital Blue",
      "model_name": "WDC WD10EZEX-08WN4A0",
      "serial_number": "WD-WCC6Y0ABCDEF",
      "firmware_version": "01.01A01",
      "user_capacity": {"blocks": 1953525168, "bytes": 1000204886016},
      "rotation_rate": 7200,
      "sata_version": {"string": "SATA 3.1", "value": 127},
      "smart_status": {"passed": true},
      "ata_smart_attributes": {"revision": 16, "table": [
        {"id": 1, "name": "Raw_Read_Error_Rate", "value": 200, "worst": 200, "thresh": 51, "when_failed": "", "raw": {"value": 0, "string": "0"}},
        {"id": 5, "name": "Reallocated_Sector_Ct", "value": 200, "worst": 200, "thresh": 140, "when_failed": "", "raw": {"value": 0, "string": "0"}},
        {"id": 9, "name": "Power_On_Hours", "value": 62, "worst": 62, "thresh": 0, "when_failed": "", "raw": {"value": 28123, "string": "28123"}},
        {"id": 12, "name": "Power_Cycle_Count", "value": 99, "worst": 99, "thresh": 0, "when_failed": "", "raw": {"value": 1534, "string": "1534"}},
        {"id": 194, "name": "Temperature_Celsius", "value": 108, "worst": 96, "thresh": 0, "when_failed": "", "raw": {"value": 158915641386, "string": "42 (Min/Max 18/54)"}},
        {"id": 197, "name": "Current_Pending_Sector", "value": 200, "worst": 200, "thresh": 0, "when_failed": "", "raw": {"value": 0, "string": "0"}},
        {"id": 198, "name": "Offline_Uncorrectable", "value": 200, "worst": 200, "thresh": 0, "when_failed": "", "raw": {"value": 0, "string": "0"}}
      ]},
      "power_on_time": {"hours": 28123},
      "power_cycle_count": 1534,
      "temperature": {"current": 42}
    }"#;

    /// The same drive a year later, with a damaged surface.
    fn failing_hdd() -> String {
        SATA_HDD
            .replace(
                r#""id": 197, "name": "Current_Pending_Sector", "value": 200, "worst": 200, "thresh": 0, "when_failed": "", "raw": {"value": 0, "string": "0"}"#,
                r#""id": 197, "name": "Current_Pending_Sector", "value": 200, "worst": 200, "thresh": 0, "when_failed": "", "raw": {"value": 24, "string": "24"}"#,
            )
            .replace(
                r#""id": 5, "name": "Reallocated_Sector_Ct", "value": 200, "worst": 200, "thresh": 140, "when_failed": "", "raw": {"value": 0, "string": "0"}"#,
                r#""id": 5, "name": "Reallocated_Sector_Ct", "value": 139, "worst": 139, "thresh": 140, "when_failed": "now", "raw": {"value": 1830, "string": "1830"}"#,
            )
            .replace(r#""smart_status": {"passed": true}"#, r#""smart_status": {"passed": false}"#)
    }

    /// A SATA SSD (Samsung style: wear in attribute 177, and the device
    /// statistics page).
    const SATA_SSD: &str = r#"{
      "smartctl": {"version": [7, 2], "exit_status": 0},
      "device": {"name": "/dev/sdb", "info_name": "/dev/sdb [SAT]", "type": "sat", "protocol": "ATA"},
      "model_name": "Samsung SSD 860 EVO 500GB",
      "serial_number": "S3Z1NB0K123456X",
      "firmware_version": "RVT04B6Q",
      "user_capacity": {"blocks": 976773168, "bytes": 500107862016},
      "rotation_rate": 0,
      "smart_status": {"passed": true},
      "ata_smart_attributes": {"table": [
        {"id": 5, "name": "Reallocated_Sector_Ct", "value": 100, "worst": 100, "thresh": 10, "when_failed": "", "raw": {"value": 0, "string": "0"}},
        {"id": 9, "name": "Power_On_Hours", "value": 97, "worst": 97, "thresh": 0, "when_failed": "", "raw": {"value": 13071, "string": "13071"}},
        {"id": 12, "name": "Power_Cycle_Count", "value": 99, "worst": 99, "thresh": 0, "when_failed": "", "raw": {"value": 702, "string": "702"}},
        {"id": 177, "name": "Wear_Leveling_Count", "value": 93, "worst": 93, "thresh": 0, "when_failed": "", "raw": {"value": 61, "string": "61"}},
        {"id": 187, "name": "Uncorrectable_Error_Cnt", "value": 100, "worst": 100, "thresh": 0, "when_failed": "", "raw": {"value": 0, "string": "0"}},
        {"id": 190, "name": "Airflow_Temperature_Cel", "value": 67, "worst": 49, "thresh": 0, "when_failed": "", "raw": {"value": 33, "string": "33"}}
      ]},
      "ata_device_statistics": {"pages": [
        {"number": 7, "name": "Solid State Device Statistics", "table": [
          {"offset": 8, "name": "Percentage Used Endurance Indicator", "size": 1, "value": 7}
        ]}
      ]},
      "power_on_time": {"hours": 13071},
      "power_cycle_count": 702,
      "temperature": {"current": 33}
    }"#;

    const NVME: &str = r#"{
      "smartctl": {"version": [7, 4], "exit_status": 0},
      "device": {"name": "/dev/nvme0", "info_name": "/dev/nvme0", "type": "nvme", "protocol": "NVMe"},
      "model_name": "Samsung SSD 970 EVO Plus 1TB",
      "serial_number": "S4EWNX0R654321",
      "firmware_version": "2B2QEXM7",
      "nvme_total_capacity": 1000204886016,
      "smart_status": {"passed": true, "nvme": {"value": 0}},
      "nvme_smart_health_information_log": {
        "critical_warning": 0, "temperature": 38, "available_spare": 100,
        "available_spare_threshold": 10, "percentage_used": 3,
        "data_units_read": 31234567, "data_units_written": 42345678,
        "power_cycles": 1203, "power_on_hours": 9876, "unsafe_shutdowns": 61,
        "media_errors": 0, "num_err_log_entries": 2410
      },
      "temperature": {"current": 38},
      "power_cycle_count": 1203,
      "power_on_time": {"hours": 9876}
    }"#;

    /// A USB enclosure whose bridge smartctl doesn't know: no SMART.
    const USB_NO_SMART: &str = r#"{
      "smartctl": {"version": [7, 3], "exit_status": 1, "messages": [
        {"string": "/dev/sdc: Unknown USB bridge [0x0bda:0x9210 (0xf01)]", "severity": "error"},
        {"string": "Please specify device type with the -d option.", "severity": "information"}
      ]},
      "device": {"name": "/dev/sdc", "info_name": "/dev/sdc", "type": "scsi", "protocol": "SCSI"},
      "scsi_vendor": "Realtek",
      "scsi_product": "RTL9210",
      "scsi_revision": "1.00"
    }"#;

    fn report_of(json: &str) -> SmartReport {
        let mut r = parse_smartctl(json);
        r.finish();
        r
    }

    #[test]
    fn reports_a_healthy_hard_drive() {
        let r = report_of(SATA_HDD);
        assert_eq!(r.model.as_deref(), Some("WDC WD10EZEX-08WN4A0"));
        assert_eq!(r.serial.as_deref(), Some("WD-WCC6Y0ABCDEF"));
        assert_eq!(r.firmware.as_deref(), Some("01.01A01"));
        assert_eq!(r.capacity, Some(1_000_204_886_016));
        assert_eq!(r.interface.as_deref(), Some("SATA"));
        assert_eq!(r.media, Some(Media::Hdd));
        assert_eq!(r.temperature, Some(42));
        assert_eq!(r.power_on_hours, Some(28123));
        assert_eq!(r.power_cycles, Some(1534));
        assert_eq!(r.reallocated_sectors, Some(0));
        assert_eq!(r.pending_sectors, Some(0));
        assert_eq!(r.uncorrectable_sectors, Some(0));
        assert_eq!(r.percentage_used, None);
        assert_eq!(r.verdict, Verdict::Good);
        assert_eq!(r.reasons.len(), 1);
    }

    #[test]
    fn reports_a_failing_hard_drive() {
        let r = report_of(&failing_hdd());
        assert_eq!(r.verdict, Verdict::Failing);
        assert_eq!(r.pending_sectors, Some(24));
        assert_eq!(r.reallocated_sectors, Some(1830));
        let all = r.reasons.join(" ");
        assert!(all.contains("health check says it is failing"), "{all}");
        assert!(all.contains("24 sectors could not be read"), "{all}");
        assert!(all.contains("Reallocated Sector Ct"), "{all}");

        // A handful of replaced sectors alone is a reason to watch it.
        let few = SATA_HDD.replace(
            r#""raw": {"value": 0, "string": "0"}},
        {"id": 9"#,
            r#""raw": {"value": 3, "string": "3"}},
        {"id": 9"#,
        );
        let r = report_of(&few);
        assert_eq!(r.reallocated_sectors, Some(3));
        assert_eq!(r.verdict, Verdict::Watch);
    }

    #[test]
    fn reports_a_sata_ssd() {
        let r = report_of(SATA_SSD);
        assert_eq!(r.media, Some(Media::Ssd));
        assert_eq!(r.interface.as_deref(), Some("SATA"));
        // The device statistics win over the vendor attribute (100 - 93).
        assert_eq!(r.percentage_used, Some(7));
        assert_eq!(r.uncorrectable_sectors, Some(0));
        assert_eq!(r.temperature, Some(33));
        assert_eq!(r.verdict, Verdict::Good);

        // Without the statistics page, the wear attribute is used.
        let mut v: Value = serde_json::from_str(SATA_SSD).unwrap();
        v.as_object_mut().unwrap().remove("ata_device_statistics");
        let r = report_of(&v.to_string());
        assert_eq!(r.percentage_used, Some(7));
    }

    #[test]
    fn reports_an_nvme_ssd() {
        let r = report_of(NVME);
        assert_eq!(r.model.as_deref(), Some("Samsung SSD 970 EVO Plus 1TB"));
        assert_eq!(r.capacity, Some(1_000_204_886_016));
        assert_eq!(r.interface.as_deref(), Some("NVMe"));
        assert_eq!(r.media, Some(Media::Ssd));
        assert_eq!(r.percentage_used, Some(3));
        assert_eq!(r.available_spare, Some(100));
        assert_eq!(r.available_spare_threshold, Some(10));
        assert_eq!(r.media_errors, Some(0));
        assert_eq!(r.power_on_hours, Some(9876));
        assert_eq!(r.power_cycles, Some(1203));
        assert_eq!(r.temperature, Some(38));
        assert_eq!(r.verdict, Verdict::Good);

        // Worn out, with the spare gone: failing.
        let worn = NVME
            .replace(r#""critical_warning": 0"#, r#""critical_warning": 1"#)
            .replace(r#""available_spare": 100"#, r#""available_spare": 4"#)
            .replace(r#""percentage_used": 3"#, r#""percentage_used": 112"#)
            .replace(r#""media_errors": 0"#, r#""media_errors": 17"#);
        let r = report_of(&worn);
        assert_eq!(r.verdict, Verdict::Failing);
        let all = r.reasons.join(" ");
        assert!(all.contains("spare blocks"), "{all}");
        assert!(all.contains("112% used"), "{all}");
        assert!(all.contains("17 data errors"), "{all}");
    }

    #[test]
    fn reports_a_usb_drive_without_smart() {
        let r = report_of(USB_NO_SMART);
        assert_eq!(r.verdict, Verdict::Unknown);
        assert_eq!(r.model.as_deref(), Some("Realtek RTL9210"));
        assert_eq!(r.firmware.as_deref(), Some("1.00"));
        assert_eq!(r.interface.as_deref(), Some("USB"));
        assert!(r.notes.iter().any(|n| n.contains("Unknown USB bridge")));
        assert!(r.reasons[0].contains("does not report its health"));

        // With SAT pass-through the same enclosure answers, and the system
        // knows it is on USB.
        let mut sat = parse_smartctl(SATA_SSD);
        sat.usb = true;
        sat.fill_from(parse_smartctl(USB_NO_SMART));
        sat.finish();
        assert_eq!(sat.interface.as_deref(), Some("USB (SATA inside)"));
        assert_eq!(sat.model.as_deref(), Some("Samsung SSD 860 EVO 500GB"));
        assert_eq!(sat.verdict, Verdict::Good);
    }

    #[test]
    fn survives_odd_smartctl_output() {
        for odd in [
            "",
            "null",
            "[]",
            "42",
            "{",
            r#"{"model_name": 7, "user_capacity": {"bytes": -5}, "rotation_rate": "fast"}"#,
            r#"{"temperature": {"current": 99999}, "power_on_time": {"hours": -1}}"#,
            r#"{"ata_smart_attributes": {"table": [{"id": "x"}, {}, 5, {"id": 5, "raw": {"value": 1e300}}]}}"#,
            r#"{"ata_smart_attributes": {"table": {"id": 5}}, "nvme_smart_health_information_log": []}"#,
            r#"{"nvme_smart_health_information_log": {"percentage_used": 1.5e40, "available_spare": "lots"}}"#,
            r#"{"smartctl": {"messages": [{"string": "\u0007\u0000bad\nline", "severity": "error"}, null, 3]}}"#,
            r#"{"ata_device_statistics": {"pages": [null, {"table": [{"name": "Percentage Used Endurance Indicator", "value": 1e30}]}]}}"#,
        ] {
            let r = report_of(odd);
            assert_eq!(r.capacity, None, "{odd}");
            assert_eq!(r.temperature, None, "{odd}");
            assert_eq!(r.power_on_hours, None, "{odd}");
            assert!(r.percentage_used.is_none_or(|p| p <= 255), "{odd}");
            assert!(r.notes.iter().all(|n| !n.chars().any(char::is_control)));
        }
        // Raw values with junk packed into the high bytes are not counts.
        let r = report_of(
            r#"{"ata_smart_attributes": {"table": [{"id": 5, "raw": {"value": 281470681743360}}]}}"#,
        );
        assert_eq!(r.reallocated_sectors, None);
        // Hours as "h+m+s" text.
        let r = report_of(
            r#"{"ata_smart_attributes": {"table": [{"id": 9, "raw": {"value": 99999999, "string": "1234h+05m+03.000s"}}]}}"#,
        );
        assert_eq!(r.power_on_hours, Some(1234));
        // Deep nesting is refused, not overflowed.
        let deep = "[".repeat(100_000);
        assert_eq!(report_of(&deep).verdict, Verdict::Unknown);
    }

    #[test]
    fn reads_the_diskutil_report() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
	<key>BusProtocol</key>
	<string>USB</string>
	<key>DeviceNode</key>
	<string>/dev/disk4</string>
	<key>MediaName</key>
	<string>Samsung PSSD T7 Media</string>
	<key>SMARTStatus</key>
	<string>Not Supported</string>
	<key>SolidState</key>
	<true/>
	<key>TotalSize</key>
	<integer>1000204886016</integer>
</dict></plist>"#;
        let mut r = parse_diskutil(xml);
        r.finish();
        assert_eq!(r.model.as_deref(), Some("Samsung PSSD T7 Media"));
        assert_eq!(r.capacity, Some(1_000_204_886_016));
        assert_eq!(r.media, Some(Media::Ssd));
        assert_eq!(r.interface.as_deref(), Some("USB"));
        assert_eq!(r.verdict, Verdict::Unknown);

        let internal = r#"<plist><dict>
	<key>BusProtocol</key><string>Apple Fabric</string>
	<key>SMARTDeviceSpecificKeysMayVaryNotGuaranteed</key>
	<dict>
		<key>AVAILABLE_SPARE</key><integer>100</integer>
		<key>AVAILABLE_SPARE_THRESHOLD</key><integer>99</integer>
		<key>CRITICAL_WARNING</key><integer>0</integer>
		<key>MEDIA_ERRORS_0</key><integer>0</integer>
		<key>PERCENTAGE_USED</key><integer>2</integer>
		<key>POWER_CYCLES_0</key><integer>312</integer>
		<key>POWER_ON_HOURS_0</key><integer>1460</integer>
		<key>TEMPERATURE</key><integer>309</integer>
	</dict>
	<key>SMARTStatus</key><string>Verified</string>
	<key>SolidState</key><true/>
</dict></plist>"#;
        let mut r = parse_diskutil(internal);
        r.finish();
        assert_eq!(r.percentage_used, Some(2));
        assert_eq!(r.power_on_hours, Some(1460));
        assert_eq!(r.temperature, Some(36));
        assert_eq!(r.interface.as_deref(), Some("Built in (Apple Fabric)"));
        assert_eq!(r.verdict, Verdict::Good);

        let mut failing = parse_diskutil(&internal.replace("Verified", "Failing"));
        failing.finish();
        assert_eq!(failing.verdict, Verdict::Failing);
        assert_eq!(parse_diskutil("<not xml"), SmartReport::default());
    }

    /// Runs on whatever drive this machine has; only checks that asking
    /// never fails or hangs.
    #[test]
    fn reports_on_a_missing_device() {
        let r = report(Path::new("/dev/reperio-no-such-disk"));
        assert!(!r.reasons.is_empty());
    }
}

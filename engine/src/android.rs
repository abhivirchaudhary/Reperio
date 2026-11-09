//! Android phones over USB with `adb` (USB debugging on). Nothing is ever
//! written to the phone.
//!
//! - [`pull`] needs no root. It copies what Android keeps after a delete:
//!   the trash (`.trashed-*` files, kept 30 days on Android 11 and later,
//!   and gallery apps' trash folders) and thumbnail caches, and puts
//!   trashed files back under their own names.
//! - [`image`] needs a rooted phone. It streams a partition to a file,
//!   hashing it as it goes. On a phone with encryption (nearly all since
//!   2016) the image is unreadable; [`image`] refuses unless told otherwise.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::forensic::{self, Algorithms, Hasher};

/// The phone's shared storage as `adb` sees it.
const SHARED: &str = "/sdcard";

pub const ADB_HELP: &str = "adb (Android platform-tools) was not found. Install it (Mac: `brew install android-platform-tools`; Linux: your package manager's `adb`). On the phone: Settings > About phone > tap Build number 7 times, then Settings > System > Developer options > USB debugging.";

/// Where `adb` is: `$ADB`, then the usual folders (an app started from the
/// Finder does not have Homebrew on its PATH), then the PATH.
pub fn find_adb() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("ADB") {
        return Ok(PathBuf::from(p));
    }
    let mut places: Vec<PathBuf> = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(p) = std::env::var_os(var) {
            places.push(PathBuf::from(p).join("platform-tools/adb"));
        }
    }
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from) {
        places.push(h.join("Library/Android/sdk/platform-tools/adb"));
        places.push(h.join("Android/Sdk/platform-tools/adb"));
        places.push(h.join("platform-tools/adb"));
    }
    places.extend(
        [
            "/opt/homebrew/bin/adb",
            "/usr/local/bin/adb",
            "/usr/bin/adb",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    if let Some(found) = places.into_iter().find(|p| p.exists()) {
        return Ok(found);
    }
    if Command::new("adb").arg("version").output().is_ok() {
        return Ok("adb".into());
    }
    Err(ADB_HELP.to_string())
}

/// A phone `adb` can see.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// `device` (ready), `unauthorized` or `offline`.
    pub state: String,
}

pub fn devices(bin: &Path) -> Result<Vec<Device>, String> {
    let out = Command::new(bin)
        .arg("devices")
        .output()
        .map_err(|e| format!("Could not run adb: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(Device {
                serial: it.next()?.to_string(),
                state: it.next()?.to_string(),
            })
        })
        .collect())
}

/// Why a phone in this state cannot be used yet.
pub fn state_problem(state: &str) -> Option<String> {
    match state {
        "device" => None,
        "unauthorized" => Some("The phone has not allowed this computer. Unlock it and tap Allow on the \"Allow USB debugging?\" message, then try again.".into()),
        "offline" => Some("The phone is offline. Unplug it, plug it in again, and unlock it.".into()),
        other => Some(format!("The phone is \"{other}\"; it needs to be on and unlocked.")),
    }
}

pub struct Adb {
    bin: PathBuf,
    pub serial: String,
}

impl Adb {
    /// The phone with this serial, or the only one connected.
    pub fn connect(device: Option<&str>) -> Result<Self, String> {
        let bin = find_adb()?;
        let all = devices(&bin)?;
        let chosen = match device {
            Some(s) => all
                .iter()
                .find(|d| d.serial == s)
                .ok_or_else(|| format!("{s} is not connected (see `adb devices`)"))?,
            None => match all.as_slice() {
                [] => return Err("No phone found. Connect it with a USB cable, turn on USB debugging, and unlock it.".into()),
                [one] => one,
                many => {
                    return Err(format!(
                        "More than one phone is connected ({}). Choose one.",
                        many.iter().map(|d| d.serial.as_str()).collect::<Vec<_>>().join(", ")
                    ))
                }
            },
        };
        if let Some(problem) = state_problem(&chosen.state) {
            return Err(problem);
        }
        Ok(Self {
            bin,
            serial: chosen.serial.clone(),
        })
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.arg("-s").arg(&self.serial);
        c
    }

    /// Output of a command run on the phone.
    pub fn shell(&self, script: &str) -> Result<String, String> {
        let out = self
            .command()
            .arg("shell")
            .arg(script)
            .output()
            .map_err(|e| format!("Could not run adb: {e}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Runs `adb -s <phone> <args>` and returns what it printed.
    pub fn run(&self, args: &[String]) -> Result<String, String> {
        let out = self
            .command()
            .args(args)
            .output()
            .map_err(|e| format!("Could not run adb: {e}"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            text.push_str(&err);
        }
        if out.status.success() {
            Ok(text)
        } else {
            Err(text.trim().to_string())
        }
    }

    /// Copies one file from the phone, keeping its times.
    pub fn pull_file(&self, remote: &str, local: &Path) -> Result<(), String> {
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let out = self
            .command()
            .args(["pull", "-a", remote])
            .arg(local)
            .output()
            .map_err(|e| format!("Could not run adb: {e}"))?;
        if out.status.success() && local.exists() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn prop(&self, name: &str) -> String {
        self.shell(&format!("getprop {name}"))
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn root(&self) -> bool {
        self.shell("su -c id 2>/dev/null")
            .map(|o| o.contains("uid=0"))
            .unwrap_or(false)
    }
}

/// What a phone is, and so what can be done with it.
#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    pub serial: String,
    pub name: String,
    pub android: String,
    pub patch: String,
    pub encrypted: bool,
    pub crypto_type: String,
    pub root: bool,
    /// Size of the `userdata` partition, when the phone is rooted.
    pub userdata_size: Option<u64>,
    /// The phone's storage chip (the biggest block device), when rooted.
    pub storage: Option<Block>,
    /// Every partition and its size, when rooted.
    pub partitions: Vec<Block>,
}

/// A partition or a whole storage chip.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Block {
    pub name: String,
    pub size: u64,
}

pub fn facts(adb: &Adb) -> Facts {
    let state = adb.prop("ro.crypto.state");
    let root = adb.root();
    let mut f = Facts {
        serial: adb.serial.clone(),
        name: format!(
            "{} {}",
            adb.prop("ro.product.manufacturer"),
            adb.prop("ro.product.model")
        )
        .trim()
        .to_string(),
        android: adb.prop("ro.build.version.release"),
        patch: adb.prop("ro.build.version.security_patch"),
        // An empty answer counts as encrypted: nearly all phones are.
        encrypted: state != "unencrypted",
        crypto_type: adb.prop("ro.crypto.type"),
        root,
        userdata_size: None,
        storage: None,
        partitions: Vec::new(),
    };
    if f.root {
        f.partitions = partitions(adb);
        f.userdata_size = f
            .partitions
            .iter()
            .find(|p| p.name == "userdata")
            .map(|p| p.size);
        f.storage = storage_chip(adb);
    }
    f
}

/// Plain words on what can be done with this phone.
pub fn verdict(f: &Facts) -> String {
    let mut s = if f.root && !f.encrypted {
        "This phone's storage can be imaged and scanned for deleted files.".to_string()
    } else if f.root {
        "You can image this phone, but its storage is encrypted with keys kept inside the phone, so the image would hold nothing readable.".to_string()
    } else {
        "Without root, the phone only gives up its normal files. Copying what Android keeps after a delete (the trash and thumbnails) works.".to_string()
    };
    if f.encrypted {
        s.push_str(
            " Deleted files cannot be recovered from encrypted storage, whatever the software.",
        );
    }
    s
}

fn valid_partition(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Parses `name size` lines.
fn blocks(text: &str) -> Vec<Block> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (name, size) = (it.next()?, it.next()?.parse().ok()?);
            (valid_partition(name) && size > 0).then(|| Block {
                name: name.to_string(),
                size,
            })
        })
        .collect()
}

/// The partitions of a rooted phone, with sizes (one call to the phone).
pub fn partitions(adb: &Adb) -> Vec<Block> {
    let script = "su -c 'for p in /dev/block/by-name/*; do echo \"$(basename $p) $(blockdev --getsize64 $p)\"; done' 2>/dev/null";
    blocks(&adb.shell(script).unwrap_or_default())
}

/// The phone's storage chip: the biggest of its whole-disk block devices
/// (`sda`, `mmcblk0`, `nvme0n1`).
pub fn storage_chip(adb: &Adb) -> Option<Block> {
    let script = "su -c 'for d in /sys/block/*; do n=$(basename $d); case $n in sd[a-z]|mmcblk[0-9]|nvme[0-9]n[0-9]) echo \"$n $(blockdev --getsize64 /dev/block/$n)\";; esac; done' 2>/dev/null";
    blocks(&adb.shell(script).unwrap_or_default())
        .into_iter()
        .max_by_key(|b| b.size)
}

/// What to image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One partition, by name under `/dev/block/by-name`.
    Partition(String),
    /// A whole block device, by name under `/dev/block`.
    Device(String),
}

impl Target {
    fn path(&self) -> Option<String> {
        match self {
            Target::Partition(n) if valid_partition(n) => Some(format!("/dev/block/by-name/{n}")),
            Target::Device(n) if valid_partition(n) => Some(format!("/dev/block/{n}")),
            _ => None,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Target::Partition(n) | Target::Device(n) => n,
        }
    }
}

fn target_size(adb: &Adb, t: &Target) -> Option<u64> {
    let path = t.path()?;
    adb.shell(&format!("su -c 'blockdev --getsize64 {path}' 2>/dev/null"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A command for `su -c`, in single quotes the phone's shell takes as one
/// word, however many quotes and `$` it holds.
fn su_wrap(cmd: &str) -> String {
    format!("su -c '{}' 2>&1", cmd.replace('\'', "'\\''"))
}

/// The phone as a forensic plug-in may use it: through [`Device`], which
/// the analyzer module only calls for commands it has checked.
pub struct Phone<'a> {
    pub adb: &'a Adb,
    pub facts: Facts,
}

impl crate::analyzer::Device for Phone<'_> {
    fn facts(&self) -> serde_json::Value {
        serde_json::to_value(&self.facts).unwrap_or_default()
    }

    fn shell(&self, cmd: &str, root: bool) -> Result<String, String> {
        if root {
            self.adb.shell(&su_wrap(cmd))
        } else {
            self.adb.shell(cmd)
        }
    }

    fn pull(&self, remote: &str, local: &Path) -> Result<(), String> {
        self.adb.pull_file(remote, local)
    }

    fn adb(&self, args: &[String]) -> Result<String, String> {
        self.adb.run(args)
    }

    fn adb_path(&self) -> Option<String> {
        Some(self.adb.bin.display().to_string())
    }
}

// ---------------------------------------------------------------------------
// pull
// ---------------------------------------------------------------------------

/// The deletion's expiry and the name a trashed file had, from
/// `.trashed-1700000000-IMG_0001.jpg`. The number is when Android will
/// remove it for good.
pub fn trashed_name(file: &str) -> Option<(u64, &str)> {
    let rest = file.strip_prefix(".trashed-")?;
    let (stamp, name) = rest.split_once('-')?;
    (!name.is_empty()).then_some((stamp.parse().ok()?, name))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeftBehind {
    Trash,
    Thumbnail,
    GalleryTrash,
}

fn kind_of(remote: &str) -> LeftBehind {
    let name = remote.rsplit('/').next().unwrap_or("");
    if trashed_name(name).is_some() {
        LeftBehind::Trash
    } else if remote.contains("/.thumbnails/") {
        LeftBehind::Thumbnail
    } else {
        LeftBehind::GalleryTrash
    }
}

/// How far a pull has got.
pub struct PullProgress<'a> {
    pub done: usize,
    pub total: usize,
    pub name: &'a str,
}

/// Writes `sha256  size  path` for every file under `root`. Returns the
/// number of files and bytes.
fn write_manifest(
    root: &Path,
    manifest: &Path,
    cancel: &AtomicBool,
) -> std::io::Result<(u64, u64)> {
    let mut list = String::from("# SHA-256  size  path (relative to the copy)\n");
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut stack = vec![root.to_path_buf()];
    let mut all = Vec::new();
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir)?.flatten() {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => all.push(path),
                _ => {}
            }
        }
    }
    all.sort();
    for path in all {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(mut f) = File::open(&path) else {
            continue;
        };
        let mut h = Hasher::new(Algorithms::parse("sha256").expect("known hash"));
        let mut buf = vec![0u8; 1 << 20];
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            size += n as u64;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        list += &format!("{}  {size}  {rel}\n", h.finish().sha256.unwrap_or_default());
        files += 1;
        bytes += size;
    }
    fs::write(manifest, list)?;
    Ok((files, bytes))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Pulled {
    /// Trashed files put back under their own names.
    pub restored: Vec<String>,
    pub restored_dir: String,
    /// Where the rest (gallery trash, thumbnails) was copied.
    pub left_dir: String,
    pub trash: usize,
    pub thumbnails: usize,
    pub gallery_trash: usize,
    pub failed: usize,
    /// When Android removes the oldest trashed file for good (UTC text).
    pub expires: Option<String>,
    /// Where all of the shared storage was copied, if asked.
    pub everything_dir: Option<String>,
    /// Files and bytes copied with it, and the list of their SHA-256 hashes.
    pub everything_files: u64,
    pub everything_bytes: u64,
    pub manifest: Option<String>,
    pub cancelled: bool,
}

pub fn pull(
    adb: &Adb,
    dest: &Path,
    everything: bool,
    progress: &mut dyn FnMut(PullProgress),
    cancel: &AtomicBool,
) -> Result<Pulled, String> {
    let io = |e: std::io::Error| e.to_string();
    fs::create_dir_all(dest).map_err(io)?;
    let mut out = Pulled::default();

    if everything {
        let target = dest.join("phone-storage");
        progress(PullProgress {
            done: 0,
            total: 1,
            name: "Everything in shared storage",
        });
        // Some files may be unreadable; that is not an error.
        let _ = adb
            .command()
            .args([
                "pull",
                "-a",
                &format!("{SHARED}/."),
                &target.display().to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        out.everything_dir = Some(target.display().to_string());
        progress(PullProgress {
            done: 0,
            total: 1,
            name: "Fingerprinting the copied files",
        });
        let manifest = dest.join("MANIFEST.sha256.txt");
        if let Ok((files, bytes)) = write_manifest(&target, &manifest, cancel) {
            out.everything_files = files;
            out.everything_bytes = bytes;
            out.manifest = Some(manifest.display().to_string());
        }
    }

    let find = format!(
        "find {SHARED} -type f \\( -name '.trashed-*' -o -path '*/.thumbnails/*' -o -path '*/.Trash/*' -o -path '*/.trash/*' -o -path '*/.RecycleBin/*' -o -path '*/.recycle/*' \\) 2>/dev/null"
    );
    let listing = adb.shell(&find)?;
    let remotes: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let left = dest.join("left-behind");
    let back = dest.join("Recovered from trash");
    out.left_dir = left.display().to_string();
    out.restored_dir = back.display().to_string();
    let mut taken = BTreeSet::new();
    let mut soonest: Option<u64> = None;
    for (i, remote) in remotes.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        let file = remote.rsplit('/').next().unwrap_or("");
        progress(PullProgress {
            done: i,
            total: remotes.len(),
            name: file,
        });
        let rel = remote.trim_start_matches(SHARED).trim_start_matches('/');
        let local = left.join(rel);
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        let copied = adb
            .command()
            .args(["pull", "-a", remote, &local.display().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !copied {
            out.failed += 1;
            continue;
        }
        match kind_of(remote) {
            LeftBehind::Trash => {
                out.trash += 1;
                if let Some((expires, name)) = trashed_name(file) {
                    soonest = Some(soonest.map_or(expires, |s| s.min(expires)));
                    fs::create_dir_all(&back).map_err(io)?;
                    // The same name from different folders gets a number.
                    let mut target = back.join(name);
                    let mut n = 1;
                    while !taken.insert(target.clone()) || target.exists() {
                        n += 1;
                        target = back.join(format!("{n}-{name}"));
                    }
                    fs::copy(&local, &target).map_err(io)?;
                    if let Ok(mtime) = fs::metadata(&local).and_then(|m| m.modified()) {
                        let _ = File::options()
                            .write(true)
                            .open(&target)
                            .and_then(|f| f.set_modified(mtime));
                    }
                    out.restored.push(
                        target
                            .file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into()),
                    );
                }
            }
            LeftBehind::Thumbnail => out.thumbnails += 1,
            LeftBehind::GalleryTrash => out.gallery_trash += 1,
        }
    }
    out.expires = soonest.map(|t| {
        forensic::utc(UNIX_EPOCH + Duration::from_secs(t))
            .replace('T', " ")
            .trim_end_matches('Z')
            .to_string()
    });
    Ok(out)
}

// ---------------------------------------------------------------------------
// image
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Imaged {
    pub path: String,
    pub report: String,
    pub size: u64,
    pub received: u64,
    pub md5: String,
    pub sha256: String,
    /// All `size` bytes arrived.
    pub complete: bool,
    pub cancelled: bool,
}

/// Streams the partition `name` of a rooted phone to `dest`, hashing it.
/// `progress` gets bytes done and the partition's size. Cancelled, the
/// partial file is removed.
pub fn image(
    adb: &Adb,
    facts: &Facts,
    dest: &Path,
    target: &Target,
    even_if_encrypted: bool,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Imaged, String> {
    let name = target.label();
    let Some(path) = target.path() else {
        return Err(format!("\"{name}\" is not a partition name"));
    };
    if !facts.root {
        return Err("Imaging a partition needs a rooted phone. This phone is not rooted. Copy what it keeps after a delete instead.".into());
    }
    if facts.encrypted && !even_if_encrypted {
        return Err("This phone's storage is encrypted, so the image would be unreadable and a scan of it would find nothing. Nothing was copied.".into());
    }
    if dest.exists() {
        return Err(format!(
            "{} already exists; choose a new file name",
            dest.display()
        ));
    }
    let size =
        target_size(adb, target).ok_or_else(|| format!("{name} was not found on the phone."))?;

    let mut child = adb
        .command()
        .arg("exec-out")
        .arg(format!(
            "su -c 'dd if={path} bs=1048576 conv=noerror,sync 2>/dev/null'"
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not run adb: {e}"))?;
    let mut input = child.stdout.take().ok_or("No output from adb")?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    let mut hasher = Hasher::new(Algorithms::parse("md5,sha256").expect("known hashes"));
    let started = SystemTime::now();
    let clock = Instant::now();
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut last) = (0u64, Instant::now());
    let mut cancelled = false;
    progress(0, size);
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            let _ = child.kill();
            break;
        }
        let n = input.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        // dd pads to a whole block; the partition is exactly `size` bytes.
        let n = n.min(size.saturating_sub(done) as usize);
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if last.elapsed() > Duration::from_millis(400) {
            progress(done, size);
            last = Instant::now();
        }
    }
    // Everything has arrived (dd may still be padding its last block), or the
    // copy was stopped: do not wait for a child nobody reads from.
    let _ = child.kill();
    let _ = child.wait();
    if cancelled {
        drop(out);
        let _ = fs::remove_file(dest);
        return Ok(Imaged {
            path: dest.display().to_string(),
            report: String::new(),
            size,
            received: done,
            md5: String::new(),
            sha256: String::new(),
            complete: false,
            cancelled: true,
        });
    }
    out.sync_all().map_err(|e| e.to_string())?;
    progress(done, size);
    let hashes = hasher.finish();
    let (md5, sha256) = (
        hashes.md5.clone().unwrap_or_default(),
        hashes.sha256.clone().unwrap_or_default(),
    );

    let report = PathBuf::from(format!("{}.hashes.txt", dest.display()));
    let text = format!(
        "Reperio Android image\nPhone: {}\nAndroid: {}\nSerial: {}\nImaged: {} {name}\nSize: {size} bytes (received {done})\nStarted: {}\nTook: {} s\nEncrypted: {}\nMD5: {md5}\nSHA-256: {sha256}\n",
        facts.name,
        facts.android,
        facts.serial,
        if matches!(target, Target::Device(_)) { "whole storage chip" } else { "partition" },
        forensic::utc(started),
        clock.elapsed().as_secs(),
        facts.encrypted,
    );
    fs::write(&report, text).map_err(|e| e.to_string())?;
    Ok(Imaged {
        path: dest.display().to_string(),
        report: report.display().to_string(),
        size,
        received: done,
        md5,
        sha256,
        complete: done == size,
        cancelled: false,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct AllImaged {
    pub dir: String,
    pub manifest: String,
    /// Partitions imaged completely.
    pub done: usize,
    /// Partitions that could not be read or arrived incomplete.
    pub failed: Vec<String>,
    pub bytes: u64,
    pub cancelled: bool,
}

/// Images every partition of a rooted phone into its own file in the new
/// folder `dir`, with a manifest of sizes and hashes. `progress` gets the
/// bytes done and the total over all partitions, and the partition's name.
pub fn image_all(
    adb: &Adb,
    facts: &Facts,
    dir: &Path,
    even_if_encrypted: bool,
    progress: &mut dyn FnMut(u64, u64, &str),
    cancel: &AtomicBool,
) -> Result<AllImaged, String> {
    if !facts.root {
        return Err("Imaging needs a rooted phone. This phone is not rooted.".into());
    }
    if facts.encrypted && !even_if_encrypted {
        return Err("This phone's storage is encrypted, so the images would be unreadable. Nothing was copied.".into());
    }
    if dir.exists() {
        return Err(format!(
            "{} already exists; choose a new folder name",
            dir.display()
        ));
    }
    let list = &facts.partitions;
    if list.is_empty() {
        return Err("No partitions were listed by the phone.".into());
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let total: u64 = list.iter().map(|p| p.size).sum();
    let mut manifest = format!(
        "Reperio Android images\nPhone: {}\nAndroid: {}\nSerial: {}\nEncrypted: {}\n\nname  size  received  MD5  SHA-256\n",
        facts.name, facts.android, facts.serial, facts.encrypted
    );
    let mut out = AllImaged {
        dir: dir.display().to_string(),
        manifest: dir.join("MANIFEST.txt").display().to_string(),
        done: 0,
        failed: Vec::new(),
        bytes: 0,
        cancelled: false,
    };
    let mut base = 0u64;
    for p in list {
        let target = Target::Partition(p.name.clone());
        let file = dir.join(format!("{}.img", p.name));
        let start = base;
        let r = image(
            adb,
            facts,
            &file,
            &target,
            even_if_encrypted,
            &mut |done, _| {
                progress(start + done, total, &p.name);
            },
            cancel,
        );
        match r {
            Ok(i) if i.cancelled => {
                out.cancelled = true;
                break;
            }
            Ok(i) => {
                manifest += &format!(
                    "{}  {}  {}  {}  {}\n",
                    p.name, i.size, i.received, i.md5, i.sha256
                );
                if i.complete {
                    out.done += 1;
                } else {
                    out.failed.push(p.name.clone());
                }
                out.bytes += i.received;
                // The per-partition report is folded into the manifest.
                let _ = fs::remove_file(&i.report);
            }
            Err(_) => {
                out.failed.push(p.name.clone());
                manifest += &format!("{}  {}  0  (could not be read)\n", p.name, p.size);
            }
        }
        base += p.size;
    }
    fs::write(dir.join("MANIFEST.txt"), manifest).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_trashed_names() {
        assert_eq!(
            trashed_name(".trashed-1700000000-IMG_0001.jpg"),
            Some((1_700_000_000, "IMG_0001.jpg"))
        );
        assert_eq!(
            trashed_name(".trashed-1700000000-a-b.jpg"),
            Some((1_700_000_000, "a-b.jpg"))
        );
        assert_eq!(trashed_name("IMG.jpg"), None);
        assert_eq!(trashed_name(".trashed-x-IMG.jpg"), None);
        assert_eq!(trashed_name(".trashed-12-"), None);
    }

    #[test]
    fn block_lists_are_parsed() {
        let b = blocks("boot 67108864\nuserdata 1000\nbad;name 5\nzero 0\nodd");
        assert_eq!(b.len(), 2);
        assert_eq!(
            b[0],
            Block {
                name: "boot".into(),
                size: 67_108_864
            }
        );
        assert_eq!(
            Target::Device("sda".into()).path().as_deref(),
            Some("/dev/block/sda")
        );
        assert_eq!(Target::Partition("a b".into()).path(), None);
    }

    #[test]
    fn root_commands_keep_their_quotes() {
        assert_eq!(su_wrap("id"), "su -c 'id' 2>&1");
        assert_eq!(
            su_wrap("echo it's $HOME; ls"),
            "su -c 'echo it'\\''s $HOME; ls' 2>&1"
        );
    }

    #[test]
    fn partition_names_are_checked() {
        assert!(valid_partition("userdata"));
        assert!(!valid_partition("a b"));
        assert!(!valid_partition("x;rm"));
        assert!(!valid_partition(""));
    }
}

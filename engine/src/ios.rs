//! iPhones and iPads: detect them, back them up, and recover from backups.
//!
//! An iPhone's storage is not a disk Reperio can read: iOS does not expose a
//! block device, and the flash is hardware-encrypted. What it does offer, to a
//! computer the phone trusts, is a *backup*: a copy of its files, its photo
//! library and its message and contact databases. Reperio makes that backup
//! (with the open-source `libimobiledevice` tools: `brew install
//! libimobiledevice`), or opens one that Finder or iTunes already made, and
//! then works inside it: browse every file, list the photos (including those
//! in *Recently Deleted*), read messages (including ones recently deleted, and
//! fragments left in the message database's free pages) and export what you
//! want.
//!
//! Nothing here writes to the phone. Backups are read-only to Reperio too:
//! databases are copied to a scratch folder before they are opened.
//!
//! Encrypted backups cannot be read without their password, and Reperio does
//! not guess it.

use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use rusqlite::{Connection, OpenFlags};

use crate::sqlite_carve::{self, Carved, Table};
use serde::Serialize;

/// Seconds between 1970 and 2001, the epoch of Apple's Core Data dates.
const APPLE_EPOCH: i64 = 978_307_200;

// ---------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------

/// Where the `libimobiledevice` programs are looked for.
#[derive(Debug, Clone, Default)]
pub struct Tools {
    /// Looked in first, before `$REPERIO_IOS_TOOLS`, the PATH and Homebrew.
    pub dir: Option<PathBuf>,
}

/// The programs Reperio uses, and what each is for.
pub const NEEDED: [(&str, &str); 4] = [
    ("idevice_id", "finds connected devices"),
    ("ideviceinfo", "reads what the device is"),
    ("idevicepair", "trusts this computer"),
    ("idevicebackup2", "makes the backup"),
];

impl Tools {
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        dirs.extend(self.dir.clone());
        dirs.extend(std::env::var_os("REPERIO_IOS_TOOLS").map(PathBuf::from));
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
        dirs.iter().map(|d| d.join(name)).find(|p| p.is_file())
    }

    /// The programs that are not installed.
    pub fn missing(&self) -> Vec<&'static str> {
        NEEDED
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| self.find(n).is_none())
            .collect()
    }

    fn command(&self, name: &str) -> Result<Command, String> {
        let path = self.find(name).ok_or_else(|| {
            format!("`{name}` is not installed. In Terminal, run: brew install libimobiledevice")
        })?;
        Ok(Command::new(path))
    }

    /// Runs a tool to the end and returns its output (stdout, then stderr).
    fn run(&self, name: &str, args: &[&str]) -> Result<(bool, String), String> {
        let out = self
            .command(name)?
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("`{name}` could not be run: {e}"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), text))
    }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// A connected iPhone, iPad or iPod.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Device {
    pub udid: String,
    pub name: String,
    /// `iPhone15,2` and the like.
    pub model: String,
    pub ios: String,
    pub serial: String,
    /// Whether this computer is trusted by the device. Nothing else can be
    /// read until it is.
    pub paired: bool,
    /// Why it could not be read, when it could not.
    pub problem: Option<String>,
    pub storage_total: Option<u64>,
    pub storage_free: Option<u64>,
    pub battery: Option<u8>,
    /// Whether the device encrypts its backups. Reperio can only read
    /// unencrypted ones.
    pub encrypts_backups: Option<bool>,
}

/// `Key: Value` lines, as `ideviceinfo` prints them.
fn key_values(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Every device that is connected now.
pub fn devices(tools: &Tools) -> Result<Vec<Device>, String> {
    let (_, listed) = tools.run("idevice_id", &["-l"])?;
    let mut out = Vec::new();
    for udid in listed
        .lines()
        .map(str::trim)
        .filter(|l| l.len() >= 20 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        out.push(device(tools, udid));
    }
    Ok(out)
}

/// What one device is. A device that has not trusted this computer comes
/// back with `paired: false` and the reason.
pub fn device(tools: &Tools, udid: &str) -> Device {
    let mut d = Device {
        udid: udid.to_string(),
        name: "iPhone".into(),
        ..Device::default()
    };
    match tools.run("ideviceinfo", &["-u", udid]) {
        Ok((true, text)) => {
            let kv = key_values(&text);
            d.paired = true;
            d.name = kv.get("DeviceName").cloned().unwrap_or(d.name);
            d.model = kv.get("ProductType").cloned().unwrap_or_default();
            d.ios = kv.get("ProductVersion").cloned().unwrap_or_default();
            d.serial = kv.get("SerialNumber").cloned().unwrap_or_default();
        }
        Ok((false, text)) => {
            d.problem = Some(trust_hint(&text));
            return d;
        }
        Err(e) => {
            d.problem = Some(e);
            return d;
        }
    }
    let query = |domain: &str, key: &str| -> Option<String> {
        let (ok, text) = tools.run("ideviceinfo", &["-u", udid, "-q", domain]).ok()?;
        ok.then(|| key_values(&text).get(key).cloned()).flatten()
    };
    d.storage_total =
        query("com.apple.disk_usage", "TotalDiskCapacity").and_then(|v| v.parse().ok());
    d.storage_free =
        query("com.apple.disk_usage", "TotalDataAvailable").and_then(|v| v.parse().ok());
    d.battery =
        query("com.apple.mobile.battery", "BatteryCurrentCapacity").and_then(|v| v.parse().ok());
    d.encrypts_backups = query("com.apple.mobile.backup", "WillEncrypt").map(|v| v == "true");
    d
}

/// What to tell someone when the device would not talk.
fn trust_hint(output: &str) -> String {
    let l = output.to_ascii_lowercase();
    if l.contains("password") || l.contains("passcode") || l.contains("locked") {
        "Unlock the device with its passcode, then try again.".into()
    } else if l.contains("trust")
        || l.contains("pair")
        || l.contains("-5")
        || l.contains("invalid host")
    {
        "Tap Trust on the device, and enter its passcode if asked.".into()
    } else {
        "The device could not be read. Unlock it, reconnect the cable, and tap Trust if asked."
            .into()
    }
}

/// Asks the device to trust this computer. The person has to tap **Trust**
/// on the device (and enter its passcode); this waits for the answer.
pub fn pair(tools: &Tools, udid: &str) -> Result<(), String> {
    let (ok, text) = tools.run("idevicepair", &["-u", udid, "pair"])?;
    if ok && !text.to_ascii_lowercase().contains("error") {
        Ok(())
    } else {
        Err(trust_hint(&text))
    }
}

// ---------------------------------------------------------------------------
// Making a backup
// ---------------------------------------------------------------------------

/// How far a backup has got.
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    /// 0 to 100.
    pub percent: u8,
    pub text: String,
}

/// Backs the device up into `dest` (a backup folder named by the device's
/// id is made inside it) and returns that folder. Stops early if `cancel`
/// is set.
pub fn backup(
    tools: &Tools,
    udid: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<PathBuf, String> {
    fs::create_dir_all(dest).map_err(|e| format!("Could not make {}: {e}", dest.display()))?;
    let mut child = tools
        .command("idevicebackup2")?
        .args(["-u", udid, "backup"])
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("The backup could not be started: {e}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            // The tool redraws its progress bar with \r, not \n.
            let mut buf = Vec::new();
            for b in BufReader::new(stream).bytes().map_while(Result::ok) {
                if b == b'\n' || b == b'\r' {
                    if !buf.is_empty() {
                        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
                        buf.clear();
                    }
                } else {
                    buf.push(b);
                }
            }
            if !buf.is_empty() {
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            }
        });
    }
    drop(tx);
    let mut tail: Vec<String> = Vec::new();
    let mut succeeded = false;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(300)) {
            Ok(line) => {
                if line.contains("Backup Successful") {
                    succeeded = true;
                }
                if let Some(p) = percent_in(&line) {
                    progress(Progress {
                        percent: p,
                        text: "Copying the device".into(),
                    });
                } else if !line.trim().is_empty() {
                    progress(Progress {
                        percent: 0,
                        text: line.trim().chars().take(120).collect(),
                    });
                    tail.push(line);
                    if tail.len() > 8 {
                        tail.remove(0);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Cancelled.".into());
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let dir = dest.join(udid);
    if (status.success() || succeeded) && dir.join("Manifest.db").is_file() {
        Ok(dir)
    } else {
        let why = tail.join(" ");
        Err(if why.to_ascii_lowercase().contains("password") {
            "The backup is encrypted. Turn off \"Encrypt local backup\" in Finder, or make it there and open it here with its password.".into()
        } else if why.is_empty() {
            "The backup did not finish.".into()
        } else {
            format!("The backup did not finish: {why}")
        })
    }
}

/// `34%` anywhere in a line.
fn percent_in(line: &str) -> Option<u8> {
    let i = line.find('%')?;
    let digits: String = line[..i]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .chars()
        .rev()
        .collect::<String>()
        .parse::<u16>()
        .ok()
        .map(|n| n.min(100) as u8)
}

// ---------------------------------------------------------------------------
// Backups on this computer
// ---------------------------------------------------------------------------

/// A backup folder.
#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub path: String,
    pub name: String,
    pub model: String,
    pub ios: String,
    /// When it was made, `2024-05-01T10:20:30Z`.
    pub date: String,
    pub encrypted: bool,
    pub udid: String,
}

/// Where Finder and iTunes keep their backups.
pub fn finder_backups_folder() -> Option<PathBuf> {
    // Finder keeps backups here on a Mac; iTunes on Windows keeps them under the user's Apple folder.
    let home = crate::plugin::home_dir()?;
    Some(if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Apple Computer/MobileSync/Backup")
    } else {
        home.join("Library/Application Support/MobileSync/Backup")
    })
}

/// The backups in `folders`, newest first. A folder that cannot be read (on a
/// Mac, Finder's needs Full Disk Access) is skipped.
pub fn backups(folders: &[PathBuf]) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    for folder in folders {
        for e in fs::read_dir(folder).into_iter().flatten().flatten() {
            if let Some(b) = backup_info(&e.path()) {
                out.push(b);
            }
        }
    }
    out.sort_by(|a, b| b.date.cmp(&a.date));
    out
}

fn backup_info(dir: &Path) -> Option<BackupInfo> {
    let info = plist::Value::from_file(dir.join("Info.plist")).ok()?;
    let info = info.as_dictionary()?;
    let text = |k: &str| {
        info.get(k)
            .and_then(plist::Value::as_string)
            .unwrap_or("")
            .to_string()
    };
    let encrypted = plist::Value::from_file(dir.join("Manifest.plist"))
        .ok()
        .and_then(|m| {
            m.as_dictionary()
                .and_then(|d| d.get("IsEncrypted"))
                .and_then(plist::Value::as_boolean)
        })
        .unwrap_or(false);
    let date = info
        .get("Last Backup Date")
        .and_then(plist::Value::as_date)
        .map(|d| d.to_xml_format())
        .unwrap_or_default();
    Some(BackupInfo {
        path: dir.display().to_string(),
        name: text("Device Name"),
        model: text("Product Type"),
        ios: text("Product Version"),
        udid: text("Unique Identifier"),
        date,
        encrypted,
    })
}

// ---------------------------------------------------------------------------
// Inside a backup
// ---------------------------------------------------------------------------

/// An open backup. Everything is read-only.
pub struct Backup {
    dir: PathBuf,
    manifest: Connection,
    scratch: PathBuf,
}

/// A file in the backup.
#[derive(Debug, Clone, Serialize)]
pub struct BackupFile {
    pub id: String,
    pub domain: String,
    pub path: String,
    pub size: u64,
}

/// A photo or video in the phone's library.
#[derive(Debug, Clone, Serialize)]
pub struct Photo {
    /// The file in the backup, if the backup holds it.
    pub file_id: Option<String>,
    pub name: String,
    pub video: bool,
    /// When it was taken, `2024-05-01T10:20:30Z`.
    pub date: Option<String>,
    /// In *Recently Deleted*: deleted on the phone, still recoverable.
    pub deleted: bool,
    pub deleted_date: Option<String>,
    pub size: u64,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// A text message.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    /// The other person: a number or an address.
    pub with: String,
    pub text: String,
    pub date: Option<String>,
    pub from_me: bool,
    /// Deleted on the phone, still held in its Recently Deleted.
    pub deleted: bool,
    /// Deleted for good and recovered from the database's leftovers. May be
    /// incomplete, and the sender and date may be missing.
    pub recovered: bool,
}

/// A piece of text left in a database's free pages: deleted, and partly
/// overwritten, so it may be incomplete.
#[derive(Debug, Clone, Serialize)]
pub struct Fragment {
    pub text: String,
    pub page: u32,
}

impl Backup {
    /// Opens the backup in `dir`. Encrypted backups are refused.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let db = dir.join("Manifest.db");
        if !db.is_file() {
            return Err("This folder is not an iPhone backup (there is no Manifest.db).".into());
        }
        if backup_info(dir).map(|b| b.encrypted).unwrap_or(false) {
            return Err("This backup is encrypted, and Reperio cannot read encrypted backups. Make an unencrypted one, or turn off \"Encrypt local backup\" in Finder and back up again.".into());
        }
        let manifest = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("The backup's file list could not be opened: {e}"))?;
        // A quick check that it is the real thing.
        manifest
            .query_row("SELECT count(*) FROM Files", [], |r| r.get::<_, i64>(0))
            .map_err(|_| "The backup's file list is damaged or encrypted.".to_string())?;
        let scratch =
            std::env::temp_dir().join(format!("reperio-ios-{}-{}", std::process::id(), tick()));
        fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            scratch,
        })
    }

    /// What the backup is: its device, iOS version and date.
    pub fn info(&self) -> Option<BackupInfo> {
        backup_info(&self.dir)
    }

    /// Where a file's contents are, or `None` if the backup does not hold them.
    pub fn stored(&self, file_id: &str) -> Option<PathBuf> {
        if file_id.len() < 2 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        [
            self.dir.join(&file_id[..2]).join(file_id),
            self.dir.join(file_id),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    pub(crate) fn lookup(&self, domain: &str, path: &str) -> Option<String> {
        self.manifest
            .query_row(
                "SELECT fileID FROM Files WHERE domain = ?1 AND relativePath = ?2 AND flags = 1",
                [domain, path],
                |r| r.get(0),
            )
            .ok()
    }

    /// Every file, optionally only in a domain (`CameraRollDomain`) or whose
    /// path contains `search`.
    pub fn files(&self, domain: Option<&str>, search: &str, limit: usize) -> Vec<BackupFile> {
        let like = format!("%{}%", search.replace(['%', '_'], ""));
        let mut stmt = match self.manifest.prepare(
            "SELECT fileID, domain, relativePath FROM Files
             WHERE flags = 1 AND (?1 IS NULL OR domain = ?1) AND relativePath LIKE ?2
             ORDER BY domain, relativePath LIMIT ?3",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params![domain, like, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(id, domain, path)| {
                let size = self.stored(&id)?.metadata().ok()?.len();
                Some(BackupFile {
                    id,
                    domain,
                    path,
                    size,
                })
            })
            .collect()
    }

    /// A copy of a database from the backup, in the scratch folder, so that
    /// opening it (which may write its journal) never touches the backup.
    pub(crate) fn database(&self, domain: &str, path: &str) -> Option<Connection> {
        let id = self.lookup(domain, path)?;
        let name = Path::new(path).file_name()?.to_str()?;
        let copy = self.scratch.join(name);
        fs::copy(self.stored(&id)?, &copy).ok()?;
        for ext in ["-wal", "-shm"] {
            if let Some(extra) = self
                .lookup(domain, &format!("{path}{ext}"))
                .and_then(|i| self.stored(&i))
            {
                let _ = fs::copy(extra, self.scratch.join(format!("{name}{ext}")));
            }
        }
        Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()
    }

    /// The photo library: everything the phone's Photos app knows of,
    /// including photos in Recently Deleted.
    pub fn photos(&self) -> Result<Vec<Photo>, String> {
        let db = self
            .database("CameraRollDomain", "Media/PhotoData/Photos.sqlite")
            .ok_or("This backup has no photo library (Photos.sqlite).")?;
        // iOS 14 and later call the table ZASSET, before that ZGENERICASSET.
        let table = ["ZASSET", "ZGENERICASSET"]
            .into_iter()
            .find(|t| table_exists(&db, t))
            .ok_or("The photo library has no photos table.")?;
        let have = columns(&db, table);
        let col = |name: &str| {
            if have.iter().any(|c| c == name) {
                name.to_string()
            } else {
                "NULL".into()
            }
        };
        let sql = format!(
            "SELECT {dir}, {file}, {kind}, {created}, {trashed}, {trashed_at}, {w}, {h} FROM {table}",
            dir = col("ZDIRECTORY"),
            file = col("ZFILENAME"),
            kind = col("ZKIND"),
            created = col("ZDATECREATED"),
            trashed = col("ZTRASHEDSTATE"),
            trashed_at = col("ZTRASHEDDATE"),
            w = col("ZWIDTH"),
            h = col("ZHEIGHT"),
        );
        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<f64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (dir, file, kind, created, trashed, trashed_at, width, height) in rows.flatten() {
            let Some(name) = file.filter(|f| !f.is_empty()) else {
                continue;
            };
            let rel = format!(
                "Media/{}/{}",
                dir.unwrap_or_default().trim_matches('/'),
                name
            );
            let file_id = self.lookup("CameraRollDomain", &rel);
            let size = file_id
                .as_deref()
                .and_then(|i| self.stored(i))
                .and_then(|p| p.metadata().ok())
                .map_or(0, |m| m.len());
            out.push(Photo {
                file_id: file_id.filter(|_| size > 0),
                name,
                video: kind == Some(1),
                date: created.map(apple_date),
                deleted: trashed == Some(1),
                deleted_date: trashed_at.filter(|_| trashed == Some(1)).map(apple_date),
                size,
                width: width.filter(|&w| w > 0),
                height: height.filter(|&h| h > 0),
            });
        }
        out.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(out)
    }

    /// Text messages. `search` filters by text or contact.
    pub fn messages(&self, search: &str, limit: usize) -> Result<Vec<Message>, String> {
        let db = self
            .database("HomeDomain", "Library/SMS/sms.db")
            .ok_or("This backup has no messages (sms.db).")?;
        let recoverable = table_exists(&db, "chat_recoverable_message_join");
        let mut stmt = db
            .prepare(&format!(
                "SELECT m.ROWID, COALESCE(h.id, ''), m.text, m.attributedBody, m.date, m.is_from_me, {}
                 FROM message m LEFT JOIN handle h ON h.ROWID = m.handle_id
                 ORDER BY m.date DESC",
                if recoverable {
                    "EXISTS (SELECT 1 FROM chat_recoverable_message_join r WHERE r.message_id = m.ROWID)"
                } else {
                    "0"
                }
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let needle = search.to_lowercase();
        let mut out = Vec::new();
        for (id, with, text, body, date, from_me, deleted) in rows.flatten() {
            // Newer iOS keeps the text of many messages only in attributedBody.
            let text = text
                .filter(|t| !t.is_empty())
                .or_else(|| body.as_deref().and_then(attributed_text))
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            if !needle.is_empty()
                && !text.to_lowercase().contains(&needle)
                && !with.to_lowercase().contains(&needle)
            {
                continue;
            }
            out.push(Message {
                id,
                with,
                text,
                date: date.map(|d| {
                    apple_date(if d > 1_000_000_000_000 {
                        d as f64 / 1e9
                    } else {
                        d as f64
                    })
                }),
                from_me: from_me == Some(1),
                deleted: deleted == 1,
                recovered: false,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Pieces of text left in the message database's free pages: messages
    /// that were deleted and not yet overwritten. Partial and unordered.
    pub fn message_fragments(&self) -> Vec<Fragment> {
        let Some(id) = self.lookup("HomeDomain", "Library/SMS/sms.db") else {
            return Vec::new();
        };
        let Some(path) = self.stored(&id) else {
            return Vec::new();
        };
        fs::read(path)
            .map(|d| free_page_text(&d))
            .unwrap_or_default()
    }

    /// Rows deleted from any database in the backup (`domain`, `path` as in
    /// the file list), found in its free space and write-ahead log. Whole rows,
    /// with their columns. Live rows are not included.
    pub fn deleted_rows(
        &self,
        domain: &str,
        path: &str,
    ) -> Result<(Vec<Carved>, Vec<Table>), String> {
        let id = self
            .lookup(domain, path)
            .ok_or("That database is not in the backup.")?;
        let from = self
            .stored(&id)
            .ok_or("That database's file is not in the backup.")?;
        let name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("bad path")?;
        let src = self.scratch.join(format!("carve-src-{}", tick()));
        let work = self.scratch.join(format!("carve-work-{}", tick()));
        fs::create_dir_all(&src)
            .and_then(|()| fs::create_dir_all(&work))
            .map_err(|e| e.to_string())?;
        fs::copy(&from, src.join(name)).map_err(|e| e.to_string())?;
        if let Some(w) = self
            .lookup(domain, &format!("{path}-wal"))
            .and_then(|i| self.stored(&i))
        {
            let _ = fs::copy(w, src.join(format!("{name}-wal")));
        }
        let out = sqlite_carve::carve_file(&src.join(name), &work);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&work);
        out
    }

    /// Messages deleted for good, put back together from what the message
    /// database still holds. Each is marked `recovered`.
    pub fn deleted_messages(&self) -> Result<Vec<Message>, String> {
        let (rows, tables) = self.deleted_rows("HomeDomain", "Library/SMS/sms.db")?;
        let Some(table) = tables.iter().find(|t| t.name == "message") else {
            return Ok(Vec::new());
        };
        let at = |n: &str| table.columns.iter().position(|c| c.name == n);
        let (text, body, date, mine, handle) = (
            at("text"),
            at("attributedBody"),
            at("date"),
            at("is_from_me"),
            at("handle_id"),
        );
        // Who the handle numbers were.
        let mut who: HashMap<i64, String> = HashMap::new();
        if let Some(db) = self.database("HomeDomain", "Library/SMS/sms.db") {
            if let Ok(mut st) = db.prepare("SELECT ROWID, id FROM handle") {
                if let Ok(rows) =
                    st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                {
                    who.extend(rows.flatten());
                }
            }
        }
        let int = |c: &sqlite_carve::Cell| {
            if let sqlite_carve::Cell::Int(n) = c {
                Some(*n)
            } else {
                None
            }
        };
        let mut out = Vec::new();
        for (n, r) in rows.iter().filter(|r| r.table == "message").enumerate() {
            let t = text
                .and_then(|i| {
                    if let sqlite_carve::Cell::Text(t) = &r.cells[i] {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    body.and_then(|i| {
                        if let sqlite_carve::Cell::Blob(b) = &r.cells[i] {
                            attributed_text(b)
                        } else {
                            None
                        }
                    })
                });
            let Some(text) = t else { continue };
            let d = date.and_then(|i| int(&r.cells[i])).filter(|&d| d > 0);
            out.push(Message {
                id: -(n as i64) - 1,
                with: handle
                    .and_then(|i| int(&r.cells[i]))
                    .and_then(|h| who.get(&h).cloned())
                    .unwrap_or_default(),
                text,
                date: d.map(|d| {
                    apple_date(if d > 1_000_000_000_000 {
                        d as f64 / 1e9
                    } else {
                        d as f64
                    })
                }),
                from_me: mine.and_then(|i| int(&r.cells[i])) == Some(1),
                deleted: true,
                recovered: true,
            });
        }
        out.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(out)
    }

    /// Copies files out of the backup into `dest`, with their real names and
    /// folders (`CameraRollDomain/Media/DCIM/100APPLE/IMG_0001.JPG`). Returns
    /// how many were copied.
    pub fn export_files(&self, ids: &[String], dest: &Path) -> Result<usize, String> {
        let mut n = 0;
        for id in ids {
            let (domain, rel): (String, String) = self
                .manifest
                .query_row(
                    "SELECT domain, relativePath FROM Files WHERE fileID = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|_| "A file is not in the backup.".to_string())?;
            let Some(from) = self.stored(id) else {
                continue;
            };
            let to = safe_join(dest, &domain, &rel)?;
            copy_into(&from, &to)?;
            n += 1;
        }
        Ok(n)
    }

    /// Copies photos out, named as on the phone, in `dest` (or in a
    /// `Recently Deleted` folder inside it for deleted ones).
    pub fn export_photos(&self, photos: &[Photo], dest: &Path) -> Result<usize, String> {
        let mut n = 0;
        for p in photos {
            let Some(id) = &p.file_id else { continue };
            let Some(from) = self.stored(id) else {
                continue;
            };
            let folder = if p.deleted {
                dest.join("Recently Deleted")
            } else {
                dest.to_path_buf()
            };
            let to = unique(&folder.join(sanitize(&p.name)));
            copy_into(&from, &to)?;
            n += 1;
        }
        Ok(n)
    }
}

impl Backup {
    /// A small JPEG of a photo or video in the backup, or `None` if it cannot
    /// be drawn (no decoder for it here, and no Quick Look on this system).
    pub fn thumbnail(&self, file_id: &str, max: u32) -> Option<Vec<u8>> {
        thumbnail_of(&self.stored(file_id)?, max, &self.scratch)
    }
}

/// A JPEG no larger than `max` pixels on a side. HEIC and video, which the
/// built-in decoders cannot read, go to macOS Quick Look when there is one.
pub fn thumbnail_of(path: &Path, max: u32, scratch: &Path) -> Option<Vec<u8>> {
    let decoded = image::ImageReader::open(path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.decode().ok());
    if let Some(img) = decoded {
        let small = img.thumbnail(max, max).to_rgb8();
        let mut out = std::io::Cursor::new(Vec::new());
        small.write_to(&mut out, image::ImageFormat::Jpeg).ok()?;
        return Some(out.into_inner());
    }
    quicklook(path, max, scratch)
}

#[cfg(target_os = "macos")]
fn quicklook(path: &Path, max: u32, scratch: &Path) -> Option<Vec<u8>> {
    // qlmanage names its output after the input, and needs a known extension:
    // backup files have none, so give it a copy that does.
    let name = path.file_name()?.to_str()?;
    let ext = sniff_extension(path)?;
    let copy = scratch.join(format!("{name}.{ext}"));
    fs::copy(path, &copy).ok()?;
    let ok = Command::new("qlmanage")
        .args(["-t", "-s", &max.to_string(), "-o"])
        .arg(scratch)
        .arg(&copy)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let png = scratch.join(format!("{name}.{ext}.png"));
    let bytes = if ok { fs::read(&png).ok() } else { None };
    let _ = fs::remove_file(&copy);
    let _ = fs::remove_file(&png);
    bytes
}

#[cfg(not(target_os = "macos"))]
fn quicklook(_: &Path, _: u32, _: &Path) -> Option<Vec<u8>> {
    None
}

/// A file's extension from its first bytes, for the kinds an iPhone makes.
#[cfg(target_os = "macos")]
fn sniff_extension(path: &Path) -> Option<&'static str> {
    let mut head = [0u8; 16];
    fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    Some(if head.starts_with(&[0xff, 0xd8]) {
        "jpg"
    } else if &head[4..8] == b"ftyp" {
        if matches!(&head[8..12], b"heic" | b"heix" | b"mif1" | b"msf1") {
            "heic"
        } else {
            "mov"
        }
    } else if head.starts_with(b"\x89PNG") {
        "png"
    } else {
        return None;
    })
}

impl Drop for Backup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

fn tick() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

pub(crate) fn table_exists(db: &Connection, name: &str) -> bool {
    db.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

pub(crate) fn columns(db: &Connection, table: &str) -> Vec<String> {
    let Ok(mut stmt) = db.prepare(&format!("PRAGMA table_info({table})")) else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// A date counted in seconds from 2001, as `2024-05-01T10:20:30Z`.
pub(crate) fn apple_date(secs: f64) -> String {
    let t = secs as i64 + APPLE_EPOCH;
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The text of a message whose `text` column is empty: iOS stores it in a
/// serialised `NSAttributedString` after the marker `NSString`.
fn attributed_text(blob: &[u8]) -> Option<String> {
    let marker = b"NSString";
    let at = blob.windows(marker.len()).position(|w| w == marker)? + marker.len();
    let rest = blob.get(at + 5..)?;
    let (len, start) = match *rest.first()? {
        0x81 => (
            u16::from_le_bytes([*rest.get(1)?, *rest.get(2)?]) as usize,
            3,
        ),
        n => (n as usize, 1),
    };
    let text = rest.get(start..start + len)?;
    let s = String::from_utf8_lossy(text).into_owned();
    (!s.trim().is_empty()).then_some(s)
}

/// Runs of readable text in the free pages of a SQLite database.
fn free_page_text(db: &[u8]) -> Vec<Fragment> {
    let be32 = |at: usize| {
        db.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    let page = match db.get(16..18).map(|b| u16::from_be_bytes([b[0], b[1]])) {
        Some(1) => 65_536,
        Some(n) if n.is_power_of_two() && n >= 512 => n as usize,
        _ => return Vec::new(),
    };
    let mut free = Vec::new();
    let mut trunk = be32(32).unwrap_or(0) as usize;
    let mut seen = 0;
    while trunk > 0 && seen < 100_000 {
        seen += 1;
        let base = (trunk - 1) * page;
        let Some(next) = be32(base) else { break };
        free.push(trunk);
        let count = be32(base + 4).unwrap_or(0) as usize;
        for i in 0..count.min(page / 4 - 2) {
            if let Some(leaf) = be32(base + 8 + i * 4) {
                free.push(leaf as usize);
            }
        }
        trunk = next as usize;
    }
    let mut out = Vec::new();
    for p in free {
        let Some(data) = db.get((p.saturating_sub(1)) * page..p * page) else {
            continue;
        };
        let mut run = Vec::new();
        for &b in data.iter().chain(std::iter::once(&0)) {
            // Plain UTF-8 text: printable ASCII, and the bytes of other scripts.
            if (0x20..0x7f).contains(&b) || b >= 0x80 || b == b'\n' {
                run.push(b);
            } else {
                if run.len() >= 8 {
                    // Record headers sit between the texts and are not valid UTF-8:
                    // they split the run, and what is left on each side is the text.
                    for piece in String::from_utf8_lossy(&run).split('\u{fffd}') {
                        let s = piece.trim().to_string();
                        if s.chars().count() >= 8
                            && s.chars().filter(|c| c.is_alphabetic()).count() >= 5
                        {
                            out.push(Fragment {
                                text: s,
                                page: p as u32,
                            });
                        }
                    }
                }
                run.clear();
            }
        }
    }
    out
}

/// A file name that cannot climb out of its folder.
fn sanitize(name: &str) -> String {
    let n: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if n.is_empty() || n == "." || n == ".." {
        "file".into()
    } else {
        n
    }
}

/// `dest/domain/relative/path`, refusing any path that escapes `dest`.
fn safe_join(dest: &Path, domain: &str, rel: &str) -> Result<PathBuf, String> {
    let mut p = dest.join(sanitize(domain));
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        if part == ".." || part == "." {
            return Err("A file in the backup has an unsafe path and was not copied.".into());
        }
        p.push(sanitize(part));
    }
    Ok(p)
}

fn unique(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

fn copy_into(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Could not make {}: {e}", parent.display()))?;
    }
    fs::copy(from, unique(to)).map_err(|e| format!("Could not copy {}: {e}", to.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_percentages() {
        assert_eq!(apple_date(0.0), "2001-01-01T00:00:00Z");
        assert_eq!(apple_date(725_760_000.0), "2024-01-01T00:00:00Z");
        assert_eq!(percent_in("[====   ] 34% Finished"), Some(34));
        assert_eq!(percent_in("Receiving files"), None);
    }

    #[test]
    fn message_text_is_found_in_attributed_bodies() {
        // typedstream: ... NSString, 5 bytes of framing, a length, the text.
        let mut blob = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+".to_vec();
        blob.push(11);
        blob.extend_from_slice(b"hello world");
        blob.extend_from_slice(b"\x86\x84\x02iI");
        assert_eq!(attributed_text(&blob).as_deref(), Some("hello world"));
        assert_eq!(attributed_text(b"nothing here"), None);
    }

    #[test]
    fn exports_cannot_escape_their_folder() {
        let d = Path::new("/out");
        assert_eq!(
            safe_join(d, "HomeDomain", "Library/a.txt").unwrap(),
            Path::new("/out/HomeDomain/Library/a.txt")
        );
        assert!(safe_join(d, "HomeDomain", "../../etc/passwd").is_err());
        assert_eq!(sanitize("../x"), ".._x");
    }

    #[test]
    fn thumbnails_are_small_jpegs() {
        let dir = scratch("thumb");
        let big = image::RgbImage::from_fn(800, 400, |x, y| {
            image::Rgb([(x % 255) as u8, (y % 255) as u8, 90])
        });
        let path = dir.join("abc");
        big.save_with_format(&path, image::ImageFormat::Jpeg)
            .unwrap();
        let t = thumbnail_of(&path, 128, &dir).expect("drawn");
        let img = image::load_from_memory(&t).unwrap();
        assert_eq!((img.width(), img.height()), (128, 64));
        fs::write(dir.join("junk"), b"not an image at all").unwrap();
        assert!(thumbnail_of(&dir.join("junk"), 128, &dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    fn mock_tools() -> Option<Tools> {
        if Command::new("python3").arg("--version").output().is_err() {
            return None;
        }
        Some(Tools {
            dir: Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/mock_ios")),
        })
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("reperio-ios-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The whole path, against mock `libimobiledevice` tools: find the phone,
    /// trust it, back it up, then read the backup.
    #[test]
    fn a_phone_is_backed_up_and_read() {
        let Some(tools) = mock_tools() else { return };
        let found = devices(&tools).unwrap();
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert!(d.paired && d.name == "Sam's iPhone" && d.ios == "17.4" && d.model == "iPhone15,2");
        assert_eq!(
            (d.storage_total, d.storage_free, d.battery),
            (Some(128_000_000_000), Some(54_000_000_000), Some(87))
        );
        assert_eq!(d.encrypts_backups, Some(false));
        pair(&tools, &d.udid).unwrap();

        let dest = scratch("backup");
        let mut last = 0;
        let dir = backup(&tools, &d.udid, &dest, &AtomicBool::new(false), &mut |p| {
            last = last.max(p.percent)
        })
        .unwrap();
        assert_eq!(last, 100, "progress was reported");
        let list = backups(std::slice::from_ref(&dest));
        assert_eq!(list.len(), 1);
        assert_eq!(
            (
                list[0].name.as_str(),
                list[0].ios.as_str(),
                list[0].encrypted
            ),
            ("Sam's iPhone", "17.4", false)
        );
        assert!(list[0].date.starts_with("2024-05-01"));

        let b = Backup::open(&dir).unwrap();
        // Photos: three in the library plus one in Recently Deleted whose file was never backed up.
        let photos = b.photos().unwrap();
        assert_eq!(photos.len(), 4);
        let by = |n: &str| photos.iter().find(|p| p.name == n).unwrap();
        assert!(!by("IMG_0001.JPG").deleted && by("IMG_0001.JPG").file_id.is_some());
        assert!(by("IMG_0002.JPG").deleted && by("IMG_0002.JPG").file_id.is_some());
        assert_eq!(
            by("IMG_0002.JPG").deleted_date.as_deref(),
            Some("2024-04-20T12:00:00Z")
        );
        assert!(by("IMG_0003.MOV").video);
        assert!(
            by("IMG_0004.JPG").deleted && by("IMG_0004.JPG").file_id.is_none(),
            "listed, but not in the backup"
        );

        // Messages, with the text from attributedBody, and the deleted one marked.
        let msgs = b.messages("", 100).unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(msgs.iter().any(|m| m.text == "text only in the blob"));
        let gone = msgs.iter().find(|m| m.text == "the code is 4821").unwrap();
        assert!(gone.deleted && gone.from_me && gone.with == "+15551230001");
        assert_eq!(b.messages("nine", 100).unwrap().len(), 1);
        // Deleted text still in the database's free pages.
        assert!(b
            .message_fragments()
            .iter()
            .any(|f| f.text.contains("signed contract")));
        // Messages deleted for good come back whole: text, sender, date, direction.
        let lost = b.deleted_messages().unwrap();
        assert!(
            lost.len() >= 30,
            "most of the 40 came back, got {}",
            lost.len()
        );
        let one = lost
            .iter()
            .find(|m| m.text == "Gone for good: the meeting moved to room 7 at ten")
            .expect("message 7");
        assert!(one.recovered && one.deleted && one.with == "+15551230001" && one.from_me);
        assert_eq!(one.date.as_deref(), Some("2024-03-01T09:37:00Z"));
        assert!(
            !lost.iter().any(|m| m.text == "see you at nine"),
            "live messages are not reported"
        );

        // Files, and exporting.
        assert!(b
            .files(Some("HomeDomain"), "notes", 10)
            .iter()
            .any(|f| f.path == "Library/Notes/notes.txt"));
        let out = scratch("export");
        let deleted: Vec<Photo> = photos.iter().filter(|p| p.deleted).cloned().collect();
        assert_eq!(
            b.export_photos(&deleted, &out).unwrap(),
            1,
            "only the one that was backed up"
        );
        assert_eq!(
            fs::read(out.join("Recently Deleted/IMG_0002.JPG")).unwrap(),
            b"\xff\xd8\xff\xe0 deleted photo \xff\xd9"
        );
        let ids: Vec<String> = b
            .files(None, "report", 10)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(b.export_files(&ids, &out).unwrap(), 1);
        assert!(out
            .join("AppDomain-com.example.app/Documents/report.pdf")
            .is_file());
        let _ = fs::remove_dir_all(&dest);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn a_phone_that_has_not_trusted_this_computer_says_so() {
        let Some(tools) = mock_tools() else { return };
        let d = device(&tools, "LOCKED-0000000000000000");
        assert!(!d.paired);
        assert!(d.problem.unwrap().contains("Trust"));
    }

    #[test]
    fn missing_tools_say_how_to_install_them() {
        let none = Tools {
            dir: Some(PathBuf::from("/nonexistent")),
        };
        if none.find("idevice_id").is_none() {
            assert!(devices(&none)
                .unwrap_err()
                .contains("brew install libimobiledevice"));
        }
    }

    #[test]
    fn encrypted_backups_are_refused_with_a_reason() {
        let Some(_) = mock_tools() else { return };
        let dir = scratch("enc");
        let status = Command::new("python3")
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../scripts/mock_ios/make_fake_backup.py"),
            )
            .arg(&dir)
            .arg("--encrypted")
            .status()
            .unwrap();
        assert!(status.success());
        let err = Backup::open(&dir).err().expect("refused");
        assert!(err.contains("encrypted"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A plug-in works on a backup: lists its files, reads one, asks for the
    /// deleted rows of a database, and has Reperio repair a damaged photo.
    #[test]
    fn a_plugin_can_work_on_a_backup_and_repair_a_photo() {
        let Some(tools) = mock_tools() else { return };
        let _g = crate::plugin::TEST_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dest = scratch("plug-backup");
        let dir = backup(
            &tools,
            "00008030-001A2B3C4D5E6F78",
            &dest,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        let b = Backup::open(&dir).unwrap();

        // A photo with its tail cut off, as a recovered file often is.
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_fn(160, 120, |x, y| {
            image::Rgb([(x * 3) as u8, (y * 2) as u8, ((x * y) % 255) as u8])
        })
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
        let jpeg = jpeg.into_inner();
        let cut: String = jpeg[..jpeg.len() * 3 / 4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let plug = scratch("plug-src");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/sdk/python/_reperio.py"),
            plug.join("_reperio.py"),
        )
        .unwrap();
        let script = plug.join("onbackup.py");
        std::fs::write(
            &script,
            format!(
                r#"import _reperio as reperio
plugin = reperio.Plugin(name="onbackup", kind="analyzer", targets=["ios-backup"])

@plugin.analyze
def analyze(run):
    run.finding("device", run.backup.get("name", "?"))
    files = run.backup_files("sms.db")
    run.finding("sms", f"{{len(files)}} {{files[0]['path']}}")
    head = run.backup_read(files[0]["id"], 0, 16)
    run.finding("head", head[:15].decode("latin-1"))
    rows = run.backup_deleted_rows("HomeDomain", "Library/SMS/sms.db")
    run.finding("deleted", str(len([r for r in rows if r["table"] == "message"])))
    open(run.path("bad.jpg"), "wb").write(bytes.fromhex("{cut}"))
    fixed = run.repair("bad.jpg")
    run.finding("repaired", fixed.rsplit("/", 1)[-1] + " " + str(len(open(fixed, "rb").read()) > 100))

plugin.run()
"#
            ),
        )
        .unwrap();
        let p = crate::plugin::probe(&script).unwrap();
        let out = scratch("plug-out");
        let r = crate::analyzer::run(
            &p,
            &crate::analyzer::Target {
                device: None,
                image: None,
                backup: Some(&b),
            },
            "ios-backup",
            &out,
            &crate::analyzer::RunOptions::default(),
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(r.error.is_none(), "{:?} {}", r.error, r.console);
        let say = |t: &str| {
            r.findings
                .iter()
                .find(|f| f.title == t)
                .map(|f| f.detail.clone())
                .unwrap_or_default()
        };
        assert_eq!(say("device"), "Sam's iPhone");
        assert_eq!(say("sms"), "1 Library/SMS/sms.db");
        assert_eq!(say("head"), "SQLite format 3");
        assert!(
            say("deleted").parse::<usize>().unwrap() >= 30,
            "deleted rows were recovered for the plug-in: {}",
            say("deleted")
        );
        assert_eq!(say("repaired"), "bad (repaired).jpg True");
        assert_eq!(r.refused, 0);
    }

    /// WhatsApp and Notes in the backup: what is still there, what is in the
    /// apps' own Recently Deleted, and what was deleted for good.
    #[test]
    fn whatsapp_and_notes_are_read_and_recovered() {
        let Some(tools) = mock_tools() else { return };
        let dest = scratch("apps");
        let dir = backup(
            &tools,
            "00008030-001A2B3C4D5E6F78",
            &dest,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        let b = Backup::open(&dir).unwrap();

        let wa = b.whatsapp().unwrap();
        assert_eq!(wa.chats.len(), 2);
        assert_eq!(wa.chats[0].name, "Family", "newest chat first");
        assert!(wa.chats[0].group && !wa.chats[1].group);
        assert_eq!(wa.messages.len(), 4);
        let photo = wa
            .messages
            .iter()
            .find(|m| m.kind == "photo")
            .expect("the photo message");
        assert!(
            photo.media.is_some() && photo.media_name.as_deref() == Some("Beach") && !photo.from_me
        );
        assert!(wa
            .messages
            .iter()
            .any(|m| m.text == "Yes, see you at the beach" && m.from_me && m.who == "Me"));
        let lost = b.whatsapp_deleted().unwrap();
        // SQLite itself overwrote the first 14 of the 40 when it split a page, so 26 are in the file.
        assert_eq!(
            lost.len(),
            26,
            "every message still in the file came back, once each"
        );
        let one = lost
            .iter()
            .find(|m| m.text == "Deleted chat: the keys are under the blue pot number 21")
            .expect("message 21");
        assert!(one.recovered && one.chat_name == "Alex" && one.from_me);
        assert!(
            !lost.iter().any(|m| m.text.starts_with("Are we still")),
            "live messages are not reported"
        );

        let notes = b.notes().unwrap();
        assert_eq!(notes.len(), 4);
        let trip = notes.iter().find(|n| n.title == "Trip ideas").unwrap();
        assert!(trip.text.contains("Lisbon in May") && !trip.deleted && trip.folder == "Notes");
        let old = notes.iter().find(|n| n.title == "Old draft").unwrap();
        assert!(old.deleted && old.folder == "Recently Deleted");
        let gone = b.notes_deleted().unwrap();
        assert!(
            gone.len() >= 18,
            "most of the 25 came back, got {}",
            gone.len()
        );
        assert!(gone.iter().any(|n| n.recovered
            && n.title == "Gone note 7"
            && n.text.contains("appointment number 7")));
    }
}

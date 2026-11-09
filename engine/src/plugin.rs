//! Plug-ins: recovery code of your own, in any language, used without
//! editing or rebuilding Reperio.
//!
//! A plug-in is one file in the plug-ins folder (see [`folders`]): a Python,
//! Node, shell, Ruby or Perl script, or a program. Reperio runs it and talks
//! to it in lines of JSON over the program's input and output. Reperio does
//! the reading from the drive, so a plug-in works on a disk, an image or a
//! RAID alike and cannot change the drive.
//!
//! Reperio sends `{"cmd":"info"}`; the plug-in answers with one line
//! `{"info":{"name":"...","fs":["ntfs","unknown"]}}` and exits. `fs` lists
//! the kinds of volume it wants (`ntfs`, `fat`, `exfat`, `hfs+`, `apfs`,
//! `ext`, `unknown` for what Reperio cannot read, or `any`).
//!
//! For each volume it sends `{"cmd":"scan","offset":N,"len":N,"fs":"ntfs"}`.
//! The plug-in may then send, one per line:
//!
//! - `{"read":{"offset":N,"len":N}}`: bytes of the volume (offset 0 is the
//!   volume's start). Reperio answers `{"ok":true,"data":"<base64>"}`, or
//!   `{"ok":false}` if they cannot be read.
//! - `{"file":{"path":"a/b.jpg","size":N,"deleted":true,"modified":
//!   "2024-05-01T10:20:30","extents":[[offset,len],...]}}`: a file found.
//! - `{"free":[[offset,len],...]}`: unused space, for the deep scan.
//! - `{"log":"text"}`: a message for the scan's details.
//! - `{"progress":{"done":N,"total":N,"text":".."}}`: how far along it is,
//!   shown on the scan screen (all fields optional).
//! - `{"done":{"label":"..."}}`: finished.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::extent::Extent;
use crate::forensic::Algorithms;
use crate::formats::DateTime;
use crate::fs::{FsFile, FsType, Volume, VolumeReader};
use crate::source::Source;

/// Reperio puts no limit on a plug-in unless the person asks for one: no time
/// limit, no cap on what it reads, sends or finds. These two are the person's
/// choice (Settings → Plug-ins) and are off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// Stop a plug-in that has said nothing for this long.
    pub idle: Option<Duration>,
    /// Stop a plug-in that has run for this long.
    pub hard: Option<Duration>,
}

static LIMITS: RwLock<Limits> = RwLock::new(Limits {
    idle: None,
    hard: None,
});

/// Sets the limits the person chose (or none).
pub fn set_limits(l: Limits) {
    if let Ok(mut cur) = LIMITS.write() {
        *cur = l;
    }
}

/// The limits in force now.
pub fn limits() -> Limits {
    LIMITS.read().map(|l| *l).unwrap_or_default()
}

/// How a plug-in's last run went, kept so the plug-in card can show it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Health {
    /// When it ran, `2024-05-01T10:20:30Z`.
    pub when: String,
    pub seconds: u64,
    pub ok: bool,
    /// One line: what it found, or what went wrong.
    pub note: String,
}

fn health_file() -> Option<PathBuf> {
    Some(user_folder()?.parent()?.join("plugin-health.json"))
}

static HEALTH_LOCK: Mutex<()> = Mutex::new(());

/// Remembers how a plug-in's run went.
pub fn record_run(name: &str, started: std::time::Instant, ok: bool, note: &str) {
    let Some(file) = health_file() else { return };
    let _guard = HEALTH_LOCK.lock();
    let mut all: serde_json::Map<String, Value> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let h = Health {
        when: crate::forensic::utc(std::time::SystemTime::now()),
        seconds: started.elapsed().as_secs(),
        ok,
        note: note.chars().take(160).collect(),
    };
    if let Ok(v) = serde_json::to_value(h) {
        all.insert(name.to_string(), v);
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, serde_json::to_string_pretty(&all).unwrap_or_default());
}

/// How a plug-in's last run went, if it has run.
pub fn last_run(name: &str) -> Option<Health> {
    let text = std::fs::read_to_string(health_file()?).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get(name)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Where plug-in messages go as they happen, for a live view.
type LogSink = Arc<dyn Fn(&str, &str) + Send + Sync>;
static SINK: RwLock<Option<LogSink>> = RwLock::new(None);

/// Sets who hears what plug-ins say (`plugin name`, `line`) while they run.
pub fn set_log_sink(f: Option<LogSink>) {
    if let Ok(mut s) = SINK.write() {
        *s = f;
    }
}

pub(crate) fn emit_log(plugin: &str, line: &str) {
    if let Ok(s) = SINK.read() {
        if let Some(f) = s.as_ref() {
            f(plugin, line);
        }
    }
}

/// How far a volume plug-in has got, for a live view while it runs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginProgress {
    pub plugin: String,
    /// Position in the volume (bytes), and the volume's size. When the
    /// plug-in says nothing, `done` is the furthest place it has read.
    pub done: u64,
    pub total: u64,
    /// Files it has handed over so far.
    pub files: u64,
    /// What it says it is doing (`{"progress":{"text":...}}`), if anything.
    pub text: String,
    /// The plug-in has stopped working on this volume.
    pub finished: bool,
}

type ProgressSink = Arc<dyn Fn(&PluginProgress) + Send + Sync>;
static PROGRESS_SINK: RwLock<Option<ProgressSink>> = RwLock::new(None);

/// Sets who hears how far volume plug-ins have got while they run.
pub fn set_progress_sink(f: Option<ProgressSink>) {
    if let Ok(mut s) = PROGRESS_SINK.write() {
        *s = f;
    }
}

pub(crate) fn emit_progress(p: &PluginProgress) {
    if let Ok(s) = PROGRESS_SINK.read() {
        if let Some(f) = s.as_ref() {
            f(p);
        }
    }
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The person pressed Stop: every plug-in being waited on is let go.
pub fn request_stop() {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// A new run begins: Stop starts afresh.
pub fn clear_stop() {
    STOP.store(false, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn stopped() -> bool {
    STOP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Waits for `rx` with the person's idle limit, if they set one: with none,
/// it checks every half second whether the run is over and goes on waiting.
pub(crate) fn recv_with_idle<T>(
    rx: &mpsc::Receiver<T>,
    last: &mut std::time::Instant,
) -> Result<T, RecvTimeoutError> {
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(v) => {
                *last = std::time::Instant::now();
                return Ok(v);
            }
            Err(RecvTimeoutError::Timeout) => {
                if stopped() {
                    return Err(RecvTimeoutError::Disconnected);
                }
                if limits().idle.is_some_and(|idle| last.elapsed() > idle) {
                    return Err(RecvTimeoutError::Timeout);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Waits until a run is over, or for as long as the person allows it.
pub(crate) fn wait_for_end(done: &mpsc::Receiver<()>) -> bool {
    match limits().hard {
        // true: the time is up and the plug-in should be stopped.
        Some(hard) => done.recv_timeout(hard).is_err(),
        None => done.recv().is_err(),
    }
}

/// Folders searched for plug-ins: `$REPERIO_PLUGINS`, the user's data folder
/// and `plugins` beside the program.
pub fn folders() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("REPERIO_PLUGINS") {
        out.push(PathBuf::from(p));
    }
    out.extend(user_folder());
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        out.push(dir.join("plugins"));
    }
    out
}

/// The user's home folder (`HOME`, or `USERPROFILE` on Windows).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The folder meant for the user's own plug-ins.
pub fn user_folder() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Some(app) = std::env::var_os("APPDATA") {
            return Some(PathBuf::from(app).join("Reperio").join("plugins"));
        }
    }
    let home = home_dir()?;
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Reperio/plugins")
    } else if cfg!(windows) {
        home.join("AppData/Roaming/Reperio/plugins")
    } else {
        home.join(".local/share/reperio/plugins")
    })
}

/// A plug-in found in a folder.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub path: PathBuf,
    pub name: String,
    /// Kinds of volume wanted, lower case.
    pub fs: Vec<String>,
    /// `volume` (reads a file system, the default) or `analyzer` (a forensic
    /// plug-in run on a phone or an image; see [`crate::analyzer`]).
    pub kind: String,
    /// What an analyzer works on: `android`, `image`.
    pub targets: Vec<String>,
    pub description: String,
    /// The source file this program was built from, for plug-ins dropped in
    /// as `.rs`, `.c`, `.cpp` and the like.
    pub source: Option<PathBuf>,
    /// The plug-in's own version, as it says (`1.2.0`), for showing.
    pub version: String,
    /// The protocol the plug-in speaks (1 if it does not say).
    pub protocol: u32,
    /// Settings the plug-in asks for; Reperio shows a form and passes the
    /// values in its start message as `options`.
    pub options: Vec<PluginOption>,
    /// What the plug-in says it does (`writes to the phone`, `needs network`).
    /// Shown to the person, never enforced.
    pub permissions: Vec<String>,
    /// The Reperio version it needs, as `>=0.2`.
    pub requires: Option<String>,
    /// Who made it, for showing.
    pub author: String,
}

/// The newest plug-in protocol this Reperio speaks.
pub const PROTOCOL: u32 = 1;

/// One setting a plug-in asks for.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginOption {
    pub id: String,
    pub label: String,
    /// `toggle`, `text`, `number` or `choice`.
    pub kind: String,
    pub default: Value,
    /// For `choice`: (value, label).
    pub choices: Vec<(String, String)>,
    pub help: String,
}

/// A file in a plug-in folder that could not be used, and why.
#[derive(Debug, Clone)]
pub struct Problem {
    pub file: String,
    pub reason: String,
}

/// What the plug-in folders hold.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub plugins: Vec<Plugin>,
    pub problems: Vec<Problem>,
}

pub(crate) fn command_for(path: &Path) -> Command {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let interpreter = match ext.as_str() {
        // Windows has `python`, not `python3`.
        "py" => Some(if cfg!(windows) { "python" } else { "python3" }),
        "js" | "mjs" => Some("node"),
        "sh" => Some("sh"),
        "rb" => Some("ruby"),
        "pl" => Some("perl"),
        _ => None,
    };
    match interpreter {
        Some(i) => {
            let mut c = Command::new(i);
            c.arg(path);
            c
        }
        None => Command::new(path),
    }
}

fn candidates(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            p.is_file()
                && !name.starts_with(['_', '.'])
                && !matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "md" | "txt"
                        | "json"
                        | "toml"
                        | "yaml"
                        | "yml"
                        | "pyc"
                        | "log"
                        | "h"
                        | "hpp"
                        | "o"
                        | "a"
                        | "so"
                        | "dylib"
                        | "lock"
                        | "zip"
                        | "png"
                        | "jpg"
                        | "pdf"
                        | "ds_store"
                )
        })
        .collect();
    // A folder with a `plugin.json` naming its `entry` is one plug-in: the entry
    // file runs, and the rest of the folder (modules, data) sits beside it.
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let folder = e.path();
        let name = folder.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !folder.is_dir() || name.starts_with(['_', '.']) {
            continue;
        }
        if let Some(entry) = package_entry(&folder) {
            out.push(entry);
        }
    }
    out.sort();
    out
}

/// The entry file of a folder package, from its `plugin.json`.
pub fn package_entry(folder: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(folder.join("plugin.json")).ok()?;
    let entry = serde_json::from_str::<Value>(&text)
        .ok()?
        .get("entry")?
        .as_str()?
        .to_string();
    let rel = Path::new(&entry);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    let path = folder.join(rel);
    path.is_file().then_some(path)
}

/// Asks one program what it is.
pub(crate) fn probe(path: &Path) -> Result<Plugin, String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("the file")
        .to_string();
    let mut child = command_for(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => {
                format!("is not an executable program. Make it executable (chmod +x {name}), or use a script or a source file (.py, .js, .sh, .rs, .c, .cpp)")
            }
            std::io::ErrorKind::NotFound => {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                match ext {
                    "py" => "needs Python 3 (python3), which was not found".to_string(),
                    "js" | "mjs" => "needs Node (node), which was not found".to_string(),
                    "rb" => "needs Ruby, which was not found".to_string(),
                    "pl" => "needs Perl, which was not found".to_string(),
                    _ => "could not be started".to_string(),
                }
            }
            _ => format!("could not be started: {e}"),
        })?;
    let mut stdin = child.stdin.take().ok_or("could not be started")?;
    let stdout = child.stdout.take().ok_or("could not be started")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).take(1 << 20).read_line(&mut line);
        let _ = tx.send(line);
    });
    let _ = writeln!(stdin, "{}", json!({"cmd": "info"}));
    drop(stdin);
    let line = rx.recv_timeout(Duration::from_secs(10)).ok();
    let _ = child.kill();
    let _ = child.wait();
    let not_one = || {
        "did not answer like a Reperio plug-in (it must reply to {\"cmd\":\"info\"} with an info line; see docs/plugins.md)".to_string()
    };
    let info = serde_json::from_str::<Value>(&line.ok_or_else(not_one)?).map_err(|_| not_one())?;
    let info = info.get("info").ok_or_else(not_one)?;
    let fs = info
        .get("fs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_ascii_lowercase))
                .collect()
        })
        .unwrap_or_else(|| vec!["any".to_string()]);
    let strings = |key: &str| -> Vec<String> {
        info.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_ascii_lowercase))
                    .collect()
            })
            .unwrap_or_default()
    };
    let kind = info
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("volume")
        .to_ascii_lowercase();
    let protocol = info.get("protocol").and_then(Value::as_u64).unwrap_or(1) as u32;
    if protocol > PROTOCOL {
        return Err(format!(
            "needs a newer Reperio (it speaks plug-in protocol {protocol}; this Reperio speaks {PROTOCOL})"
        ));
    }
    let requires = info
        .get("requires")
        .and_then(|r| {
            r.as_str()
                .or_else(|| r.get("reperio").and_then(Value::as_str))
        })
        .map(str::to_string);
    if let Some(req) = &requires {
        if !version_satisfies(env!("CARGO_PKG_VERSION"), req) {
            return Err(format!(
                "needs Reperio {req}; this is Reperio {}",
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
    let text = |key: &str| {
        info.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(200)
            .collect::<String>()
    };
    Ok(Plugin {
        version: text("version"),
        author: text("author"),
        protocol,
        requires,
        permissions: info
            .get("permissions")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.chars().take(120).collect()))
                    .collect()
            })
            .unwrap_or_default(),
        options: info
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(parse_option).collect())
            .unwrap_or_default(),
        kind,
        targets: strings("targets"),
        description: info
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        source: None,
        path: path.to_path_buf(),
        name: info
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| path.file_stem().and_then(|s| s.to_str()))
            .unwrap_or("plugin")
            .to_string(),
        fs,
    })
}

/// Whether `have` (`0.1.0`) meets `need` (`>=0.2`, `0.1`, `>0.1.5`): the
/// numbers are compared part by part. Anything not understood is accepted.
pub fn version_satisfies(have: &str, need: &str) -> bool {
    let need = need.trim();
    let (op, rest) = ["<=", ">=", "<", ">", "="]
        .iter()
        .find_map(|o| need.strip_prefix(o).map(|r| (*o, r.trim())))
        .unwrap_or((">=", need));
    let nums =
        |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.trim().parse().ok()).collect() };
    let (Some(mut a), Some(mut b)) = (nums(have), nums(rest)) else {
        return true;
    };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    match op {
        ">=" => a >= b,
        ">" => a > b,
        "<=" => a <= b,
        "<" => a < b,
        _ => a == b,
    }
}

fn parse_option(o: &Value) -> Option<PluginOption> {
    let id = o.get("id").and_then(Value::as_str)?.to_string();
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let kind = o
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("text")
        .to_ascii_lowercase();
    let choices = o
        .get("choices")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| match c {
                    Value::String(s) => Some((s.clone(), s.clone())),
                    Value::Object(_) => Some((
                        c.get("value").and_then(Value::as_str)?.to_string(),
                        c.get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| c.get("value").and_then(Value::as_str).unwrap_or(""))
                            .to_string(),
                    )),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Some(PluginOption {
        label: o
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_string(),
        default: o.get("default").cloned().unwrap_or(match kind.as_str() {
            "toggle" => json!(false),
            "number" => json!(0),
            _ => json!(""),
        }),
        help: o
            .get("help")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        kind: if ["toggle", "text", "number", "choice"].contains(&kind.as_str()) {
            kind
        } else {
            "text".into()
        },
        choices,
        id,
    })
}

// ---------------------------------------------------------------------------
// Settings chosen for plug-ins
// ---------------------------------------------------------------------------

fn options_file() -> Option<PathBuf> {
    Some(user_folder()?.parent()?.join("plugin-options.json"))
}

/// Settings for this run only (from a recipe), over the person's saved ones.
static OVERRIDES: RwLock<Option<serde_json::Map<String, Value>>> = RwLock::new(None);

/// Sets settings that apply to this run only and are not saved: a map of
/// plug-in name to its options.
pub fn set_option_overrides(o: Option<serde_json::Map<String, Value>>) {
    if let Ok(mut cur) = OVERRIDES.write() {
        *cur = o;
    }
}

/// What a plug-in's settings are now: its defaults, with the person's
/// choices over them. Passed to the plug-in as `options`.
pub fn options_for(p: &Plugin) -> serde_json::Map<String, Value> {
    let saved: Value = options_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let mine = saved.get(&p.name);
    let run = OVERRIDES
        .read()
        .ok()
        .and_then(|o| o.as_ref().and_then(|m| m.get(&p.name).cloned()));
    p.options
        .iter()
        .map(|o| {
            let v = run
                .as_ref()
                .and_then(|r| r.get(&o.id))
                .or_else(|| mine.and_then(|m| m.get(&o.id)))
                .cloned()
                .unwrap_or_else(|| o.default.clone());
            (o.id.clone(), v)
        })
        .collect()
}

/// Remembers the person's settings for a plug-in.
pub fn save_options(name: &str, values: &serde_json::Map<String, Value>) -> std::io::Result<()> {
    let file = options_file().ok_or_else(|| std::io::Error::other("no home folder"))?;
    let mut all: serde_json::Map<String, Value> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    all.insert(name.to_string(), Value::Object(values.clone()));
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, serde_json::to_string_pretty(&all)?)
}

// ---------------------------------------------------------------------------
// Plug-ins dropped in as source code
// ---------------------------------------------------------------------------

/// Where programs built from source plug-ins are kept.
fn build_dir() -> Option<PathBuf> {
    Some(user_folder()?.parent()?.join("plugin-builds"))
}

/// A tool by name: on the PATH, or in the usual places (an app started
/// from the Finder does not have Homebrew's or Rust's folders on its PATH).
fn find_tool(names: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(h) = home_dir() {
        dirs.push(h.join(".cargo/bin"));
        dirs.push(h.join("go/bin"));
    }
    for d in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/usr/local/go/bin",
    ] {
        dirs.push(d.into());
    }
    // On Windows a program is name.exe.
    let suffixes: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    names.iter().find_map(|n| {
        dirs.iter()
            .flat_map(|d| suffixes.iter().map(move |x| d.join(format!("{n}{x}"))))
            .find(|p| p.is_file())
    })
}

/// The program to run for a plug-in file: the file itself, or, for source
/// code (`.rs`, `.c`, ...), what it builds to.
pub fn build_for_test(path: &Path) -> Result<PathBuf, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if recipe(&ext).is_some() {
        build_source(path)
    } else {
        Ok(path.to_path_buf())
    }
}

/// What a source file needs to be built, by extension: the compilers to try,
/// how to say it is missing, and the arguments (`{src}` and `{out}`).
fn recipe(
    ext: &str,
) -> Option<(
    &'static [&'static str],
    &'static str,
    &'static [&'static str],
)> {
    Some(match ext {
        "rs" => (
            &["rustc"],
            "needs the Rust compiler (rustc), which was not found. Install it from https://rustup.rs",
            &["--edition", "2021", "-O", "{src}", "-o", "{out}"],
        ),
        "c" => (
            &["cc", "clang", "gcc"],
            "needs a C compiler, which was not found. On a Mac, run `xcode-select --install`",
            &["-O2", "-o", "{out}", "{src}"],
        ),
        "cpp" | "cc" | "cxx" => (
            &["c++", "clang++", "g++"],
            "needs a C++ compiler, which was not found. On a Mac, run `xcode-select --install`",
            &["-std=c++17", "-O2", "-o", "{out}", "{src}"],
        ),
        "go" => (
            &["go"],
            "needs Go, which was not found. Install it from https://go.dev/dl",
            &["build", "-o", "{out}", "{src}"],
        ),
        "swift" => (
            &["swiftc"],
            "needs the Swift compiler, which was not found. On a Mac, run `xcode-select --install`",
            &["-O", "{src}", "-o", "{out}"],
        ),
        "zig" => (
            &["zig"],
            "needs Zig, which was not found. Install it from https://ziglang.org/download",
            &["build-exe", "-O", "ReleaseFast", "-femit-bin={out}", "{src}"],
        ),
        _ => return None,
    })
}

/// Builds a source plug-in, once per version of the file. The program is
/// kept in the build folder.
fn build_source(src: &Path) -> Result<PathBuf, String> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let (compilers, missing, args) = recipe(&ext).ok_or("is not a type Reperio can build")?;
    let data = std::fs::read(src).map_err(|e| format!("could not be read: {e}"))?;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("plugin");
    let hash = crate::forensic::hash_bytes(&data, Algorithms::parse("md5").expect("known hash"))
        .md5
        .unwrap_or_default();
    let dir = build_dir().ok_or("has nowhere to be built (no home folder)")?;
    let out = dir.join(format!("{stem}-{}", &hash[..12.min(hash.len())]));
    if out.is_file() {
        return Ok(out);
    }
    let compiler = find_tool(compilers).ok_or_else(|| missing.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make the build folder: {e}"))?;
    let log = out.with_extension("log");
    let log_file = File::create(&log).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&compiler);
    for a in args {
        cmd.arg(match *a {
            "{src}" => src.as_os_str().to_owned(),
            "{out}" => out.as_os_str().to_owned(),
            other => match other.split_once("{out}") {
                // e.g. Zig's `-femit-bin={out}`
                Some((before, after)) => {
                    let mut s = std::ffi::OsString::from(before);
                    s.push(out.as_os_str());
                    s.push(after);
                    s
                }
                None => other.into(),
            },
        });
    }
    let mut child = cmd
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", compiler.display()))?;
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if started.elapsed() > Duration::from_secs(240) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("took too long to build".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    };
    if !status.success() || !out.is_file() {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        let tail: String = text
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("did not build:\n{}", tail.trim()));
    }
    Ok(out)
}

/// Which plug-ins may read volumes: `None` is all, a list is only those
/// named. Set by a scan from its options; see
/// [`crate::session::ScanOptions::plugins`].
static ENABLED: RwLock<Option<Vec<String>>> = RwLock::new(None);

pub fn set_enabled(names: Option<Vec<String>>) {
    if let Ok(mut e) = ENABLED.write() {
        *e = names;
    }
}

fn enabled(name: &str) -> bool {
    ENABLED
        .read()
        .map(|e| e.as_ref().is_none_or(|list| list.iter().any(|n| n == name)))
        .unwrap_or(true)
}

/// Plug-ins found a moment ago: asking every plug-in what it is means
/// starting each program, which a scan would otherwise do for every volume.
static FOUND: Mutex<Option<(std::time::Instant, Discovery)>> = Mutex::new(None);
const FOUND_FOR: Duration = Duration::from_secs(4);

/// What is in the plug-in folders: the plug-ins that work, and the files
/// that could not be used, with the reason. Source files are built first.
pub fn discovery() -> Discovery {
    if let Ok(found) = FOUND.lock() {
        if let Some((at, d)) = found.as_ref() {
            if at.elapsed() < FOUND_FOR {
                return d.clone();
            }
        }
    }
    let d = discover_now();
    if let Ok(mut found) = FOUND.lock() {
        *found = Some((std::time::Instant::now(), d.clone()));
    }
    d
}

/// Looks again now, ignoring what was found a moment ago (after an install or
/// a removal).
pub fn rediscover() -> Discovery {
    if let Ok(mut found) = FOUND.lock() {
        *found = None;
    }
    discovery()
}

/// Every plug-in in the plug-in folders.
pub fn discover() -> Vec<Plugin> {
    discovery().plugins
}

fn discover_now() -> Discovery {
    let mut seen = HashSet::new();
    let mut out = Discovery::default();
    for dir in folders() {
        for path in candidates(&dir) {
            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let attempt = if recipe(&ext).is_some() {
                build_source(&path).and_then(|bin| {
                    probe(&bin).map(|p| Plugin {
                        source: Some(path.clone()),
                        ..p
                    })
                })
            } else {
                probe(&path)
            };
            match attempt {
                Ok(p) => {
                    if seen.insert(p.name.clone()) {
                        out.plugins.push(p);
                    }
                }
                Err(reason) => out.problems.push(Problem { file, reason }),
            }
        }
    }
    out
}

/// The plug-ins as volume readers, for [`crate::fs::open`].
pub fn readers() -> Vec<Box<dyn VolumeReader>> {
    discover()
        .into_iter()
        .filter(|p| p.kind == "volume" && enabled(&p.name))
        .map(|p| Box::new(p) as Box<dyn VolumeReader>)
        .collect()
}

fn kind_of(fs: Option<FsType>) -> &'static str {
    match fs {
        Some(FsType::Fat12 | FsType::Fat16 | FsType::Fat32) => "fat",
        Some(FsType::ExFat) => "exfat",
        Some(FsType::Ntfs) => "ntfs",
        Some(FsType::HfsPlus) => "hfs+",
        Some(FsType::Apfs) => "apfs",
        Some(FsType::Ext2 | FsType::Ext3 | FsType::Ext4) => "ext",
        Some(FsType::Other) | None => "unknown",
    }
}

impl VolumeReader for Plugin {
    fn name(&self) -> &'static str {
        "plug-in"
    }

    fn fs(&self) -> FsType {
        FsType::Other
    }

    fn handles(&self, detected: Option<FsType>) -> bool {
        let kind = kind_of(detected);
        self.fs.iter().any(|f| f == "any" || f == kind)
    }

    fn read(&self, src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
        run(self, src, offset, len, "unknown")
    }

    fn read_as(
        &self,
        src: &dyn Source,
        offset: u64,
        len: u64,
        detected: Option<FsType>,
    ) -> Option<Volume> {
        run(self, src, offset, len, kind_of(detected))
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16
            | u32::from(*c.get(1).unwrap_or(&0)) << 8
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn extents(v: &Value, offset: u64, len: u64) -> Option<Vec<Extent>> {
    let mut out = Vec::new();
    for e in v.as_array()? {
        let (o, l) = (e.get(0)?.as_u64()?, e.get(1)?.as_u64()?);
        if l == 0 || o.checked_add(l)? > len {
            return None;
        }
        out.push(Extent::new(offset + o, l));
    }
    Some(out)
}

fn date(v: &Value) -> Option<DateTime> {
    let s = v.as_str()?;
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u16>().ok();
    DateTime::new(
        n(0..4)?,
        n(5..7)? as u8,
        n(8..10)? as u8,
        n(11..13).unwrap_or(0) as u8,
        n(14..16).unwrap_or(0) as u8,
        n(17..19).unwrap_or(0) as u8,
    )
}

pub(crate) fn file_from(v: &Value, offset: u64, len: u64) -> Option<FsFile> {
    let path = v.get("path")?.as_str()?.replace('\\', "/");
    if path.ends_with('/') {
        return None;
    }
    let path = path.trim_matches('/');
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    if name.is_empty() {
        return None;
    }
    let extents = extents(v.get("extents")?, offset, len)?;
    let size = v
        .get("size")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| extents.iter().map(|e| e.len).sum());
    Some(FsFile {
        dir: dir.to_string(),
        name: name.to_string(),
        size,
        modified: v.get("modified").and_then(date),
        created: v.get("created").and_then(date),
        deleted: v.get("deleted").and_then(Value::as_bool).unwrap_or(false),
        extents,
        overwritten: v
            .get("overwritten")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        alternatives: Vec::new(),
    })
}

/// Runs the plug-in on one volume.
fn run(plugin: &Plugin, src: &dyn Source, offset: u64, len: u64, kind: &str) -> Option<Volume> {
    let began = std::time::Instant::now();
    let child = command_for(&plugin.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let child = Arc::new(Mutex::new(child));
    let (mut stdin, stdout) = {
        let mut c = child.lock().ok()?;
        (c.stdin.take()?, c.stdout.take()?)
    };

    // Stops the program if it outlives its limit or the scan ends first.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watched: Arc<Mutex<Child>> = Arc::clone(&child);
    let watchdog = std::thread::spawn(move || {
        if wait_for_end(&done_rx) {
            if let Ok(mut c) = watched.lock() {
                let _ = c.kill();
            }
        }
    });

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let start = json!({"cmd": "scan", "offset": 0, "len": len, "fs": kind, "source_len": src.len(), "options": options_for(plugin)});
    let mut volume = Volume::empty(FsType::Other, offset, len, 4096);
    let started = writeln!(stdin, "{start}").is_ok();
    let mut got_anything = false;
    let mut last_heard = std::time::Instant::now();
    // What the person sees while it works: the plug-in's own words if it has
    // any, otherwise how far into the volume it has read.
    let mut shown = PluginProgress {
        plugin: plugin.name.clone(),
        done: 0,
        total: len,
        files: 0,
        text: String::new(),
        finished: false,
    };
    let mut said_progress = false;
    let mut last_shown = std::time::Instant::now() - Duration::from_secs(1);
    let mut tell = |shown: &PluginProgress, force: bool| {
        if force || last_shown.elapsed() >= Duration::from_millis(200) {
            last_shown = std::time::Instant::now();
            emit_progress(shown);
        }
    };
    tell(&shown, true);
    loop {
        if !started {
            break;
        }
        let line = match recv_with_idle(&rx, &mut last_heard) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(r) = msg.get("read") {
            let (at, n) = (
                r.get("offset").and_then(Value::as_u64),
                r.get("len").and_then(Value::as_u64),
            );
            // With "binary":true the bytes follow the reply line as they are.
            let binary = r.get("binary").and_then(Value::as_bool) == Some(true);
            let mut raw: Option<Vec<u8>> = None;
            if let (Some(at), Some(n), false) = (at, n, said_progress) {
                shown.done = shown.done.max(at.saturating_add(n).min(len));
                tell(&shown, false);
            }
            let reply = match (at, n) {
                (Some(at), Some(n)) if at.checked_add(n).is_some_and(|e| e <= len) => {
                    let mut buf = vec![0u8; n as usize];
                    match src.read_at(offset + at, &mut buf) {
                        Ok(got) => {
                            buf.truncate(got);
                            if binary {
                                let reply = json!({"ok": true, "binary": buf.len()});
                                raw = Some(buf);
                                reply
                            } else {
                                json!({"ok": true, "data": base64(&buf)})
                            }
                        }
                        Err(_) => json!({"ok": false}),
                    }
                }
                _ => json!({"ok": false}),
            };
            if writeln!(stdin, "{reply}").is_err() {
                break;
            }
            if let Some(bytes) = raw {
                if stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        } else if let Some(f) = msg.get("file") {
            if let Some(file) = file_from(f, offset, len) {
                volume.files.push(file);
                got_anything = true;
                shown.files = volume.files.len() as u64;
                tell(&shown, false);
            }
        } else if let Some(pr) = msg.get("progress") {
            // {"progress":{"done":N,"total":N,"text":".."}}, all optional.
            said_progress = true;
            if let Some(t) = pr.get("total").and_then(Value::as_u64).filter(|t| *t > 0) {
                shown.total = t;
            }
            if let Some(d) = pr.get("done").and_then(Value::as_u64) {
                shown.done = d.min(shown.total);
            }
            if let Some(t) = pr.get("text").and_then(Value::as_str) {
                shown.text = t.chars().take(120).collect();
            }
            tell(&shown, false);
        } else if let Some(t) = msg.get("log").and_then(Value::as_str) {
            emit_log(&plugin.name, t);
        } else if let Some(free) = msg.get("free") {
            if let Some(e) = extents(free, offset, len) {
                volume.free.extend(e);
            }
        } else if let Some(d) = msg.get("done") {
            volume.label = d.get("label").and_then(Value::as_str).map(str::to_string);
            break;
        }
    }
    drop(stdin);
    let _ = done_tx.send(());
    if let Ok(mut c) = child.lock() {
        let _ = c.kill();
        let _ = c.wait();
    }
    let _ = watchdog.join();
    shown.finished = true;
    emit_progress(&shown);
    let found = got_anything || !volume.free.is_empty();
    // Most volumes are not the plug-in's: only runs that found something are worth remembering.
    if found {
        record_run(
            &plugin.name,
            began,
            true,
            &format!("read a {kind} volume: {} files", volume.files.len()),
        );
    }
    found.then(|| {
        volume.label.get_or_insert_with(|| plugin.name.clone());
        volume
    })
}

/// Tests that change the limits or press Stop (or depend on them not being
/// pressed) take turns.
#[cfg(test)]
pub(crate) static TEST_GLOBALS: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn files_are_checked_against_the_volume() {
        let ok = json!({"path": "a\\b/c.jpg", "extents": [[0, 10], [100, 5]], "deleted": true,
                        "modified": "2024-05-01T10:20:30"});
        let f = file_from(&ok, 1000, 200).unwrap();
        assert_eq!(
            (f.dir.as_str(), f.name.as_str(), f.size),
            ("a/b", "c.jpg", 15)
        );
        assert_eq!(f.extents[1].offset, 1100);
        assert_eq!(f.modified.unwrap().to_string(), "2024-05-01 10.20.30");
        assert!(file_from(&json!({"path": "x", "extents": [[190, 20]]}), 0, 200).is_none());
        assert!(file_from(&json!({"path": "x", "extents": [[0, 0]]}), 0, 200).is_none());
        assert!(file_from(&json!({"path": "x/", "extents": [[0, 1]]}), 0, 200).is_none());
    }

    /// The shipped example, run for real on a made-up volume.
    #[test]
    fn the_example_plugin_reads_its_toy_volume() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/examples/toyfs.py");
        let plugin = probe(&path).expect("the example answers `info`");
        assert_eq!(plugin.name, "toyfs-python");
        assert!(plugin.handles(None) && !plugin.handles(Some(FsType::Ntfs)));

        let mut img = vec![0u8; 4096];
        img[..8].copy_from_slice(b"TOYFS001");
        img[8..12].copy_from_slice(&1u32.to_le_bytes());
        img[12..12 + 8].copy_from_slice(b"gone.jpg");
        img[52..60].copy_from_slice(&1024u64.to_le_bytes());
        img[60..68].copy_from_slice(&100u64.to_le_bytes());
        img[68..72].copy_from_slice(&1u32.to_le_bytes());
        let v = plugin.read_as(&img, 0, 4096, None).expect("finds the file");
        assert_eq!(v.files.len(), 1);
        assert!(v.files[0].deleted && v.files[0].name == "gone.jpg");
        assert_eq!(v.files[0].extents[0].offset, 1024);
        // Not a TOYFS volume: nothing found, nothing breaks.
        assert!(plugin.read_as(&vec![0u8; 4096], 0, 4096, None).is_none());
    }

    #[test]
    fn versions_are_compared_part_by_part() {
        assert!(version_satisfies("0.1.0", ">=0.1"));
        assert!(version_satisfies("0.10.0", ">=0.9"));
        assert!(!version_satisfies("0.1.0", ">=0.2"));
        assert!(version_satisfies("1.0.0", "1"));
        assert!(version_satisfies("0.1.0", "<0.2"));
        assert!(!version_satisfies("0.2.0", "<0.2"));
        assert!(
            version_satisfies("0.1.0", "whatever"),
            "what is not understood is accepted"
        );
    }

    #[test]
    fn options_are_read_and_bad_ones_dropped() {
        let o = parse_option(&json!({"id":"depth","label":"Depth","type":"choice","choices":["fast",{"value":"deep","label":"Deep"}],"default":"fast","help":"h"})).unwrap();
        assert_eq!(
            (o.kind.as_str(), o.choices.len(), o.choices[1].1.as_str()),
            ("choice", 2, "Deep")
        );
        assert_eq!(
            parse_option(&json!({"id":"t","type":"toggle"}))
                .unwrap()
                .default,
            json!(false)
        );
        assert!(parse_option(&json!({"id":"bad id!"})).is_none());
        assert!(parse_option(&json!({"label":"no id"})).is_none());
        assert_eq!(
            parse_option(&json!({"id":"x","type":"nonsense"}))
                .unwrap()
                .kind,
            "text"
        );
    }

    fn plugin_script(name: &str, info: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reperio-manifest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.py"));
        std::fs::write(
            &path,
            format!("import sys\nsys.stdin.readline()\nprint({info:?})\n"),
        )
        .unwrap();
        path
    }

    #[test]
    fn the_manifest_is_read_and_checked() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let ok = plugin_script(
            "good",
            r#"{"info":{"name":"good","version":"1.2.0","author":"Sam","protocol":1,"requires":">=0.1","permissions":["writes to the phone"],"options":[{"id":"deep","type":"toggle","label":"Deep"}],"kind":"tool","targets":["none"]}}"#,
        );
        let p = probe(&ok).unwrap();
        assert_eq!(
            (p.version.as_str(), p.author.as_str(), p.protocol),
            ("1.2.0", "Sam", 1)
        );
        assert_eq!((p.permissions.len(), p.options.len()), (1, 1));
        let newer = plugin_script("newer", r#"{"info":{"name":"newer","protocol":99}}"#);
        assert!(probe(&newer).unwrap_err().contains("newer Reperio"));
        let wants = plugin_script("wants", r#"{"info":{"name":"wants","requires":">=99.0"}}"#);
        assert!(probe(&wants).unwrap_err().contains("needs Reperio"));
        let plain = plugin_script("plain", r#"{"info":{"name":"plain"}}"#);
        let p = probe(&plain).unwrap();
        assert_eq!(
            (p.protocol, p.options.len(), p.version.as_str()),
            (1, 0, "")
        );
    }

    fn slow_plugin(name: &str, info: &str, seconds: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reperio-slow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.py"));
        let script = format!(
            "import sys, time, json\nmsg = json.loads(sys.stdin.readline())\nif msg['cmd'] == 'info':\n    print({info:?}, flush=True)\nelse:\n    time.sleep({seconds})\n    print(json.dumps({{'opened': {{'size': 4096}}}}) if msg['cmd'] == 'open' else '{{\"done\":{{}}}}', flush=True)\n"
        );
        std::fs::write(&path, script).unwrap();
        path
    }

    fn run_tool(path: &Path) -> crate::analyzer::Report {
        let p = probe(path).unwrap();
        let dir = std::env::temp_dir().join(format!("reperio-slow-out-{}", std::process::id()));
        crate::analyzer::run(
            &p,
            &crate::analyzer::Target {
                device: None,
                image: None,
                backup: None,
            },
            "none",
            &dir,
            &crate::analyzer::RunOptions::default(),
            &mut |_| {},
            &std::sync::atomic::AtomicBool::new(false),
        )
    }

    #[test]
    fn nothing_stops_a_slow_plugin_unless_the_person_asks() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let path = slow_plugin(
            "slowtool",
            r#"{"info":{"name":"slowtool","kind":"tool","targets":["none"]}}"#,
            3,
        );
        set_limits(Limits::default());
        let r = run_tool(&path);
        assert!(r.error.is_none(), "no limit by default: {:?}", r.error);
        assert!(
            last_run("slowtool").is_some_and(|h| h.ok && h.seconds >= 2) || health_file().is_none()
        );
        set_limits(Limits {
            idle: Some(Duration::from_secs(1)),
            hard: None,
        });
        let r = run_tool(&path);
        set_limits(Limits::default());
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("limit you set")),
            "{:?}",
            r.error
        );
    }

    #[test]
    fn stop_lets_go_of_a_source_that_is_waiting() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let path = slow_plugin(
            "slowsource",
            r#"{"info":{"name":"slowsource","kind":"source"}}"#,
            30,
        );
        clear_stop();
        let stopper = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(800));
            request_stop();
        });
        let began = std::time::Instant::now();
        let r = crate::devices::PluginSource::open_program(&path, "any");
        stopper.join().unwrap();
        clear_stop();
        assert!(r.is_err());
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "Stop ended the wait"
        );
    }

    /// A plug-in written with the SDK, in `binary` mode: it reads the image
    /// and serves a device with raw bytes instead of base64.
    fn sdk_plugin(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reperio-bin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/sdk/python/_reperio.py"),
            dir.join("_reperio.py"),
        )
        .unwrap();
        let path = dir.join(format!("{name}.py"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn binary_reads_give_the_same_bytes_as_base64() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        // An analyzer that reads the image both ways and reports whether they agree.
        let path = sdk_plugin(
            "binreader",
            r#"import _reperio as reperio
plugin = reperio.Plugin(name="binreader", kind="analyzer", targets=["image"], binary=True)

@plugin.analyze
def analyze(run):
    big = run.read(1000, 300000)
    run._binary = False
    same = run.read(1000, 300000)
    run.finding("agree" if big == same and len(big) == 300000 else "differ", f"{len(big)} {len(same)} {sum(big)}")

plugin.run()
"#,
        );
        let p = probe(&path).unwrap();
        let image: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let dir = std::env::temp_dir().join(format!("reperio-bin-out-{}", std::process::id()));
        let r = crate::analyzer::run(
            &p,
            &crate::analyzer::Target {
                device: None,
                image: Some(&image),
                backup: None,
            },
            "image",
            &dir,
            &crate::analyzer::RunOptions::default(),
            &mut |_| {},
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].title, "agree", "{:?}", r.findings[0].detail);
    }

    #[test]
    fn a_source_can_serve_raw_bytes() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let path = sdk_plugin(
            "binsource",
            r#"import _reperio as reperio
plugin = reperio.Plugin(name="binsource", kind="source", binary=True)
SIZE = 3 << 20

@plugin.devices
def devices():
    return [reperio.Device("x", "X", SIZE, "drive")]

@plugin.open
def open_device(device_id):
    return reperio.Opened(SIZE, lambda off, n: bytes((off + k) % 251 for k in range(min(n, SIZE - off))), "X")

plugin.run()
"#,
        );
        let src = crate::devices::PluginSource::open_program(&path, "x").unwrap();
        let mut buf = vec![0u8; 2 << 20];
        assert_eq!(src.read_at(1234, &mut buf).unwrap(), buf.len());
        assert!(buf
            .iter()
            .enumerate()
            .all(|(i, &b)| b == ((1234 + i) % 251) as u8));
        let mut tail = vec![0u8; 100];
        assert_eq!(src.read_at(src.len() - 10, &mut tail).unwrap(), 10);
    }

    #[test]
    fn a_reader_can_read_the_volume_in_binary() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let path = sdk_plugin(
            "binvol",
            r#"import _reperio as reperio
plugin = reperio.Plugin(name="binvol", kind="volume", fs=["unknown"], binary=True)

@plugin.scan
def scan(volume):
    data = volume.read(0, 200000)
    if data == bytes(i % 251 for i in range(200000)):
        volume.file("found/ok.bin", 1000, [(10, 1000)], deleted=True)

plugin.run()
"#,
        );
        let p = probe(&path).unwrap();
        let img: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let v = p
            .read_as(&img, 0, img.len() as u64, None)
            .expect("the plug-in read the volume and found its file");
        assert_eq!(v.files.len(), 1);
    }

    /// An analyzer that finds a deleted photo in an image: it becomes a result
    /// with its place and name, checked like any file Reperio found.
    #[test]
    fn an_analyzer_can_offer_a_recoverable_file() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 5) as u8, 90])
        })
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
        let jpeg = jpeg.into_inner();
        let mut disk = vec![0u8; 100_000];
        disk[5000..5000 + jpeg.len()].copy_from_slice(&jpeg);
        let path = sdk_plugin(
            "findfile",
            &format!(
                r#"import _reperio as reperio
plugin = reperio.Plugin(name="findfile", kind="analyzer", targets=["image"])

@plugin.analyze
def analyze(run):
    run.file("Cache/pics/lost.jpg", {size}, [(5000, {size})], modified="2024-05-01T10:20:30")
    run.file("bad/outside.jpg", 10, [(99999999, 10)])
    run.finding("done", "")

plugin.run()
"#,
                size = jpeg.len()
            ),
        );
        let p = probe(&path).unwrap();
        let dir = std::env::temp_dir().join(format!("reperio-found-out-{}", std::process::id()));
        let r = crate::analyzer::run(
            &p,
            &crate::analyzer::Target {
                device: None,
                image: Some(&disk),
                backup: None,
            },
            "image",
            &dir,
            &crate::analyzer::RunOptions::default(),
            &mut |_| {},
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.found.len(), 1, "the file outside the image was refused");
        let items = crate::session::plugin_items(&disk, &r.found, "findfile", 7);
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(
            (it.id, it.name.as_str(), it.dir.as_str()),
            (7, "lost.jpg", "Cache/pics")
        );
        assert_eq!(it.found_by.as_deref(), Some("findfile"));
        assert!(matches!(it.kind, crate::Kind::Photo));
        assert_eq!(it.extents[0].offset, 5000);
    }

    #[test]
    fn zig_source_is_a_build_recipe() {
        let (compilers, _, args) = recipe("zig").expect("zig has a recipe");
        assert_eq!(compilers, &["zig"]);
        assert!(args.iter().any(|a| a.contains("{out}")));
        assert!(args.contains(&"{src}"));
    }

    /// A volume plug-in's own progress, and the fallback (how far it has read),
    /// reach whoever listens while it works.
    #[test]
    fn volume_plugins_report_how_far_they_have_got() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = TEST_GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("reperio-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("walker.py");
        std::fs::write(
            &path,
            r#"import json, sys, time
def send(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m["cmd"] == "info":
        send({"info": {"name": "walker", "kind": "volume", "fs": ["any"]}}); break
    if m["cmd"] == "scan":
        time.sleep(0.25)
        send({"progress": {"done": 1000, "total": 4096, "text": "Carving"}})
        time.sleep(0.25)
        send({"file": {"path": "a.jpg", "size": 10, "deleted": True, "extents": [[0, 10]]}})
        time.sleep(0.25)
        send({"progress": {"done": 4096, "total": 4096, "text": "Carving"}})
        send({"done": {}}); break
"#,
        )
        .unwrap();
        let plugin = probe(&path).expect("answers info");
        let heard: Arc<Mutex<Vec<PluginProgress>>> = Arc::default();
        let sink = Arc::clone(&heard);
        set_progress_sink(Some(Arc::new(move |p: &PluginProgress| {
            sink.lock().unwrap().push(p.clone())
        })));
        let vol = run(&plugin, &vec![0u8; 4096], 0, 4096, "unknown");
        set_progress_sink(None);
        assert_eq!(vol.expect("found a file").files.len(), 1);
        let heard = heard.lock().unwrap();
        assert!(heard
            .iter()
            .any(|p| p.text == "Carving" && p.done == 1000 && p.total == 4096));
        assert!(heard.iter().any(|p| p.files == 1));
        assert!(heard.last().unwrap().finished);
    }
}

//! Forensic plug-ins ("analyzers") and tools: programs of your own, in any
//! language, that Reperio runs without being edited or rebuilt. An analyzer
//! works on an Android phone or a disk image: it looks for things
//! (messages, app data, artefacts), reports findings and writes report
//! files. A tool (`"kind":"tool"`) is any program at all. Both live in the
//! plug-in folders beside the file-system plug-ins; the plug-in says its
//! `"kind"` in its `info` answer.
//!
//! **Plug-ins are your own code and Reperio does not limit them.** They run
//! with your user's permissions on this computer, and they can run any
//! command on a connected phone (including `su` and any `adb` command). What
//! Reperio adds is a record, not a restriction:
//! - **An audit log** (`audit.log`) of every request the plug-in made of
//!   the phone, with times, results and the hash of each file copied.
//! - **Everything it printed** (`console.log`), including its error output.
//! - **Fingerprints.** Every file the plug-in reports is hashed (SHA-256);
//!   `report.json` has the plug-in's own hash and the files' hashes.
//!
//! Protocol (JSON lines, as for file-system plug-ins):
//!
//! Reperio sends `{"cmd":"info"}` and the plug-in answers
//! `{"info":{"name":"...","kind":"analyzer","targets":["android","image"],
//! "description":"..."}}`. To run it, Reperio sends
//! `{"cmd":"analyze","target":"android","device":{...},"workdir":"/abs/path",
//! "args":[...],"adb":"/path/to/adb"}` (`target` is `image` for an image, with
//! `"image_len":N`, or `none` for a tool with no phone or image). The
//! plug-in may then send, one per line:
//!
//! - `{"device":{"op":"shell","cmd":"getprop ro.product.model","root":false}}`
//!   → `{"ok":true,"out":"..."}` or `{"ok":false,"error":"..."}`
//! - `{"device":{"op":"adb","args":["reboot","bootloader"]}}` → runs
//!   `adb -s <phone> <args>`, `{"ok":true,"out":"..."}`
//! - `{"device":{"op":"pull","remote":"/sdcard/x.db","to":"x.db"}}` → copies
//!   the file into `workdir/pulled/x.db`, `{"ok":true,"path":"/abs/..."}`
//! - `{"read":{"offset":N,"len":N}}` (image target) → `{"ok":true,"data":"<base64>"}`
//! - `{"finding":{"title":"...","detail":"...","severity":"info|warn|important|critical","count":N,"tags":["..."],"confidence":"low|medium|high"}}`
//! - `{"output":{"path":"report.csv","title":"...","description":"..."}}`:
//!   a file the plug-in wrote inside `workdir`
//! - `{"progress":{"done":N,"total":N,"text":"..."}}`, `{"log":"..."}`
//! - `{"done":{}}` or `{"error":"..."}`
//!
//! Anything else the plug-in prints (plain text, not JSON) and its error
//! output are kept in `console.log` and shown with the report. A tool that
//! simply exits with status 0 has finished; the files it left in its folder
//! are listed as its output.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::{json, Value};

use crate::forensic::{self, Algorithms, Hasher};
use crate::plugin::{command_for, Plugin};
use crate::source::Source;

// Reperio puts no limit on a plug-in: no cap on what it reads, prints, finds or
// writes, and no time limit unless the person chose one (`plugin::limits`).

/// A phone, as a plug-in reaches it.
pub trait Device: Send + Sync {
    /// What the phone is, shown to the analyzer.
    fn facts(&self) -> Value;
    /// Runs a command on the phone; `root` runs it with `su`.
    fn shell(&self, cmd: &str, root: bool) -> Result<String, String>;
    fn pull(&self, remote: &str, local: &Path) -> Result<(), String>;
    /// Runs `adb -s <phone> <args>` as given.
    fn adb(&self, args: &[String]) -> Result<String, String>;
    /// Where `adb` is.
    fn adb_path(&self) -> Option<String>;
}

/// How to run a plug-in.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Arguments for the plug-in, passed in the start message.
    pub args: Vec<String>,
}

/// A relative path that stays inside the folder it is joined to.
fn inside(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    (!rel.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, Component::Normal(_))))
    .then(|| p.to_path_buf())
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub title: String,
    pub detail: String,
    /// `info`, `warn`, `important` or `critical`.
    pub severity: String,
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Output {
    /// Relative to the plug-in's folder.
    pub path: String,
    pub title: String,
    pub description: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Report {
    pub plugin: String,
    /// SHA-256 of the plug-in's own file, to show which code ran.
    pub plugin_sha256: String,
    pub target: String,
    pub started: String,
    pub finished: String,
    pub dir: String,
    pub findings: Vec<Finding>,
    pub outputs: Vec<Output>,
    pub audit_log: String,
    /// Requests the plug-in made of the device, and how many were refused.
    pub requests: usize,
    pub refused: usize,
    pub error: Option<String>,
    pub cancelled: bool,
    /// What the plug-in printed outside the protocol, and its error output:
    /// the end of it here, all of it in `console_log`.
    pub console: String,
    pub console_log: String,
    pub exit_code: Option<i32>,
    /// Recoverable files the plug-in reported (image targets): they are turned
    /// into results by the caller (`session::plugin_items`).
    #[serde(skip)]
    pub found: Vec<crate::fs::FsFile>,
    /// How many, for showing.
    pub found_count: usize,
}

pub struct Progress<'a> {
    pub done: u64,
    pub total: u64,
    pub text: &'a str,
}

/// What to run an analyzer on.
pub struct Target<'a> {
    pub device: Option<&'a dyn Device>,
    pub image: Option<&'a dyn Source>,
    /// An iPhone backup (target `ios-backup`): files, photos, messages and
    /// deleted rows through `{"backup": {...}}` requests.
    pub backup: Option<&'a crate::ios::Backup>,
}

fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
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
    Ok((h.finish().sha256.unwrap_or_default(), size))
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

/// Plug-ins that analyze (not the file-system ones).
pub fn analyzers() -> Vec<Plugin> {
    crate::plugin::discover()
        .into_iter()
        .filter(|p| p.kind == "analyzer")
        .collect()
}

/// Every plug-in that can be run on `target` (`android`, `image` or `none`):
/// analyzers and tools whose targets include it, or `any`.
pub fn runnable_on(target: &str) -> Vec<Plugin> {
    crate::plugin::discover()
        .into_iter()
        .filter(|p| p.kind == "analyzer" || p.kind == "tool")
        .filter(|p| p.targets.iter().any(|t| t == target || t == "any"))
        .collect()
}

/// Runs one analyzer. Its files, `report.json`, `MANIFEST.sha256.txt` and
/// `audit.log` go in `dest/<plug-in name>/`.
pub fn run(
    plugin: &Plugin,
    target: &Target,
    target_name: &str,
    dest: &Path,
    opts: &RunOptions,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Report {
    let safe: String = plugin
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let dir = dest.join(&safe);
    let mut report = Report {
        plugin: plugin.name.clone(),
        plugin_sha256: sha256_file(&plugin.path).map(|h| h.0).unwrap_or_default(),
        target: target_name.to_string(),
        started: forensic::utc(SystemTime::now()),
        dir: dir.display().to_string(),
        audit_log: dir.join("audit.log").display().to_string(),
        console_log: dir.join("console.log").display().to_string(),
        ..Default::default()
    };
    if let Err(e) = std::fs::create_dir_all(dir.join("pulled")) {
        report.error = Some(e.to_string());
        return report;
    }
    let mut audit = std::fs::File::create(dir.join("audit.log")).ok();
    let plugin_name = plugin.name.clone();
    let run_started = std::time::Instant::now();
    let mut log = |line: String| {
        crate::plugin::emit_log(&plugin_name, &line);
        if let Some(f) = audit.as_mut() {
            let _ = writeln!(f, "{}  {line}", forensic::utc(SystemTime::now()));
        }
    };
    log(format!(
        "plug-in {} (SHA-256 {}) started on {target_name}",
        plugin.name, report.plugin_sha256
    ));
    if !opts.args.is_empty() {
        log(format!("arguments: {:?}", opts.args));
    }

    // What the plug-in prints outside the protocol, and its error output.
    let mut console = std::fs::File::create(dir.join("console.log")).ok();
    let stderr_to = console
        .as_ref()
        .and_then(|f| f.try_clone().ok())
        .map_or_else(Stdio::null, Stdio::from);
    let child = command_for(&plugin.path)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_to)
        .spawn();
    let Ok(child) = child else {
        report.error = Some("The plug-in could not be started.".into());
        return report;
    };
    let child = Arc::new(Mutex::new(child));
    let (mut stdin, stdout) = {
        let mut c = child.lock().expect("child lock");
        (
            c.stdin.take().expect("piped"),
            c.stdout.take().expect("piped"),
        )
    };
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watched = Arc::clone(&child);
    let watchdog = std::thread::spawn(move || {
        if crate::plugin::wait_for_end(&done_rx) {
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

    let mut start = json!({
        "cmd": "analyze",
        "target": target_name,
        "workdir": dir.display().to_string(),
        "args": opts.args,
        "options": crate::plugin::options_for(plugin),
    });
    if let Some(d) = target.device {
        if let Some(path) = d.adb_path() {
            start["adb"] = json!(path);
        }
    }
    if let Some(d) = target.device {
        start["device"] = d.facts();
    }
    if let Some(img) = target.image {
        start["image_len"] = json!(img.len());
    }
    if let Some(b) = target.backup {
        start["backup"] = serde_json::to_value(b.info()).unwrap_or(Value::Null);
    }
    let alive = writeln!(stdin, "{start}").is_ok();
    let mut finished = false;
    let mut last_heard = std::time::Instant::now();
    loop {
        if !alive {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            log("stopped by the user".into());
            break;
        }
        let line = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => {
                if crate::plugin::limits()
                    .idle
                    .is_some_and(|idle| last_heard.elapsed() > idle)
                {
                    report.error = Some("The plug-in went quiet for longer than the limit you set, and was stopped.".into());
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        last_heard = std::time::Instant::now();
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            // Plain text (a print, a progress line): keep it, and show it live.
            if let Some(f) = console.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
            crate::plugin::emit_log(&plugin_name, line.trim_end());
            continue;
        };
        let mut raw: Option<Vec<u8>> = None;
        let reply = if let Some(b) = msg.get("backup") {
            report.requests += 1;
            let (v, bytes) = backup_request(target.backup, b, &dir, &mut log);
            if v.get("ok") == Some(&json!(false)) {
                report.refused += 1;
            }
            raw = bytes;
            Some(v)
        } else if let Some(r) = msg.get("repair") {
            report.requests += 1;
            let v = repair_request(r, &dir, &mut log);
            if v.get("ok") == Some(&json!(false)) {
                report.refused += 1;
            }
            Some(v)
        } else if let Some(d) = msg.get("device") {
            report.requests += 1;
            let r = device_request(target.device, d, &dir, &mut log);
            if r.get("ok") == Some(&json!(false)) {
                report.refused += 1;
            }
            Some(r)
        } else if let Some(r) = msg.get("read") {
            report.requests += 1;
            let (v, bytes) = image_read(target.image, r, &mut log);
            raw = bytes;
            Some(v)
        } else {
            None
        };
        if let Some(reply) = reply {
            if writeln!(stdin, "{reply}").is_err() {
                break;
            }
            // A binary read: the bytes follow the line, as they are.
            if let Some(bytes) = raw.take() {
                if stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            continue;
        }
        if let Some(f) = msg.get("file") {
            // A recoverable file: where its bytes are in the image. It is
            // checked like a file the file system listed and joins the results.
            match (target.image, crate::plugin::file_from(f, 0, target.image.map_or(0, |i| i.len()))) {
                (Some(_), Some(file)) => {
                    log(format!("file {}{} ({} bytes)", file.dir, if file.dir.is_empty() { file.name.clone() } else { format!("/{}", file.name) }, file.size));
                    report.found.push(file);
                }
                (None, _) => log("file ignored: a plug-in can only offer recoverable files from an image or disk (it has no extents on a phone)".into()),
                (_, None) => log("file ignored: it needs a path and extents [[offset, length], ...] that lie inside the image".into()),
            }
        } else if let Some(f) = msg.get("finding") {
            {
                let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let severity = match s("severity").as_str() {
                    "warn" => "warn",
                    "important" => "important",
                    "critical" => "critical",
                    _ => "info",
                };
                let tags: Vec<String> = f
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let confidence = s("confidence");
                let confidence = if confidence.is_empty() {
                    None
                } else {
                    Some(confidence)
                };
                report.findings.push(Finding {
                    title: s("title"),
                    detail: s("detail"),
                    severity: severity.into(),
                    count: f.get("count").and_then(Value::as_u64),
                    tags,
                    confidence,
                });
            }
        } else if let Some(o) = msg.get("output") {
            let rel = o.get("path").and_then(Value::as_str).unwrap_or("");
            let ok = inside(rel).map(|p| dir.join(p)).filter(|p| p.is_file());
            match ok {
                Some(path) => {
                    if let Ok((sha256, size)) = sha256_file(&path) {
                        let s =
                            |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        log(format!("output {rel} ({size} bytes, SHA-256 {sha256})"));
                        report.outputs.push(Output {
                            path: rel.to_string(),
                            title: s("title"),
                            description: s("description"),
                            size,
                            sha256,
                        });
                    }
                }
                _ => log(format!(
                    "output {rel:?} ignored: not a file inside the plug-in's folder"
                )),
            }
        } else if let Some(p) = msg.get("progress") {
            progress(Progress {
                done: p.get("done").and_then(Value::as_u64).unwrap_or(0),
                total: p.get("total").and_then(Value::as_u64).unwrap_or(0),
                text: p.get("text").and_then(Value::as_str).unwrap_or(""),
            });
        } else if let Some(t) = msg.get("log").and_then(Value::as_str) {
            log(format!("plug-in: {t}"));
        } else if let Some(e) = msg.get("error").and_then(Value::as_str) {
            report.error = Some(e.to_string());
            break;
        } else if msg.get("done").is_some() {
            finished = true;
            break;
        }
    }
    drop(stdin);
    let _ = done_tx.send(());
    let mut exited = None;
    if let Ok(mut c) = child.lock() {
        // If it ended by itself, its exit status says how.
        exited = c.try_wait().ok().flatten();
        let _ = c.kill();
        let _ = c.wait();
    }
    let _ = watchdog.join();
    report.exit_code = exited.and_then(|s| s.code());
    // A tool needs no protocol: leaving with status 0 is finishing.
    if !finished && plugin.kind == "tool" && exited.is_some_and(|s| s.success()) {
        finished = true;
    }
    if !finished && report.error.is_none() && !report.cancelled {
        report.error = Some(match report.exit_code {
            Some(code) => format!("The plug-in stopped before it finished (exit status {code})."),
            None => "The plug-in stopped before it finished.".into(),
        });
    }
    // A tool's files: whatever it left in its folder, besides Reperio's own.
    if plugin.kind == "tool" {
        let known: Vec<PathBuf> = report.outputs.iter().map(|o| dir.join(&o.path)).collect();
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    if name != "pulled" {
                        stack.push(path);
                    }
                } else if !known.contains(&path)
                    && !matches!(
                        name.as_str(),
                        "audit.log" | "console.log" | "report.json" | "MANIFEST.sha256.txt"
                    )
                {
                    if let Ok((sha256, size)) = sha256_file(&path) {
                        let rel = path
                            .strip_prefix(&dir)
                            .unwrap_or(&path)
                            .display()
                            .to_string();
                        log(format!("output {rel} ({size} bytes, SHA-256 {sha256})"));
                        report.outputs.push(Output {
                            path: rel,
                            title: String::new(),
                            description: String::new(),
                            size,
                            sha256,
                        });
                    }
                }
            }
        }
    }
    // The end of everything it printed.
    drop(console);
    if let Ok(text) = std::fs::read(dir.join("console.log")) {
        let start = text.len().saturating_sub(6000);
        report.console = String::from_utf8_lossy(&text[start..]).trim().to_string();
    }
    report.finished = forensic::utc(SystemTime::now());
    log(format!(
        "finished: {} findings, {} files, {} requests ({} refused){}",
        report.findings.len(),
        report.outputs.len(),
        report.requests,
        report.refused,
        report
            .error
            .as_deref()
            .map(|e| format!(", error: {e}"))
            .unwrap_or_default()
    ));
    crate::plugin::record_run(
        &plugin.name,
        run_started,
        report.error.is_none() && !report.cancelled,
        &report.error.clone().unwrap_or_else(|| {
            format!(
                "{} findings, {} files{}",
                report.findings.len(),
                report.outputs.len(),
                if report.cancelled { ", stopped" } else { "" }
            )
        }),
    );
    drop(audit);

    // The record of the run, and a fingerprint list of everything in it.
    if let Ok(text) = serde_json::to_string_pretty(&report) {
        let _ = std::fs::write(dir.join("report.json"), text);
    }
    let mut manifest = String::from("# SHA-256  size  path\n");
    for o in &report.outputs {
        manifest += &format!("{}  {}  {}\n", o.sha256, o.size, o.path);
    }
    for name in ["audit.log", "console.log", "report.json"] {
        if let Ok((h, size)) = sha256_file(&dir.join(name)) {
            manifest += &format!("{h}  {size}  {name}\n");
        }
    }
    let _ = std::fs::write(dir.join("MANIFEST.sha256.txt"), manifest);
    report
}

fn device_request(
    device: Option<&dyn Device>,
    d: &Value,
    dir: &Path,
    log: &mut dyn FnMut(String),
) -> Value {
    let refuse = |log: &mut dyn FnMut(String), what: &str, why: String| {
        log(format!("REFUSED {what}: {why}"));
        json!({"ok": false, "error": why})
    };
    let Some(device) = device else {
        return refuse(
            log,
            "device request",
            "there is no phone in this run".into(),
        );
    };
    let op = d.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "shell" => {
            let cmd = d.get("cmd").and_then(Value::as_str).unwrap_or("");
            let root = d.get("root").and_then(Value::as_bool).unwrap_or(false);
            if cmd.trim().is_empty() {
                return refuse(log, "shell", "the command is empty".into());
            }
            match device.shell(cmd, root) {
                Ok(out) => {
                    log(format!(
                        "shell{} {cmd:?} -> {} bytes",
                        if root { " (root)" } else { "" },
                        out.len()
                    ));
                    json!({"ok": true, "out": out})
                }
                Err(e) => {
                    log(format!("shell {cmd:?} failed: {e}"));
                    json!({"ok": false, "error": e})
                }
            }
        }
        "pull" => {
            let remote = d.get("remote").and_then(Value::as_str).unwrap_or("");
            let to = d.get("to").and_then(Value::as_str).unwrap_or("");
            if remote.is_empty() {
                return refuse(log, "pull", "the remote path is empty".into());
            }
            if to.is_empty() {
                return refuse(log, "pull", "the local name is empty".into());
            }
            // A relative name goes in <report folder>/pulled/; an absolute one is used as given.
            let local = if Path::new(to).is_absolute() {
                PathBuf::from(to)
            } else {
                dir.join("pulled").join(to)
            };
            match device.pull(remote, &local) {
                Ok(()) => {
                    let hash = sha256_file(&local).map(|h| h.0).unwrap_or_default();
                    log(format!("pull {remote} -> pulled/{to} (SHA-256 {hash})"));
                    json!({"ok": true, "path": local.display().to_string()})
                }
                Err(e) => {
                    log(format!("pull {remote} failed: {e}"));
                    json!({"ok": false, "error": e})
                }
            }
        }
        "adb" => {
            let args: Vec<String> = d
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if args.is_empty() {
                return refuse(log, "adb", "no arguments".into());
            }
            match device.adb(&args) {
                Ok(out) => {
                    log(format!("adb {args:?} -> {} bytes", out.len()));
                    json!({"ok": true, "out": out})
                }
                Err(e) => {
                    log(format!("adb {args:?} failed: {e}"));
                    json!({"ok": false, "error": e})
                }
            }
        }
        other => refuse(
            log,
            &format!("device op {other:?}"),
            "unknown operation (use shell, adb or pull)".into(),
        ),
    }
}

/// A local path a plug-in names: relative to its working folder, or absolute.
fn local_path(dir: &Path, name: &str) -> PathBuf {
    if Path::new(name).is_absolute() {
        PathBuf::from(name)
    } else {
        dir.join(name)
    }
}

/// Requests about an iPhone backup (target `ios-backup`):
/// `files`, `read`, `export`, `photos`, `messages`, `deleted_rows`.
fn backup_request(
    backup: Option<&crate::ios::Backup>,
    r: &Value,
    dir: &Path,
    log: &mut dyn FnMut(String),
) -> (Value, Option<Vec<u8>>) {
    let no = |why: &str| (json!({"ok": false, "error": why}), None);
    let Some(b) = backup else {
        log("REFUSED backup request: there is no backup in this run".into());
        return no("there is no backup in this run");
    };
    let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
    match text("op") {
        "files" => {
            let limit = r.get("limit").and_then(Value::as_u64).unwrap_or(5000) as usize;
            let domain = r.get("domain").and_then(Value::as_str);
            let files = b.files(domain, text("search"), limit);
            log(format!("backup files {:?} -> {}", text("search"), files.len()));
            (json!({"ok": true, "files": files}), None)
        }
        "read" => {
            let Some(path) = b.stored(text("id")) else { return no("that file is not in the backup") };
            let (at, n) = (r.get("offset").and_then(Value::as_u64).unwrap_or(0), r.get("len").and_then(Value::as_u64));
            let len = std::fs::metadata(&path).map_or(0, |m| m.len());
            let n = n.unwrap_or(len.saturating_sub(at)).min(len.saturating_sub(at));
            let mut buf = vec![0u8; n as usize];
            let ok = std::fs::File::open(&path).and_then(|mut f| {
                use std::io::{Read, Seek, SeekFrom};
                f.seek(SeekFrom::Start(at))?;
                f.read_exact(&mut buf)
            });
            if ok.is_err() {
                return no("the file could not be read");
            }
            if r.get("binary").and_then(Value::as_bool) == Some(true) {
                (json!({"ok": true, "binary": buf.len()}), Some(buf))
            } else {
                (json!({"ok": true, "data": base64(&buf)}), None)
            }
        }
        "export" => {
            let Some(from) = b.stored(text("id")) else { return no("that file is not in the backup") };
            let to = text("to");
            if to.is_empty() {
                return no("`to` is empty");
            }
            let local = if Path::new(to).is_absolute() { PathBuf::from(to) } else { dir.join("pulled").join(to) };
            if let Some(parent) = local.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&from, &local) {
                Ok(_) => {
                    log(format!("backup export {} -> {}", text("id"), local.display()));
                    (json!({"ok": true, "path": local.display().to_string()}), None)
                }
                Err(e) => no(&e.to_string()),
            }
        }
        "photos" => match b.photos() {
            Ok(p) => (json!({"ok": true, "photos": p}), None),
            Err(e) => no(&e),
        },
        "messages" => match b.messages(text("search"), r.get("limit").and_then(Value::as_u64).unwrap_or(100_000) as usize) {
            Ok(m) => (json!({"ok": true, "messages": m}), None),
            Err(e) => no(&e),
        },
        "deleted_rows" => match b.deleted_rows(text("domain"), text("path")) {
            Ok((rows, _)) => {
                log(format!("backup deleted_rows {} -> {}", text("path"), rows.len()));
                (json!({"ok": true, "rows": rows}), None)
            }
            Err(e) => no(&e),
        },
        other => no(&format!("unknown backup operation {other:?} (files, read, export, photos, messages, deleted_rows)")),
    }
}

/// Runs Reperio's repair on a local file for a plug-in: photos, videos and
/// documents that will not open. The repaired copy is written beside it, in
/// `repaired/` of the plug-in's folder.
fn repair_request(r: &Value, dir: &Path, log: &mut dyn FnMut(String)) -> Value {
    let name = r.get("path").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return json!({"ok": false, "error": "`path` is empty"});
    }
    let path = local_path(dir, name);
    let src = match crate::source::DeviceSource::open(&path) {
        Ok(s) => s,
        Err(e) => {
            return json!({"ok": false, "error": format!("could not open {}: {e}", path.display())})
        }
    };
    let ext = r
        .get("ext")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            path.extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_default();
    let reference = r
        .get("reference")
        .and_then(Value::as_str)
        .and_then(|p| crate::source::DeviceSource::open(&local_path(dir, p)).ok());
    match crate::fixfile::repair(&src, &ext, reference.as_ref().map(|s| s as &dyn Source)) {
        Ok(fixed) => {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            let out_ext = fixed.ext().map(str::to_string).unwrap_or(ext);
            let out_dir = dir.join("repaired");
            let _ = std::fs::create_dir_all(&out_dir);
            let out = out_dir.join(format!("{stem} (repaired).{out_ext}"));
            let written = std::fs::File::create(&out).and_then(|f| {
                let mut w = std::io::BufWriter::new(f);
                let bad = fixed.write_to(&src, &mut w)?;
                w.flush()?;
                Ok(bad)
            });
            match written {
                Ok(bad) => {
                    log(format!(
                        "repair {} -> {} ({})",
                        path.display(),
                        out.display(),
                        fixed.describe()
                    ));
                    json!({"ok": true, "path": out.display().to_string(), "how": fixed.describe(), "unreadable_sectors": bad})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
        Err(e) => {
            log(format!("repair {} failed: {e}", path.display()));
            json!({"ok": false, "error": e})
        }
    }
}

/// Answers a plug-in's read of the image: base64 in the reply, or, if it
/// asked with `"binary":true`, the bytes after the reply line. Returns the
/// reply and those bytes.
fn image_read(
    image: Option<&dyn Source>,
    r: &Value,
    log: &mut dyn FnMut(String),
) -> (Value, Option<Vec<u8>>) {
    let Some(src) = image else {
        log("REFUSED read: there is no image in this run".into());
        return (
            json!({"ok": false, "error": "there is no image in this run"}),
            None,
        );
    };
    let (at, n) = (
        r.get("offset").and_then(Value::as_u64),
        r.get("len").and_then(Value::as_u64),
    );
    let binary = r.get("binary").and_then(Value::as_bool) == Some(true);
    match (at, n) {
        (Some(at), Some(n)) if at.checked_add(n).is_some_and(|e| e <= src.len()) => {
            let mut buf = vec![0u8; n as usize];
            match src.read_at(at, &mut buf) {
                Ok(got) => {
                    buf.truncate(got);
                    if binary {
                        (json!({"ok": true, "binary": buf.len()}), Some(buf))
                    } else {
                        (json!({"ok": true, "data": base64(&buf)}), None)
                    }
                }
                Err(_) => (json!({"ok": false}), None),
            }
        }
        _ => (
            json!({"ok": false, "error": "offset and len must lie inside the image"}),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_names_stay_inside() {
        assert!(inside("a/b.db").is_some());
        assert!(inside("../x").is_none());
        assert!(inside("/etc/passwd").is_none());
        assert!(inside("a/../../x").is_none());
        assert!(inside("").is_none());
    }
}

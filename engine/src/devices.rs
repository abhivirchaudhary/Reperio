//! Device plug-ins: recover from anything you can write code to reach.
//!
//! Reperio reads disks, cards and images itself. A *source* plug-in teaches
//! it to reach something else: an iPhone over USB, a camera over MTP, a
//! network share, a cloud bucket, a device behind a vendor tool, a dump made
//! by hardware Reperio has never heard of. The plug-in decides *how* the
//! bytes are fetched; Reperio does everything after that, the same as for a
//! disk: reads the file systems it knows (and any reader plug-ins), carves
//! free space, checks and repairs files, previews and recovers them.
//!
//! The plug-in only ever answers "give me N bytes at offset X", so it cannot
//! write through Reperio, and Reperio never asks it to.
//!
//! The plug-in says what it is, with `"kind":"source"`:
//!
//! ```text
//! {"info":{"name":"ios-usb","kind":"source","description":"iPhone over USB"}}
//! ```
//!
//! **Which devices.** Reperio sends `{"cmd":"devices"}`; the plug-in answers
//! with one line and exits:
//!
//! ```text
//! {"devices":[{"id":"00008030-0012","name":"Sam's iPhone","size":128000000000,
//!              "kind":"phone","detail":"iOS 17, trusted"}]}
//! ```
//!
//! `kind` is `card`, `usb`, `phone`, `camera`, `network`, `drive` or anything
//! else (shown as "Device"). `detail` is optional.
//!
//! **Open one.** Reperio starts the plug-in again and sends
//! `{"cmd":"open","device":"<id>"}`. The plug-in answers with
//! `{"opened":{"size":N,"label":"Sam's iPhone"}}` (or `{"error":"why"}`) and
//! then serves reads until its input closes. For each
//! `{"read":{"offset":N,"len":N}}` it answers one line,
//! `{"ok":true,"data":"<base64>"}` or `{"ok":false,"error":"why"}`. Reads are
//! at most 4 MB. Answering with less than asked is allowed only at the end.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::plugin::{self, command_for, Plugin};
use crate::source::Source;

/// How much is asked of a plug-in in one read. This is how Reperio breaks up
/// a big read, not a limit on the plug-in.
const CHUNK: usize = 4 << 20;
// There is no time limit on a plug-in unless the person set one
// (`plugin::limits`): opening may wait on a tap on the phone, and a slow
// device may take its time.

/// A device a source plug-in can reach.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginDevice {
    /// The plug-in that serves it.
    pub plugin: String,
    /// Its own name for the device.
    pub id: String,
    pub name: String,
    pub size: u64,
    pub kind: String,
    pub detail: String,
}

/// Devices found a moment ago: asking means starting every source plug-in.
static LISTED: Mutex<Option<(Instant, Vec<PluginDevice>)>> = Mutex::new(None);
const LISTED_FOR: Duration = Duration::from_secs(5);

/// Every device any source plug-in can reach right now.
pub fn list() -> Vec<PluginDevice> {
    if let Ok(l) = LISTED.lock() {
        if let Some((at, d)) = l.as_ref() {
            if at.elapsed() < LISTED_FOR {
                return d.clone();
            }
        }
    }
    let sources = plugin::discover()
        .into_iter()
        .filter(|p| p.kind == "source")
        .collect::<Vec<_>>();
    // Ask them all at once: one slow plug-in must not hold up the others.
    let handles = sources
        .into_iter()
        .map(|p| std::thread::spawn(move || list_from(&p.name, &p.path)))
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    for h in handles {
        if let Ok(devices) = h.join() {
            out.extend(devices);
        }
    }
    if let Ok(mut l) = LISTED.lock() {
        *l = Some((Instant::now(), out.clone()));
    }
    out
}

/// What one plug-in program reaches. A plug-in that fails shows nothing.
pub fn list_from(plugin: &str, program: &Path) -> Vec<PluginDevice> {
    let Ok(mut proc) = Proc::start(program) else {
        return Vec::new();
    };
    let reply = proc
        .send(&json!({"cmd": "devices"}))
        .ok()
        .and_then(|()| proc.line().ok());
    drop(proc);
    let Some(v) = reply.and_then(|l| serde_json::from_str::<Value>(&l).ok()) else {
        return Vec::new();
    };
    v.get("devices")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let id = d.get("id").and_then(Value::as_str)?.to_string();
                    let text = |k: &str| {
                        d.get(k)
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect::<String>()
                    };
                    Some(PluginDevice {
                        plugin: plugin.to_string(),
                        name: if text("name").is_empty() {
                            id.clone()
                        } else {
                            text("name")
                        },
                        size: d.get("size").and_then(Value::as_u64).unwrap_or(0),
                        kind: text("kind").to_ascii_lowercase(),
                        detail: text("detail"),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A running plug-in program, spoken to in lines of JSON.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    /// A line from the plug-in, and the raw bytes that followed it, if it said
    /// (`"binary":N`) and the plug-in is in binary mode.
    lines: Receiver<io::Result<(String, Vec<u8>)>>,
    binary: Arc<AtomicBool>,
}

impl Proc {
    fn start(program: &Path) -> io::Result<Self> {
        let mut child = command_for(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("no output"))?;
        let (tx, lines) = mpsc::channel();
        let binary = Arc::new(AtomicBool::new(false));
        let in_binary = Arc::clone(&binary);
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match r.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        // In binary mode a reply that says `"binary":N` is followed
                        // by N bytes, as they are.
                        let mut data = Vec::new();
                        if in_binary.load(Ordering::Relaxed) {
                            let n = serde_json::from_str::<Value>(&line)
                                .ok()
                                .and_then(|v| v.get("binary").and_then(Value::as_u64));
                            if let Some(n) = n {
                                data = vec![0u8; n as usize];
                                if let Err(e) = r.read_exact(&mut data) {
                                    let _ = tx.send(Err(e));
                                    break;
                                }
                            }
                        }
                        if tx.send(Ok((line, data))).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            binary,
        })
    }

    fn send(&mut self, v: &Value) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("the plug-in has stopped"))?;
        writeln!(stdin, "{v}")?;
        stdin.flush()
    }

    /// The next line from the plug-in. With no limit set it waits for as long
    /// as it takes; with the person's idle limit it gives up after that.
    fn line(&mut self) -> io::Result<String> {
        self.frame().map(|(l, _)| l)
    }

    /// The next line and the bytes after it.
    fn frame(&mut self) -> io::Result<(String, Vec<u8>)> {
        let began = Instant::now();
        loop {
            // Look up every half second: Stop, or the person's idle limit.
            match self.lines.recv_timeout(Duration::from_millis(500)) {
                Ok(r) => return r,
                Err(RecvTimeoutError::Timeout) => {
                    if plugin::stopped() {
                        return Err(io::Error::other("stopped"));
                    }
                    if plugin::limits()
                        .idle
                        .is_some_and(|idle| began.elapsed() > idle)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the plug-in said nothing for longer than the limit you set",
                        ));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("the plug-in stopped answering"))
                }
            }
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A device served by a plug-in, as a [`Source`]: Reperio reads it like a disk.
pub struct PluginSource {
    proc: Mutex<Proc>,
    len: u64,
    label: String,
}

impl PluginSource {
    /// Opens `device` through the source plug-in called `plugin`.
    pub fn open(plugin: &str, device: &str) -> Result<Self, String> {
        let found: Plugin = plugin::discover()
            .into_iter()
            .find(|p| p.kind == "source" && p.name == plugin)
            .ok_or_else(|| format!("The plug-in \"{plugin}\" is not installed."))?;
        Self::open_program_with(
            &found.path,
            device,
            Value::Object(plugin::options_for(&found)),
        )
    }

    /// Opens `device` through the plug-in program at `program`.
    pub fn open_program(program: &Path, device: &str) -> Result<Self, String> {
        Self::open_program_with(program, device, Value::Null)
    }

    /// As [`Self::open_program`], passing the plug-in its settings.
    pub fn open_program_with(program: &Path, device: &str, options: Value) -> Result<Self, String> {
        let mut proc =
            Proc::start(program).map_err(|e| format!("The plug-in could not be started: {e}"))?;
        proc.send(&json!({"cmd": "open", "device": device, "options": options}))
            .map_err(|e| format!("The plug-in did not accept the request: {e}"))?;
        let line = proc.line().map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&line)
            .map_err(|_| "The plug-in did not answer like a source plug-in.".to_string())?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            return Err(e.to_string());
        }
        let opened = v
            .get("opened")
            .ok_or("The plug-in did not say the device was open.")?;
        // A plug-in that says `"binary":true` answers reads with raw bytes
        // after the reply line instead of base64 inside it.
        proc.binary.store(
            opened.get("binary").and_then(Value::as_bool) == Some(true),
            Ordering::Relaxed,
        );
        let len = opened
            .get("size")
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .ok_or("The plug-in did not say how big the device is.")?;
        let label = opened
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(device)
            .chars()
            .take(200)
            .collect();
        Ok(Self {
            proc: Mutex::new(proc),
            len,
            label,
        })
    }

    /// The device's own name for itself.
    pub fn label(&self) -> &str {
        &self.label
    }

    fn fetch(&self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
        let mut proc = self
            .proc
            .lock()
            .map_err(|_| io::Error::other("the plug-in failed"))?;
        proc.send(&json!({"read": {"offset": offset, "len": out.len()}}))?;
        let (line, raw) = proc.frame()?;
        let v: Value = serde_json::from_str(&line)
            .map_err(|_| io::Error::other("the plug-in sent something that was not JSON"))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let why = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("read failed");
            return Err(io::Error::other(why.to_string()));
        }
        let bytes = if proc.binary.load(Ordering::Relaxed) {
            raw
        } else {
            let data = v.get("data").and_then(Value::as_str).unwrap_or("");
            unbase64(data).ok_or_else(|| io::Error::other("the plug-in sent bad data"))?
        };
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }
}

impl Source for PluginSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min((self.len - offset) as usize);
        let mut done = 0;
        while done < want {
            let step = (want - done).min(CHUNK);
            match self.fetch(offset + done as u64, &mut buf[done..done + step]) {
                Ok(0) => break,
                Ok(n) => done += n,
                // Bytes already read are good; report the failure next time.
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn base64_decodes() {
        assert_eq!(unbase64("").unwrap(), b"");
        assert_eq!(unbase64("Zg==").unwrap(), b"f");
        assert_eq!(unbase64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(unbase64("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(unbase64("Zm9v!").is_none());
    }

    /// The shipped example, run for real: listed, opened and read.
    #[test]
    fn the_example_source_plugin_serves_a_device() {
        let _g = crate::plugin::TEST_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/examples/source_file.py");
        let devices = list_from("source-file-example", &path);
        assert_eq!(devices.len(), 1, "one device listed");
        assert_eq!(devices[0].kind, "phone");
        let src = PluginSource::open_program(&path, &devices[0].id).expect("opens");
        assert_eq!(src.len(), devices[0].size);
        let mut buf = vec![0u8; 5000];
        assert_eq!(src.read_at(100, &mut buf).unwrap(), 5000);
        // The example's device is a repeating counter: byte i is i % 251.
        assert!(buf
            .iter()
            .enumerate()
            .all(|(i, &b)| b == ((100 + i) % 251) as u8));
        // Reads past the end are short, not errors.
        let mut tail = vec![0u8; 100];
        assert_eq!(src.read_at(src.len() - 10, &mut tail).unwrap(), 10);
        assert!(PluginSource::open_program(&path, "no-such-device").is_err());
    }
}

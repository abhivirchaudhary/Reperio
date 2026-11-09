//! `reperio plugin test`: does a plug-in follow the protocol?
//!
//! It starts the plug-in, reads its manifest, and for its kind runs it on
//! made-up data (an empty volume, an empty image, or a device it lists) and
//! checks that what it says is well formed and what it reads is consistent.
//! It cannot say whether the plug-in finds the right things: only that
//! Reperio can talk to it. Plug-ins that need a phone are not run.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::analyzer::{self, RunOptions, Target};
use crate::devices::{self, PluginSource};
use crate::fs::VolumeReader;
use crate::plugin::{self, Limits, Plugin};
use crate::source::Source;

/// One thing checked.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn pass(out: &mut Vec<Check>, name: &str, detail: impl Into<String>) {
    out.push(Check {
        name: name.into(),
        ok: true,
        detail: detail.into(),
    });
}

fn fail(out: &mut Vec<Check>, name: &str, detail: impl Into<String>) {
    out.push(Check {
        name: name.into(),
        ok: false,
        detail: detail.into(),
    });
}

/// Checks the plug-in program at `path`. Nothing is written outside a scratch
/// folder, and a plug-in that hangs is stopped after 30 seconds.
pub fn check(path: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let p = match plugin::probe(path) {
        Ok(p) => p,
        Err(e) => {
            fail(&mut out, "Starts and answers `info`", e);
            return out;
        }
    };
    pass(
        &mut out,
        "Starts and answers `info`",
        format!(
            "{} {}(kind: {}, protocol {})",
            p.name,
            if p.version.is_empty() {
                String::new()
            } else {
                format!("{} ", p.version)
            },
            p.kind,
            p.protocol
        ),
    );
    manifest(&p, &mut out);

    // A plug-in that hangs must not hang the test.
    let before = plugin::limits();
    plugin::set_limits(Limits {
        idle: Some(Duration::from_secs(30)),
        hard: Some(Duration::from_secs(120)),
    });
    match p.kind.as_str() {
        "volume" => volume(&p, &mut out),
        "analyzer" | "tool" => analyze(&p, &mut out),
        "source" => source(&p, &mut out),
        other => fail(
            &mut out,
            "Kind",
            format!("\"{other}\" is not a plug-in kind (volume, analyzer, tool, source)"),
        ),
    }
    plugin::set_limits(before);
    out
}

fn manifest(p: &Plugin, out: &mut Vec<Check>) {
    if p.name.trim().is_empty() {
        fail(out, "Has a name", "the manifest has no `name`");
    } else {
        pass(out, "Has a name", p.name.clone());
    }
    match p.kind.as_str() {
        "volume" if p.fs.is_empty() => {
            fail(out, "Says which file systems it reads", "`fs` is empty")
        }
        "analyzer" if p.targets.is_empty() => fail(
            out,
            "Says what it works on",
            "`targets` is empty (android, image, none)",
        ),
        "tool" if p.targets.is_empty() => pass(
            out,
            "Says what it works on",
            "no `targets`: it will be offered as a tool with nothing connected",
        ),
        _ => {}
    }
    if p.description.is_empty() {
        pass(
            out,
            "Has a description",
            "none (add one so people know what it is for)",
        );
    }
}

fn volume(p: &Plugin, out: &mut Vec<Check>) {
    let empty = vec![0u8; 1 << 20];
    for (label, data) in [
        ("an empty volume", empty.clone()),
        (
            "noise",
            (0..1 << 20)
                .map(|i: u32| (i.wrapping_mul(2654435761) >> 24) as u8)
                .collect(),
        ),
    ] {
        let began = std::time::Instant::now();
        let got = p.read_as(&data, 0, data.len() as u64, None);
        match got {
            None => pass(
                out,
                &format!("Scans {label}"),
                format!(
                    "found nothing, as it should ({} ms)",
                    began.elapsed().as_millis()
                ),
            ),
            Some(v) => pass(
                out,
                &format!("Scans {label}"),
                format!(
                    "reported {} files and {} free areas",
                    v.files.len(),
                    v.free.len()
                ),
            ),
        }
    }
}

fn analyze(p: &Plugin, out: &mut Vec<Check>) {
    let dest = std::env::temp_dir().join(format!("reperio-plugin-test-{}", std::process::id()));
    let image = vec![0u8; 1 << 20];
    let wants = |t: &str| p.targets.iter().any(|x| x == t || x == "any");
    let mut ran = false;
    let mut runs: Vec<(&str, Target)> = Vec::new();
    if wants("none") || p.targets.is_empty() {
        runs.push((
            "nothing connected",
            Target {
                device: None,
                image: None,
                backup: None,
            },
        ));
    }
    if wants("image") {
        runs.push((
            "an empty image",
            Target {
                device: None,
                image: Some(&image),
                backup: None,
            },
        ));
    }
    for (label, target) in &runs {
        ran = true;
        let name = if *label == "an empty image" {
            "image"
        } else {
            "none"
        };
        let r = analyzer::run(
            p,
            target,
            name,
            &dest,
            &RunOptions::default(),
            &mut |_| {},
            &AtomicBool::new(false),
        );
        let title = format!("Runs on {label}");
        match &r.error {
            Some(e) => fail(out, &title, e.clone()),
            None => pass(
                out,
                &title,
                format!(
                    "{} findings, {} files{}",
                    r.findings.len(),
                    r.outputs.len(),
                    if r.refused > 0 {
                        format!(", {} requests refused (see audit.log)", r.refused)
                    } else {
                        String::new()
                    }
                ),
            ),
        }
    }
    if wants("android") {
        pass(
            out,
            "Runs on a phone",
            "not run: this needs a connected Android phone (try `reperio android analyze`)",
        );
    }
    if wants("ios-backup") {
        pass(
            out,
            "Runs on an iPhone backup",
            "not run: this needs a backup (try `reperio ios analyze BACKUP DEST --plugin NAME`)",
        );
    }
    if !ran && !wants("android") && !wants("ios-backup") {
        fail(
            out,
            "Has something to run on",
            "`targets` has none of android, image, ios-backup, none",
        );
    }
    let _ = std::fs::remove_dir_all(dest);
}

fn source(p: &Plugin, out: &mut Vec<Check>) {
    let devices = devices::list_from(&p.name, &p.path);
    let Some(d) = devices.first() else {
        pass(
            out,
            "Lists devices",
            "none right now: connect a device and test again to check opening and reading",
        );
        return;
    };
    let bad = devices.iter().find(|d| d.id.is_empty() || d.size == 0);
    match bad {
        Some(b) => fail(
            out,
            "Lists devices",
            format!("\"{}\" has no id or no size", b.name),
        ),
        None => pass(
            out,
            "Lists devices",
            format!(
                "{}: {}",
                devices.len(),
                devices
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    }
    let src = match PluginSource::open_program(&p.path, &d.id) {
        Ok(s) => s,
        Err(e) => {
            fail(out, "Opens a device", e);
            return;
        }
    };
    pass(
        out,
        "Opens a device",
        format!("{} ({} bytes)", src.label(), src.len()),
    );
    let n = src.len();
    let mut a = vec![0u8; 4096.min(n as usize)];
    let mut b = vec![0u8; 8192.min(n as usize)];
    let mut c = vec![0u8; 4096.min(n as usize)];
    let read = |at: u64, buf: &mut [u8]| src.read_at(at, buf);
    match (read(0, &mut a), read(0, &mut b)) {
        (Ok(x), Ok(y)) if x == a.len() && y == b.len() => {
            if b[..a.len()] == a[..] {
                pass(
                    out,
                    "Reads the start twice the same",
                    format!("{} bytes", y),
                );
            } else {
                fail(
                    out,
                    "Reads the start twice the same",
                    "two reads of the same bytes differ",
                );
            }
        }
        (x, y) => fail(
            out,
            "Reads the start",
            format!("short or failed read: {x:?} / {y:?}"),
        ),
    }
    if n > 8192 {
        let mut d1 = vec![0u8; 4096];
        let mut d2 = vec![0u8; 4096];
        let mut whole = vec![0u8; 8192];
        match (
            read(4096, &mut d1),
            read(8192, &mut d2),
            read(4096, &mut whole),
        ) {
            (Ok(4096), Ok(4096), Ok(8192)) if [d1.as_slice(), d2.as_slice()].concat() == whole => {
                pass(
                    out,
                    "Reads agree wherever they start",
                    "two 4 KB reads equal one 8 KB read",
                )
            }
            r => fail(out, "Reads agree wherever they start", format!("{r:?}")),
        }
    }
    let last = n.saturating_sub(c.len() as u64);
    match read(last, &mut c) {
        Ok(got) if got == c.len() => pass(out, "Reads the end", format!("{} bytes at {last}", got)),
        r => fail(out, "Reads the end", format!("{r:?}")),
    }
    let mut past = [0u8; 16];
    match src.read_at(n, &mut past) {
        Ok(0) => pass(out, "A read past the end is empty", "0 bytes"),
        r => fail(out, "A read past the end is empty", format!("{r:?}")),
    }
}

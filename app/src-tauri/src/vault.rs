//! The Recovery Vault in the app: which drives are protected, keeping
//! their records fresh, and naming files after a scan.
//!
//! Records live in the app's data folder, one per protected drive, named
//! after the drive's volume ID. They are refreshed when the app starts,
//! every 15 minutes while it runs, and on macOS by a small background task
//! (a launch agent) whenever a drive is mounted and every hour, so a card's
//! files are recorded as soon as it is inserted, before anything is
//! deleted from it.

use std::path::{Path, PathBuf};

use reperio_engine::session::Item;
use reperio_engine::vault::Vault;
use reperio_engine::Source;
use serde::Serialize;

/// Flag the background task runs the app with.
pub const REFRESH_FLAG: &str = "--vault-refresh";

/// A drive mounted now that can be protected.
#[derive(Debug, Clone)]
pub struct Mounted {
    pub id: String,
    pub label: String,
    pub mount: PathBuf,
}

/// A drive as shown in Settings.
#[derive(Debug, Clone, Serialize)]
pub struct VaultDrive {
    pub id: String,
    pub label: String,
    pub mounted: bool,
    pub protected: bool,
    /// Seconds since 1970, 0 if never.
    pub updated: i64,
    pub files: usize,
}

/// The folder holding the records.
pub fn dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
    };
    Some(base.join("app.reperio.recovery").join("vault"))
}

fn record_path(id: &str) -> Option<PathBuf> {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(dir()?.join(format!("{safe}.json")))
}

fn records() -> Vec<Vault> {
    let Some(dir) = dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Vault::load(&e.path()).ok())
        .collect()
}

/// Removable and external drives mounted now.
#[cfg(target_os = "macos")]
pub fn mounted() -> Vec<Mounted> {
    use plist::Value;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/Volumes") else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        // The startup disk appears here as a link to /.
        if e.file_type().map(|t| t.is_symlink()).unwrap_or(true) {
            continue;
        }
        let Ok(o) = std::process::Command::new("diskutil")
            .args(["info", "-plist"])
            .arg(&path)
            .output()
        else {
            continue;
        };
        let Some(info) = Value::from_reader_xml(&o.stdout[..])
            .ok()
            .and_then(Value::into_dictionary)
        else {
            continue;
        };
        let get = |k: &str| {
            info.get(k)
                .and_then(Value::as_string)
                .unwrap_or("")
                .to_string()
        };
        let internal = info
            .get("Internal")
            .and_then(Value::as_boolean)
            .unwrap_or(false);
        let id = [get("VolumeUUID"), get("DiskUUID")]
            .into_iter()
            .find(|s| !s.is_empty());
        let (false, Some(id)) = (internal, id) else {
            continue;
        };
        let label = get("VolumeName");
        out.push(Mounted {
            id,
            label,
            mount: path,
        });
    }
    out
}

#[cfg(target_os = "linux")]
pub fn mounted() -> Vec<Mounted> {
    let Ok(o) = std::process::Command::new("lsblk")
        .args(["-J", "-o", "UUID,LABEL,MOUNTPOINT,RM,HOTPLUG"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack: Vec<&serde_json::Value> =
        v["blockdevices"].as_array().into_iter().flatten().collect();
    while let Some(d) = stack.pop() {
        stack.extend(d["children"].as_array().into_iter().flatten());
        let flag = |k: &str| d[k].as_bool().unwrap_or(false) || d[k].as_str() == Some("1");
        let (Some(id), Some(mount)) = (d["uuid"].as_str(), d["mountpoint"].as_str()) else {
            continue;
        };
        if flag("rm") || flag("hotplug") {
            out.push(Mounted {
                id: id.to_string(),
                label: d["label"].as_str().unwrap_or("").to_string(),
                mount: PathBuf::from(mount),
            });
        }
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn mounted() -> Vec<Mounted> {
    Vec::new()
}

/// Mounted drives, plus protected drives not mounted now.
pub fn drives() -> Vec<VaultDrive> {
    let recs = records();
    let mut out: Vec<VaultDrive> = mounted()
        .into_iter()
        .map(|m| {
            let rec = recs.iter().find(|r| r.volume == m.id);
            VaultDrive {
                label: if m.label.is_empty() {
                    "Untitled".into()
                } else {
                    m.label
                },
                protected: rec.is_some(),
                updated: rec.map_or(0, |r| r.updated),
                files: rec.map_or(0, |r| r.files.len()),
                mounted: true,
                id: m.id,
            }
        })
        .collect();
    for r in &recs {
        if !out.iter().any(|d| d.id == r.volume) {
            out.push(VaultDrive {
                id: r.volume.clone(),
                label: r.label.clone(),
                mounted: false,
                protected: true,
                updated: r.updated,
                files: r.files.len(),
            });
        }
    }
    out
}

/// Starts or stops protecting a drive. Starting records it right away.
pub fn protect(id: &str, on: bool) -> Result<(), String> {
    let path = record_path(id).ok_or("No folder for the Recovery Vault")?;
    if !on {
        let _ = std::fs::remove_file(&path);
        sync_agent();
        return Ok(());
    }
    let m = mounted()
        .into_iter()
        .find(|m| m.id == id)
        .ok_or("That drive is no longer connected.")?;
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    let blank = Vault {
        volume: m.id.clone(),
        label: m.label.clone(),
        ..Default::default()
    };
    let rec = blank.refresh(&m.mount).map_err(|e| e.to_string())?;
    rec.save(&path).map_err(|e| e.to_string())?;
    sync_agent();
    Ok(())
}

/// Refreshes the record of every protected drive mounted now.
pub fn refresh_all() -> usize {
    let mounted = mounted();
    let mut n = 0;
    for rec in records() {
        let Some(m) = mounted.iter().find(|m| m.id == rec.volume) else {
            continue;
        };
        let Some(path) = record_path(&rec.volume) else {
            continue;
        };
        let mut fresh = match rec.refresh(&m.mount) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !m.label.is_empty() {
            fresh.label = m.label.clone();
        }
        // A drive that suddenly looks empty was most likely formatted or
        // emptied: that is exactly the record that must not be lost.
        if fresh.files.is_empty() && !rec.files.is_empty() {
            continue;
        }
        if fresh.save(&path).is_ok() {
            n += 1;
        }
    }
    n
}

/// Names files found by the deep scan from every record. Returns the IDs
/// of the items renamed.
pub fn name_found(src: &dyn Source, items: &mut [Item]) -> Vec<u32> {
    let before: Vec<(String, String)> = items
        .iter()
        .map(|i| (i.dir.clone(), i.name.clone()))
        .collect();
    for rec in records() {
        rec.name_found_files(src, items);
    }
    items
        .iter()
        .zip(before)
        .filter(|(i, (d, n))| &i.dir != d || &i.name != n)
        .map(|(i, _)| i.id)
        .collect()
}

/// Installs the background task while any drive is protected, and
/// removes it when none is (macOS).
#[cfg(target_os = "macos")]
pub fn sync_agent() {
    use std::process::Command;
    const LABEL: &str = "app.reperio.vault";
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let plist = home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .output();
    if records().is_empty() {
        let _ = std::fs::remove_file(&plist);
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>{REFRESH_FLAG}</string></array>
  <key>StartOnMount</key><true/>
  <key>StartInterval</key><integer>3600</integer>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>Nice</key><integer>10</integer>
</dict>
</plist>
"#,
        exe.display()
    );
    let _ = std::fs::create_dir_all(plist.parent().unwrap_or(Path::new(".")));
    if std::fs::write(&plist, xml).is_ok() {
        let _ = Command::new("launchctl")
            .args(["bootstrap", &target])
            .arg(&plist)
            .output();
    }
}

#[cfg(not(target_os = "macos"))]
pub fn sync_agent() {}

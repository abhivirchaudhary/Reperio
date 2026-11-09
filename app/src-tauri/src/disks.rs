//! Lists the drives attached to the computer, so people pick "SD card,
//! 32 GB" instead of typing device paths.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Disk {
    /// Stable identifier passed back to open the disk (`disk4`, `sdb`).
    pub id: String,
    /// The device to read, preferring the fast raw node on macOS.
    pub device: String,
    pub name: String,
    pub size: u64,
    /// "card", "usb", "external" or "internal".
    pub kind: String,
    pub volumes: Vec<String>,
    pub mounted: bool,
}

pub fn list() -> Vec<Disk> {
    let mut disks = platform::list();
    // Cards and removable drives first: they are what people recover from.
    let rank = |d: &Disk| match d.kind.as_str() {
        "card" => 0,
        "usb" => 1,
        "external" => 2,
        _ => 3,
    };
    disks.sort_by_key(|d| (rank(d), d.id.clone()));
    disks
}

/// Unmounts every volume of a disk so nothing writes to it while we read
/// (Spotlight and Finder would otherwise add files to the card).
pub fn unmount(id: &str) -> Result<(), String> {
    platform::unmount(id)
}

/// Ejects a disk so it can be removed safely.
pub fn eject(id: &str) -> Result<(), String> {
    platform::eject(id)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Disk;
    use plist::{Dictionary, Value};
    use std::process::Command;

    fn diskutil_plist(args: &[&str]) -> Option<Dictionary> {
        let out = Command::new("diskutil").args(args).output().ok()?;
        Value::from_reader_xml(&out.stdout[..])
            .ok()?
            .into_dictionary()
    }

    pub fn list() -> Vec<Disk> {
        let Some(all) = diskutil_plist(&["list", "-plist", "physical"]) else {
            return Vec::new();
        };
        let Some(disks) = all.get("AllDisksAndPartitions").and_then(Value::as_array) else {
            return Vec::new();
        };
        disks
            .iter()
            .filter_map(|d| {
                let d = d.as_dictionary()?;
                let id = d.get("DeviceIdentifier")?.as_string()?.to_string();
                let info = diskutil_plist(&["info", "-plist", &id])?;
                let get_bool = |k: &str| info.get(k).and_then(Value::as_boolean).unwrap_or(false);
                let get_str = |k: &str| {
                    info.get(k)
                        .and_then(Value::as_string)
                        .unwrap_or("")
                        .to_string()
                };
                let protocol = get_str("BusProtocol");
                let internal = get_bool("Internal");
                // USB card readers report the USB bus, but name the card.
                let media = get_str("MediaName").to_ascii_uppercase();
                let looks_like_card = ["SD", "CARD", "MMC", "READER", "XQD", "CFEXPRESS"]
                    .iter()
                    .any(|w| media.contains(w));
                let kind = if protocol.contains("Secure Digital")
                    || get_str("MediaType").contains("SD")
                    || (!internal && looks_like_card)
                {
                    "card"
                } else if internal {
                    "internal"
                } else if protocol == "USB" {
                    "usb"
                } else {
                    "external"
                };
                let mut volumes = Vec::new();
                let mut mounted = false;
                let parts = d
                    .get("Partitions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for p in parts.iter().filter_map(Value::as_dictionary) {
                    if let Some(name) = p.get("VolumeName").and_then(Value::as_string) {
                        volumes.push(name.to_string());
                    }
                    mounted |= p.get("MountPoint").is_some();
                }
                if let Some(name) = d.get("VolumeName").and_then(Value::as_string) {
                    volumes.push(name.to_string());
                    mounted |= d.get("MountPoint").is_some();
                }
                let size = info
                    .get("TotalSize")
                    .or_else(|| info.get("Size"))
                    .and_then(Value::as_unsigned_integer)
                    .unwrap_or(0);
                let mut name = get_str("MediaName").trim().to_string();
                if name.is_empty() {
                    name = "Disk".into();
                }
                Some(Disk {
                    device: format!("/dev/r{id}"),
                    id,
                    name,
                    size,
                    kind: kind.into(),
                    volumes,
                    mounted,
                })
            })
            .collect()
    }

    fn diskutil(args: &[&str]) -> Result<(), String> {
        let out = Command::new("diskutil")
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    pub fn unmount(id: &str) -> Result<(), String> {
        diskutil(&["unmountDisk", &format!("/dev/{id}")])
    }

    pub fn eject(id: &str) -> Result<(), String> {
        diskutil(&["eject", &format!("/dev/{id}")])
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Disk;
    use serde_json::Value;
    use std::process::Command;

    pub fn list() -> Vec<Disk> {
        let Ok(out) = Command::new("lsblk")
            .args([
                "-J",
                "-b",
                "-o",
                "NAME,PATH,SIZE,MODEL,TRAN,RM,HOTPLUG,TYPE,LABEL,MOUNTPOINT",
            ])
            .output()
        else {
            return Vec::new();
        };
        let Ok(json) = serde_json::from_slice::<Value>(&out.stdout) else {
            return Vec::new();
        };
        let Some(devices) = json["blockdevices"].as_array() else {
            return Vec::new();
        };
        let s = |v: &Value| v.as_str().unwrap_or("").trim().to_string();
        let flag = |v: &Value| v.as_bool().unwrap_or(false) || v.as_str() == Some("1");
        devices
            .iter()
            .filter(|d| matches!(d["type"].as_str(), Some("disk" | "loop")))
            .filter(|d| d["size"].as_u64().unwrap_or(0) > 0)
            .map(|d| {
                let id = s(&d["name"]);
                let tran = s(&d["tran"]);
                let removable = flag(&d["rm"]) || flag(&d["hotplug"]);
                let kind = if id.starts_with("mmcblk") {
                    "card"
                } else if tran == "usb" {
                    "usb"
                } else if removable || id.starts_with("loop") {
                    "external"
                } else {
                    "internal"
                };
                let children = d["children"].as_array().cloned().unwrap_or_default();
                let all = std::iter::once(d).chain(children.iter());
                let volumes: Vec<String> = all
                    .clone()
                    .map(|c| s(&c["label"]))
                    .filter(|l| !l.is_empty())
                    .collect();
                let mounted = all.into_iter().any(|c| !c["mountpoint"].is_null());
                let model = s(&d["model"]);
                let name = if model.is_empty() {
                    if id.starts_with("loop") {
                        "Disk image".into()
                    } else {
                        "Disk".into()
                    }
                } else {
                    model
                };
                Disk {
                    device: s(&d["path"]),
                    id,
                    name,
                    size: d["size"].as_u64().unwrap_or(0),
                    kind: kind.into(),
                    volumes,
                    mounted,
                }
            })
            .collect()
    }

    pub fn unmount(id: &str) -> Result<(), String> {
        // Unmount each mounted partition; udisks lets desktop users do this.
        let out = Command::new("lsblk")
            .args(["-ln", "-o", "PATH,MOUNTPOINT", &format!("/dev/{id}")])
            .output()
            .map_err(|e| e.to_string())?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut f = line.split_whitespace();
            if let (Some(path), Some(_mount)) = (f.next(), f.next()) {
                let ok = Command::new("udisksctl")
                    .args(["unmount", "-b", path])
                    .status()
                    .is_ok_and(|s| s.success())
                    || Command::new("umount")
                        .arg(path)
                        .status()
                        .is_ok_and(|s| s.success());
                if !ok {
                    return Err(format!("could not unmount {path}"));
                }
            }
        }
        Ok(())
    }

    pub fn eject(id: &str) -> Result<(), String> {
        let ok = Command::new("udisksctl")
            .args(["power-off", "-b", &format!("/dev/{id}")])
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            Ok(())
        } else {
            Err("could not eject".into())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::Disk;
    pub fn list() -> Vec<Disk> {
        Vec::new()
    }
    pub fn unmount(_: &str) -> Result<(), String> {
        Ok(())
    }
    pub fn eject(_: &str) -> Result<(), String> {
        Err("not supported on this system".into())
    }
}

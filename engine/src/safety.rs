//! Guards against the one mistake that destroys recoverable files: writing
//! the recovered copies onto the card being recovered.

use std::path::{Path, PathBuf};

/// Fails if `output` lives on the same physical disk as `source`.
pub fn check_output(source: &Path, output: &Path) -> Result<(), String> {
    let Some(src_disk) = source.to_str().and_then(whole_disk) else {
        return Ok(()); // an image file, not a device
    };
    let Some(out_dev) = mount_device(output) else {
        return Ok(());
    };
    if out_dev.to_str().and_then(whole_disk).as_deref() == Some(src_disk.as_str()) {
        return Err(format!(
            "{} is on the disk being recovered ({}). Saving there would overwrite the files you are \
             trying to get back. Choose a folder on another drive.",
            output.display(),
            source.display()
        ));
    }
    Ok(())
}

/// Reduces a device path to its whole-disk name: `/dev/rdisk4s1` -> `disk4`,
/// `/dev/sdb1` -> `sdb`, `/dev/mmcblk0p1` -> `mmcblk0`.
fn whole_disk(path: &str) -> Option<String> {
    let name = path.strip_prefix("/dev/")?;
    let name = name
        .strip_prefix('r')
        .filter(|n| n.starts_with("disk"))
        .unwrap_or(name);
    if let Some(rest) = name.strip_prefix("disk") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return (!digits.is_empty()).then(|| format!("disk{digits}"));
    }
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() < name.len() {
        // mmcblk0p1 / nvme0n1p2: the partition follows a 'p' after a digit.
        if let Some(base) = trimmed.strip_suffix('p') {
            if base.ends_with(|c: char| c.is_ascii_digit()) {
                return Some(base.to_string());
            }
        }
        if name.starts_with("mmcblk") || name.starts_with("nvme") || name.starts_with("loop") {
            return Some(name.to_string());
        }
        return Some(trimmed.to_string());
    }
    Some(name.to_string())
}

/// The device of the filesystem `path` would be written to.
fn mount_device(path: &Path) -> Option<PathBuf> {
    // The output folder may not exist yet; use its nearest existing ancestor.
    let abs = std::path::absolute(path).ok()?;
    let existing = abs.ancestors().find(|p| p.exists())?.canonicalize().ok()?;
    platform_mount_device(&existing)
}

#[cfg(target_os = "macos")]
fn platform_mount_device(path: &Path) -> Option<PathBuf> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statfs fills the zeroed struct; f_mntfromname is NUL-terminated.
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let from = CStr::from_ptr(st.f_mntfromname.as_ptr());
        Some(PathBuf::from(from.to_str().ok()?))
    }
}

#[cfg(target_os = "linux")]
fn platform_mount_device(path: &Path) -> Option<PathBuf> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?, f.next()?))
        })
        .filter(|(_, mnt)| path.starts_with(mnt))
        .max_by_key(|(_, mnt)| mnt.len())
        .map(|(dev, _)| PathBuf::from(dev))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_mount_device(_: &Path) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_disk_names() {
        for (path, disk) in [
            ("/dev/rdisk4", "disk4"),
            ("/dev/disk4s1", "disk4"),
            ("/dev/disk12", "disk12"),
            ("/dev/sdb", "sdb"),
            ("/dev/sdb1", "sdb"),
            ("/dev/mmcblk0", "mmcblk0"),
            ("/dev/mmcblk0p1", "mmcblk0"),
            ("/dev/nvme0n1p2", "nvme0n1"),
        ] {
            assert_eq!(whole_disk(path).as_deref(), Some(disk), "{path}");
        }
        assert_eq!(whole_disk("card.img"), None);
    }
}

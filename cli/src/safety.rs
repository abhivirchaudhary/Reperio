//! Ownership fix-ups for recovery run under sudo. The same-disk check lives
//! in the engine.

use std::path::Path;

use anyhow::Result;

/// Fails if `output` lives on the same physical disk as `source`.
pub fn check_output(source: &Path, output: &Path) -> Result<()> {
    reperio_engine::safety::check_output(source, output).map_err(anyhow::Error::msg)
}

/// Recovery runs under sudo to read raw devices, which would leave every
/// recovered file owned by root. Hand them back to the user who ran sudo so
/// they can open, move and delete them normally.
pub fn give_to_user(path: &Path) {
    #[cfg(unix)]
    {
        let id = |var| std::env::var(var).ok().and_then(|v| v.parse::<u32>().ok());
        if let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) {
            let _ = std::os::unix::fs::chown(path, Some(uid), Some(gid));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Hands a whole recovered folder tree back to the user who ran sudo.
pub fn give_tree_to_user(root: &Path) {
    give_to_user(root);
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                give_tree_to_user(&path);
            } else {
                give_to_user(&path);
            }
        }
    }
}

//! Opening raw disks, with administrator rights when needed.
//!
//! The app first tries to open the device directly (which works when the
//! user already has access). Otherwise it starts a copy of itself in helper
//! mode through the system's own password prompt: `osascript ... with
//! administrator privileges` on macOS, `pkexec` on Linux. The helper only
//! serves read-only reads of that device; see `reperio_engine::remote`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use reperio_engine::{DeviceSource, Source};

/// The first argument that switches the executable into helper mode.
pub const HELPER_FLAG: &str = "--reperio-helper";

/// Entry point in helper mode: `<exe> --reperio-helper <device> <socket>`.
/// Everything it prints goes to a log the app reads, so a failure here is
/// shown to the user instead of leaving the app waiting.
#[cfg(unix)]
pub fn run(args: &[String]) -> ! {
    // SAFETY: geteuid has no preconditions.
    eprintln!("reperio helper: started (uid {})", unsafe {
        libc::geteuid()
    });
    let code = match args {
        [device, socket] if is_device(Path::new(device)) => {
            if let Err(e) = DeviceSource::open(Path::new(device)) {
                eprintln!("reperio helper error: could not open {device}: {e}");
                std::process::exit(1);
            }
            match reperio_engine::remote::serve(Path::new(device), Path::new(socket)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("reperio helper error: could not talk to Reperio over {socket}: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("reperio helper error: expected a device under /dev and a socket path, got {args:?}");
            2
        }
    };
    std::process::exit(code)
}

#[cfg(not(unix))]
pub fn run(_: &[String]) -> ! {
    std::process::exit(2)
}

/// The helper only ever reads devices, never arbitrary files.
#[cfg(unix)]
fn is_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.starts_with("/dev/")
        && std::fs::metadata(path)
            .is_ok_and(|m| m.file_type().is_block_device() || m.file_type().is_char_device())
}

/// Opens `device` for reading, asking for the administrator password if
/// the user cannot read it directly.
pub fn open_device(device: &Path, cancel: &AtomicBool) -> io::Result<Arc<dyn Source>> {
    match DeviceSource::open(device) {
        Ok(src) => Ok(Arc::new(src)),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => privileged(device, cancel),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn privileged(device: &Path, cancel: &AtomicBool) -> io::Result<Arc<dyn Source>> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    // A private directory, so no other user can reach the socket. Kept
    // short: socket paths are limited to 104 bytes on macOS.
    let dir = private_dir()?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = dir.join("s");
    let log = dir.join("helper.log");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let exe = std::env::current_exe()?;

    let mut child = launch(&exe, device, &socket, &log)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let helper_error = |log: &str| {
        log.lines()
            .find_map(|l| l.strip_prefix("reperio helper error: "))
            .map(str::to_string)
    };
    let started = Instant::now();
    let result = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                break reperio_engine::remote::RemoteSource::handshake(stream)
                    .map(|s| Arc::new(s) as Arc<dyn Source>);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => break Err(e),
        }
        if let Some(e) = helper_error(&read_log()) {
            let _ = child.kill();
            break Err(io::Error::other(explain(&e)));
        }
        // The helper runs inside the password step, so that step ending
        // before the helper connected means it was cancelled or failed.
        if let Ok(Some(status)) = child.try_wait() {
            let mut msg = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = io::Read::read_to_string(&mut err, &mut msg);
            }
            let log = read_log();
            break Err(
                if msg.contains("-128")
                    || msg.to_lowercase().contains("cancel")
                    || status.code() == Some(126)
                {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "The password prompt was cancelled.",
                    )
                } else if let Some(e) = helper_error(&log) {
                    io::Error::other(explain(&e))
                } else {
                    io::Error::other(format!(
                        "The disk reader stopped before it could start (exit {status}). {} {}",
                        msg.trim(),
                        log.trim()
                    ))
                },
            );
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            break Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled."));
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let log = read_log();
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                if log.contains("started") {
                    format!(
                        "The disk reader started but never connected. Details: {}",
                        log.trim()
                    )
                } else {
                    "The disk reader never started after the password was entered.".to_string()
                },
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // The helper keeps running while the app reads the disk; it exits on
    // its own when the app closes the connection.
    let _ = std::fs::remove_file(&socket);
    if result.is_err() {
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_dir(&dir);
    }
    result
}

/// Turns the helper's error into advice. macOS refuses raw access to
/// removable disks, even for an administrator, unless the app has Full Disk
/// Access; the system reports that as "Operation not permitted".
#[cfg(unix)]
fn explain(error: &str) -> String {
    if cfg!(target_os = "macos") && error.contains("Operation not permitted") {
        format!(
            "{FULL_DISK_ACCESS} macOS needs your permission before any app can read a memory card directly. \
             Turn on Reperio in System Settings → Privacy & Security → Full Disk Access, then quit and reopen Reperio. ({error})"
        )
    } else {
        format!("Reperio could not read the disk: {error}")
    }
}

/// Marks errors the app answers with a button to the privacy settings.
pub const FULL_DISK_ACCESS: &str = "Full Disk Access needed.";

#[cfg(not(unix))]
fn privileged(_: &Path, _: &AtomicBool) -> io::Result<Arc<dyn Source>> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Run Reperio as administrator to read disks.",
    ))
}

#[cfg(unix)]
fn private_dir() -> io::Result<PathBuf> {
    let base = PathBuf::from("/tmp");
    for i in 0..100u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = base.join(format!("rp-{}-{nanos:x}-{i}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not create a private directory"))
}

#[cfg(target_os = "macos")]
fn launch(
    exe: &Path,
    device: &Path,
    socket: &Path,
    log: &Path,
) -> io::Result<std::process::Command> {
    // Arguments go through `argv` and `quoted form of`, so no path is ever
    // interpolated into the script text. The helper runs in the foreground
    // of the password step, so the app sees at once if it stops.
    let script = [
        "on run argv",
        "do shell script (quoted form of item 1 of argv) & \" --reperio-helper \" & (quoted form of item 2 of argv) & \" \" & (quoted form of item 3 of argv) & \" > \" & (quoted form of item 5 of argv) & \" 2>&1\" with prompt (item 4 of argv) with administrator privileges",
        "end run",
    ];
    let mut cmd = std::process::Command::new("osascript");
    for line in script {
        cmd.args(["-e", line]);
    }
    cmd.arg(exe).arg(device).arg(socket).arg(
        "Reperio needs your password to read the disk directly. It only reads: nothing on the disk will be changed.",
    ).arg(log);
    Ok(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch(
    exe: &Path,
    device: &Path,
    socket: &Path,
    log: &Path,
) -> io::Result<std::process::Command> {
    // pkexec cannot redirect, so a shell inside it does.
    let mut cmd = std::process::Command::new("pkexec");
    cmd.arg("/bin/sh")
        .arg("-c")
        .arg("exec \"$0\" --reperio-helper \"$1\" \"$2\" > \"$3\" 2>&1")
        .arg(exe)
        .arg(device)
        .arg(socket)
        .arg(log);
    Ok(cmd)
}

//! Installing, removing and keeping track of plug-ins.
//!
//! **Install** puts a plug-in (a file, a folder with a `plugin.json`, a `.zip`
//! of either, or a URL to one) in the plug-ins folder, checks it starts and
//! says what it is, and remembers its fingerprint. If you give the SHA-256 it
//! should have, a file that differs is not installed.
//!
//! **Trust** here is only a record, never a gate: Reperio notes each plug-in's
//! SHA-256 the first time it sees it, and says when one is *new* (you have not
//! looked at it) or *changed* (it is not the file you had). A plug-in runs
//! either way, and nothing is blocked, because the code is yours. The record
//! is there so a swapped file does not go unnoticed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::plugin::{self, Plugin};

const PY_SDK: &str = include_str!("../../plugins/sdk/python/_reperio.py");
const JS_SDK: &str = include_str!("../../plugins/sdk/js/_reperio.js");

/// What an install did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Installed {
    pub name: String,
    pub kind: String,
    pub version: String,
    pub permissions: Vec<String>,
    pub sha256: String,
    pub path: String,
    /// It replaced a plug-in of the same name.
    pub updated: bool,
}

pub fn sha256_of(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The file whose fingerprint stands for a plug-in: its source if it was built.
fn identity(p: &Plugin) -> &Path {
    p.source.as_deref().unwrap_or(&p.path)
}

// ---------------------------------------------------------------------------
// The record of what has been seen
// ---------------------------------------------------------------------------

fn trust_file() -> Option<PathBuf> {
    Some(plugin::user_folder()?.parent()?.join("plugin-trust.json"))
}

static LOCK: Mutex<()> = Mutex::new(());

fn load() -> Option<serde_json::Map<String, Value>> {
    serde_json::from_str::<Value>(&std::fs::read_to_string(trust_file()?).ok()?)
        .ok()?
        .as_object()
        .cloned()
}

fn store(all: &serde_json::Map<String, Value>) {
    if let Some(f) = trust_file() {
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(f, serde_json::to_string_pretty(all).unwrap_or_default());
    }
}

fn entry(sha: &str, source: &str) -> Value {
    json!({"sha256": sha, "when": crate::forensic::utc(std::time::SystemTime::now()), "source": source})
}

/// The first time the record is used, every plug-in already there is taken as
/// known: only what arrives afterwards is *new*.
fn baseline() {
    let _g = LOCK.lock();
    if trust_file().is_some_and(|f| f.exists()) {
        return;
    }
    let mut all = serde_json::Map::new();
    for p in plugin::discover() {
        if let Ok(sha) = sha256_of(identity(&p)) {
            all.insert(p.name.clone(), entry(&sha, "already installed"));
        }
    }
    store(&all);
}

/// `new` (not seen before), `known` (the file you had) or `changed` (it is not
/// the file you had), and its SHA-256.
pub fn trust_state(p: &Plugin) -> (&'static str, String) {
    baseline();
    let sha = sha256_of(identity(p)).unwrap_or_default();
    let _g = LOCK.lock();
    let state = match load().and_then(|a| {
        a.get(&p.name)
            .and_then(|e| e.get("sha256"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }) {
        None => "new",
        Some(seen) if seen == sha => "known",
        Some(_) => "changed",
    };
    (state, sha)
}

/// Marks this file as the one you know.
pub fn trust(p: &Plugin) {
    baseline();
    let sha = sha256_of(identity(p)).unwrap_or_default();
    let _g = LOCK.lock();
    let mut all = load().unwrap_or_default();
    all.insert(p.name.clone(), entry(&sha, "acknowledged"));
    store(&all);
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

fn safe_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    n.trim_start_matches(['.', '_', '-']).to_string()
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        let kind = e.file_type()?;
        if kind.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::metadata(p) {
        let mut perm = m.permissions();
        perm.set_mode(perm.mode() | 0o755);
        let _ = std::fs::set_permissions(p, perm);
    }
}
#[cfg(not(unix))]
fn make_executable(_: &Path) {}

/// Installs a plug-in from `src`: a path or an http(s) URL, to a file, a
/// folder with a `plugin.json`, or a `.zip` of either. `expected_sha256`, if
/// given, must match the file (or `.zip`) fetched, or nothing is installed.
pub fn install(src: &str, expected_sha256: Option<&str>) -> Result<Installed, String> {
    baseline();
    let dest_dir = plugin::user_folder().ok_or("There is no home folder to install into.")?;
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("Could not make {}: {e}", dest_dir.display()))?;
    let scratch = std::env::temp_dir().join(format!(
        "reperio-install-{}-{}",
        std::process::id(),
        crate::forensic::utc(std::time::SystemTime::now()).replace(':', "")
    ));
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let result = install_from(src, expected_sha256, &dest_dir, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn install_from(
    src: &str,
    expected: Option<&str>,
    dest_dir: &Path,
    scratch: &Path,
) -> Result<Installed, String> {
    // 1. Get it.
    let mut local = if src.starts_with("http://") || src.starts_with("https://") {
        let name = safe_name(
            src.split(['?', '#'])
                .next()
                .unwrap_or(src)
                .rsplit('/')
                .next()
                .unwrap_or("plugin"),
        );
        let file = scratch.join(if name.is_empty() {
            "plugin".into()
        } else {
            name
        });
        let out = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&file)
            .arg(src)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("Could not download it (curl): {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "Could not download {src}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        file
    } else {
        PathBuf::from(src)
    };
    if !local.exists() {
        return Err(format!("{} does not exist.", local.display()));
    }
    // 2. The fingerprint you asked for.
    if local.is_file() {
        let sha = sha256_of(&local).map_err(|e| e.to_string())?;
        if let Some(want) = expected
            .map(|w| w.trim().to_ascii_lowercase())
            .filter(|w| !w.is_empty())
        {
            if want != sha {
                return Err(format!(
                    "Not installed: the file's SHA-256 is {sha}, not {want}."
                ));
            }
        }
    } else if expected.is_some() {
        return Err("A SHA-256 can be checked for a file or a .zip, not a folder.".into());
    }
    // 3. A .zip becomes the folder or file inside it.
    if local.is_file()
        && local
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    {
        let out_dir = scratch.join("unzipped");
        let st = Command::new("unzip")
            .args(["-q", "-o"])
            .arg(&local)
            .arg("-d")
            .arg(&out_dir)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("Could not unzip it (unzip): {e}"))?;
        if !st.success() {
            return Err("The .zip could not be unpacked.".into());
        }
        let mut kids: Vec<PathBuf> = std::fs::read_dir(&out_dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                !p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(['.', '_']))
            })
            .collect();
        local = if out_dir.join("plugin.json").is_file() || kids.len() != 1 {
            out_dir
        } else {
            kids.remove(0)
        };
    }
    // 4. Put it where plug-ins live.
    let (target, entry) = if local.is_dir() {
        let entry = plugin::package_entry(&local).ok_or("A folder plug-in needs a plugin.json with an `entry` naming the file to run (for example {\"entry\":\"main.py\"}).")?;
        let name = serde_json::from_str::<Value>(
            &std::fs::read_to_string(local.join("plugin.json")).unwrap_or_default(),
        )
        .ok()
        .and_then(|v| v.get("name").and_then(Value::as_str).map(safe_name))
        .filter(|n| !n.is_empty())
        .or_else(|| local.file_name().and_then(|n| n.to_str()).map(safe_name))
        .filter(|n| !n.is_empty())
        .ok_or("The folder needs a name.")?;
        let rel = entry
            .strip_prefix(&local)
            .map_err(|_| "bad entry")?
            .to_path_buf();
        let target = dest_dir.join(&name);
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .map_err(|e| format!("Could not replace the old {name}: {e}"))?;
        }
        copy_dir(&local, &target).map_err(|e| format!("Could not copy it: {e}"))?;
        let entry = target.join(rel);
        (target, entry)
    } else {
        let file_name = local
            .file_name()
            .and_then(|n| n.to_str())
            .map(safe_name)
            .filter(|n| !n.is_empty())
            .ok_or("The file needs a name.")?;
        let target = dest_dir.join(&file_name);
        std::fs::copy(&local, &target).map_err(|e| format!("Could not copy it: {e}"))?;
        // A plug-in written with the SDK needs it beside it.
        if let Ok(text) = std::fs::read_to_string(&target) {
            for (needle, name, sdk) in [
                ("_reperio", "_reperio.py", PY_SDK),
                ("_reperio", "_reperio.js", JS_SDK),
            ] {
                let wants = (name.ends_with(".py") && file_name.ends_with(".py")
                    || name.ends_with(".js") && file_name.ends_with(".js"))
                    && text.contains(needle);
                if wants && !dest_dir.join(name).exists() {
                    let _ = std::fs::write(dest_dir.join(name), sdk);
                }
            }
        }
        (target.clone(), target)
    };
    make_executable(&entry);
    // 5. Does it start and say what it is?
    let found = plugin::rediscover();
    let identity_path = |p: &Plugin| p.source.clone().unwrap_or_else(|| p.path.clone());
    let ours = found
        .plugins
        .iter()
        .find(|p| identity_path(p) == entry || p.path == entry);
    let Some(p) = ours else {
        let reason = found
            .problems
            .iter()
            .find(|pr| {
                entry
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| pr.file == n)
            })
            .map(|pr| pr.reason.clone())
            .unwrap_or_else(|| "it did not answer like a plug-in".into());
        // Do not leave something that cannot work in the folder.
        if target.is_dir() {
            let _ = std::fs::remove_dir_all(&target);
        } else {
            let _ = std::fs::remove_file(&target);
        }
        return Err(format!("Not installed: it {reason}"));
    };
    let updated = load().is_some_and(|a| a.contains_key(&p.name));
    let sha = sha256_of(identity(p)).map_err(|e| e.to_string())?;
    // You asked for this one: it is the file you know.
    {
        let _g = LOCK.lock();
        let mut all = load().unwrap_or_default();
        all.insert(p.name.clone(), entry_value(&sha, src));
        store(&all);
    }
    Ok(Installed {
        name: p.name.clone(),
        kind: p.kind.clone(),
        version: p.version.clone(),
        permissions: p.permissions.clone(),
        sha256: sha,
        path: target.display().to_string(),
        updated,
    })
}

fn entry_value(sha: &str, source: &str) -> Value {
    entry(sha, &format!("installed from {source}"))
}

/// Removes a plug-in: its file (or folder), and the record of it. Only
/// plug-ins inside a plug-ins folder can be removed.
pub fn remove(name: &str) -> Result<String, String> {
    let p = plugin::discover()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("There is no plug-in called {name}."))?;
    let file = identity(&p).to_path_buf();
    let inside = plugin::folders().iter().any(|f| file.starts_with(f));
    if !inside {
        return Err(
            "That plug-in is not in a plug-ins folder, so Reperio will not remove it.".into(),
        );
    }
    // A folder package goes with its folder; a built program goes with its source.
    let parent = file.parent().map(Path::to_path_buf);
    let gone = match parent
        .as_ref()
        .filter(|d| plugin::package_entry(d).as_deref() == Some(file.as_path()))
    {
        Some(dir) => {
            std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?;
            dir.clone()
        }
        None => {
            std::fs::remove_file(&file).map_err(|e| e.to_string())?;
            if p.source.is_some() {
                let _ = std::fs::remove_file(&p.path);
            }
            file.clone()
        }
    };
    {
        let _g = LOCK.lock();
        let mut all = load().unwrap_or_default();
        all.remove(name);
        store(&all);
    }
    plugin::rediscover();
    Ok(gone.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("my plugin (1).py"), "my_plugin__1_.py");
        assert_eq!(safe_name("../evil"), "__evil".trim_start_matches('_'));
        assert_eq!(safe_name(".hidden"), "hidden");
    }

    #[test]
    fn a_hash_has_a_known_value() {
        let f = std::env::temp_dir().join(format!("reperio-sha-{}", std::process::id()));
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_of(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(f);
    }

    /// The whole life of a plug-in in a made-up home folder: install it, see it
    /// known, change it, see it changed, install a folder package, remove both.
    #[test]
    fn install_trust_and_remove() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let _g = crate::plugin::TEST_GLOBALS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("reperio-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let old_home = std::env::var_os("HOME");
        let old_plugins = std::env::var_os("REPERIO_PLUGINS");
        std::env::set_var("HOME", &home);
        std::env::remove_var("REPERIO_PLUGINS");
        let script = |name: &str| {
            format!("import sys, json\nsys.stdin.readline()\nprint(json.dumps({{'info': {{'name': '{name}', 'kind': 'tool', 'targets': ['none'], 'version': '1.0', 'permissions': ['says hello']}}}}))\n")
        };
        let result = std::panic::catch_unwind(|| {
            // A file.
            let f = src.join("hello.py");
            std::fs::write(&f, script("hello")).unwrap();
            let sha = sha256_of(&f).unwrap();
            let err = install(f.to_str().unwrap(), Some("00ff")).unwrap_err();
            assert!(err.contains("Not installed") && err.contains(&sha), "{err}");
            let done = install(f.to_str().unwrap(), Some(&sha)).unwrap();
            assert_eq!(
                (
                    done.name.as_str(),
                    done.kind.as_str(),
                    done.version.as_str()
                ),
                ("hello", "tool", "1.0")
            );
            assert_eq!(done.permissions, vec!["says hello".to_string()]);
            let p = plugin::discover()
                .into_iter()
                .find(|p| p.name == "hello")
                .unwrap();
            assert_eq!(
                trust_state(&p).0,
                "known",
                "what you installed is what you know"
            );
            std::fs::write(&done.path, script("hello") + "# changed\n").unwrap();
            let p = plugin::rediscover()
                .plugins
                .into_iter()
                .find(|p| p.name == "hello")
                .unwrap();
            assert_eq!(trust_state(&p).0, "changed");
            trust(&p);
            assert_eq!(trust_state(&p).0, "known");
            // Something that is not a plug-in is not left behind.
            let junk = src.join("junk.py");
            std::fs::write(&junk, "print('hi')\n").unwrap();
            assert!(install(junk.to_str().unwrap(), None)
                .unwrap_err()
                .contains("Not installed"));
            assert!(!plugin::user_folder().unwrap().join("junk.py").exists());
            // A folder package.
            let pkg = src.join("pkg");
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(
                pkg.join("plugin.json"),
                r#"{"name":"packaged","entry":"main.py"}"#,
            )
            .unwrap();
            std::fs::write(pkg.join("main.py"), script("packaged")).unwrap();
            std::fs::write(pkg.join("helper.py"), "X = 1\n").unwrap();
            let done = install(pkg.to_str().unwrap(), None).unwrap();
            assert_eq!(done.name, "packaged");
            assert!(plugin::user_folder()
                .unwrap()
                .join("packaged/helper.py")
                .is_file());
            // A new arrival, seen after the record began, is `new` until you say.
            let late = plugin::user_folder().unwrap().join("late.py");
            std::fs::write(&late, script("late")).unwrap();
            let p = plugin::rediscover()
                .plugins
                .into_iter()
                .find(|p| p.name == "late")
                .unwrap();
            assert_eq!(trust_state(&p).0, "new");
            // Remove.
            remove("packaged").unwrap();
            assert!(!plugin::user_folder().unwrap().join("packaged").exists());
            remove("hello").unwrap();
            assert!(!plugin::user_folder().unwrap().join("hello.py").exists());
            assert!(remove("nothing-here").is_err());
        });
        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        if let Some(p) = old_plugins {
            std::env::set_var("REPERIO_PLUGINS", p);
        }
        let _ = std::fs::remove_dir_all(&home);
        plugin::rediscover();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}

//! `reperio plugin`: make, test and configure plug-ins.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use reperio_engine::plugin;
use reperio_engine::plugin_check;
use reperio_engine::plugin_install;

#[derive(Subcommand)]
pub enum PluginCmd {
    /// Start a new plug-in from a working example, with the SDK beside it.
    New {
        /// The plug-in's name: letters, digits, `-` and `_`.
        name: String,
        /// What it is: `tool` (any program), `analyzer` (a phone or an
        /// image), `reader` (a file system) or `source` (a device).
        #[arg(long, default_value = "tool")]
        kind: String,
        /// `py` (Python 3) or `js` (Node).
        #[arg(long, default_value = "py")]
        lang: String,
        /// Where to put it. Default: your plug-ins folder.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Check a plug-in against the protocol: it starts, says what it is, and
    /// for its kind behaves on made-up data. Exits with an error if not.
    Test {
        /// The plug-in file.
        file: PathBuf,
    },
    /// Install a plug-in: a file, a folder with a plugin.json, a .zip of either,
    /// or a URL. It is checked (it must start and say what it is) and its
    /// fingerprint remembered.
    Install {
        /// A path or an http(s) URL.
        source: String,
        /// The SHA-256 the file should have; a file that differs is not installed.
        #[arg(long)]
        sha256: Option<String>,
    },
    /// Remove an installed plug-in.
    Remove { name: String },
    /// Mark a plug-in's current file as the one you know (after reading a
    /// `new` or `changed` notice). Nothing is blocked either way.
    Trust { name: String },
    /// Change a plug-in's settings: `reperio plugin set NAME deep=true mode=fast`.
    Set {
        name: String,
        /// `option=value` pairs (see `reperio plugins` for the options).
        values: Vec<String>,
    },
}

const PY_SDK: &str = include_str!("../../plugins/sdk/python/_reperio.py");
const JS_SDK: &str = include_str!("../../plugins/sdk/js/_reperio.js");

fn template(kind: &str, lang: &str) -> Result<&'static str> {
    Ok(match (kind, lang) {
        ("tool", "py") => include_str!("../../plugins/templates/tool.py"),
        ("analyzer", "py") => include_str!("../../plugins/templates/analyzer.py"),
        ("reader" | "volume", "py") => include_str!("../../plugins/templates/reader.py"),
        ("source", "py") => include_str!("../../plugins/templates/source.py"),
        ("tool", "js") => include_str!("../../plugins/templates/tool.js"),
        ("analyzer", "js") => include_str!("../../plugins/templates/analyzer.js"),
        ("reader" | "volume", "js") => include_str!("../../plugins/templates/reader.js"),
        ("source", "js") => include_str!("../../plugins/templates/source.js"),
        _ => bail!("kind must be tool, analyzer, reader or source, and lang py or js"),
    })
}

pub fn run(cmd: PluginCmd) -> Result<()> {
    match cmd {
        PluginCmd::New {
            name,
            kind,
            lang,
            dir,
        } => new(&name, &kind, &lang, dir),
        PluginCmd::Test { file } => test(&file),
        PluginCmd::Install { source, sha256 } => {
            let done =
                plugin_install::install(&source, sha256.as_deref()).map_err(anyhow::Error::msg)?;
            println!(
                "{} {} ({}{})",
                if done.updated { "Updated" } else { "Installed" },
                done.name,
                done.kind,
                if done.version.is_empty() {
                    String::new()
                } else {
                    format!(", {}", done.version)
                }
            );
            println!("  at {}\n  SHA-256 {}", done.path, done.sha256);
            if !done.permissions.is_empty() {
                println!("  it says: {}", done.permissions.join("; "));
            }
            println!("Check it with: reperio plugin test {}", done.path);
            Ok(())
        }
        PluginCmd::Remove { name } => {
            let gone = plugin_install::remove(&name).map_err(anyhow::Error::msg)?;
            println!("Removed {gone}");
            Ok(())
        }
        PluginCmd::Trust { name } => {
            let p = plugin::discover()
                .into_iter()
                .find(|p| p.name == name)
                .with_context(|| format!("no plug-in called {name}"))?;
            plugin_install::trust(&p);
            println!("{name}: this file is now the one you know.");
            Ok(())
        }
        PluginCmd::Set { name, values } => set(&name, &values),
    }
}

fn new(name: &str, kind: &str, lang: &str, dir: Option<PathBuf>) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || name.starts_with(['-', '_'])
    {
        bail!("the name may use letters, digits, - and _, and must not start with - or _");
    }
    let body = template(kind, lang)?.replace("{{name}}", name);
    let dir = match dir {
        Some(d) => d,
        None => plugin::user_folder().context("no home folder; use --dir")?,
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("could not make {}", dir.display()))?;
    let file = dir.join(format!("{name}.{lang}"));
    if file.exists() {
        bail!("{} already exists", file.display());
    }
    std::fs::write(&file, body)?;
    make_executable(&file);
    // The SDK sits beside it. Its name starts with `_`, so Reperio does not
    // mistake it for a plug-in.
    let (sdk_name, sdk) = if lang == "js" {
        ("_reperio.js", JS_SDK)
    } else {
        ("_reperio.py", PY_SDK)
    };
    let sdk_path = dir.join(sdk_name);
    if std::fs::read_to_string(&sdk_path).ok().as_deref() != Some(sdk) {
        std::fs::write(&sdk_path, sdk)?;
    }
    println!("Made {} (and {sdk_name} beside it).", file.display());
    println!("Next:\n  reperio plugin test {}\n  reperio plugins        (see it listed)\nEdit the file: it has comments on every part.", file.display());
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

fn test(file: &Path) -> Result<()> {
    if !file.is_file() {
        bail!("{} is not a file", file.display());
    }
    // A source file is built first, like Reperio does when it finds one.
    let program = reperio_engine::plugin::build_for_test(file).map_err(anyhow::Error::msg)?;
    // The test prints its own verdict; what the plug-in says while it runs is not needed.
    plugin::set_log_sink(None);
    println!("Testing {} ...\n", file.display());
    let checks = plugin_check::check(&program);
    let mut bad = 0;
    for c in &checks {
        println!(
            "  {} {}{}",
            if c.ok { "✔" } else { "✘" },
            c.name,
            if c.detail.is_empty() {
                String::new()
            } else {
                format!("\n      {}", c.detail)
            }
        );
        if !c.ok {
            bad += 1;
        }
    }
    println!();
    if bad > 0 {
        bail!("{bad} check{} failed", if bad == 1 { "" } else { "s" });
    }
    println!("All checks passed. (This shows Reperio can talk to it, not that it finds the right things.)");
    Ok(())
}

fn set(name: &str, values: &[String]) -> Result<()> {
    let p = plugin::discover()
        .into_iter()
        .find(|p| p.name == name)
        .with_context(|| format!("no plug-in called {name} (see `reperio plugins`)"))?;
    let mut now = plugin::options_for(&p);
    for kv in values {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("{kv}: write option=value"))?;
        let opt = p
            .options
            .iter()
            .find(|o| o.id == k)
            .with_context(|| format!("{name} has no option {k}"))?;
        let value = match opt.kind.as_str() {
            "toggle" => serde_json::Value::Bool(matches!(v, "true" | "on" | "yes" | "1")),
            "number" => serde_json::json!(v
                .parse::<f64>()
                .with_context(|| format!("{k} needs a number"))?),
            "choice" => {
                if !opt.choices.iter().any(|(c, _)| c == v) {
                    bail!(
                        "{k} must be one of: {}",
                        opt.choices
                            .iter()
                            .map(|(c, _)| c.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                serde_json::Value::String(v.into())
            }
            _ => serde_json::Value::String(v.into()),
        };
        now.insert(k.to_string(), value);
    }
    plugin::save_options(name, &now)?;
    println!("Saved. {name} now has: {}", serde_json::Value::Object(now));
    Ok(())
}

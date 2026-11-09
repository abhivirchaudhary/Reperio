//! `reperio android`: what an Android phone still holds, over USB with
//! `adb` (USB debugging on). Nothing is ever written to the phone. The work
//! is in `reperio_engine::android`, shared with the app; see
//! `docs/android.md`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};
use clap::Subcommand;
use reperio_engine::analyzer;
use reperio_engine::android::{self, Adb, Facts};

#[derive(Subcommand)]
pub enum AndroidCmd {
    /// Show the phone's model, Android version, encryption and whether root
    /// is available, and what that means for recovery.
    Info {
        /// Which phone, if several are connected (see `adb devices`).
        #[arg(long)]
        device: Option<String>,
    },
    /// Copy what Android keeps after a delete (trash, thumbnails) to a
    /// folder, and put trashed files back under their own names. No root.
    Pull {
        /// Folder to copy into. Made if it does not exist.
        dest: PathBuf,
        #[arg(long)]
        device: Option<String>,
        /// Also copy everything in the phone's shared storage (/sdcard).
        #[arg(long)]
        everything: bool,
    },
    /// Run forensic plug-ins ("analyzers") on the phone. Each can only run
    /// read-only commands on it, through Reperio, and everything it asks is
    /// written to an audit log beside its report.
    Analyze {
        /// Folder for the reports. Made if it does not exist.
        dest: PathBuf,
        #[arg(long)]
        device: Option<String>,
        /// Run only this plug-in (default: every analyzer that works on phones).
        #[arg(long)]
        plugin: Option<String>,
    },
    /// Image a partition of a rooted phone to a file, with hashes, then
    /// optionally scan it. Read-only on the phone.
    Image {
        /// File to write the image to (must not exist).
        dest: PathBuf,
        #[arg(long)]
        device: Option<String>,
        /// Partition name under /dev/block/by-name.
        #[arg(long, default_value = "userdata", conflicts_with_all = ["whole", "all"])]
        partition: String,
        /// Image the whole storage chip (the partition table and every
        /// partition in one file), the fullest single image.
        #[arg(long, conflicts_with = "all")]
        whole: bool,
        /// Image every partition into its own file in the folder DEST, with
        /// a manifest of sizes and hashes.
        #[arg(long)]
        all: bool,
        /// List the phone's partitions and stop.
        #[arg(long)]
        list: bool,
        /// Go on to `reperio scan` on the image, recovering into this folder.
        #[arg(long, value_name = "DIR")]
        scan_to: Option<PathBuf>,
        /// Image even if the phone says it is encrypted.
        #[arg(long)]
        even_if_encrypted: bool,
    },
}

fn describe(f: &Facts) {
    println!("Phone:     {}", f.name);
    println!("Android:   {} (security patch {})", f.android, f.patch);
    println!(
        "Storage:   {}",
        if f.encrypted {
            format!(
                "encrypted ({})",
                if f.crypto_type.is_empty() {
                    "file-based"
                } else {
                    &f.crypto_type
                }
            )
        } else {
            "not encrypted".to_string()
        }
    );
    println!("Root:      {}", if f.root { "yes" } else { "no" });
    println!("\n{}", android::verdict(f));
}

fn connect(device: &Option<String>) -> Result<Adb> {
    Adb::connect(device.as_deref()).map_err(anyhow::Error::msg)
}

pub fn run(cmd: AndroidCmd) -> Result<()> {
    let never = AtomicBool::new(false);
    match cmd {
        AndroidCmd::Info { device } => {
            describe(&android::facts(&connect(&device)?));
            Ok(())
        }
        AndroidCmd::Pull {
            dest,
            device,
            everything,
        } => {
            let adb = connect(&device)?;
            let f = android::facts(&adb);
            println!("Phone: {} (Android {})", f.name, f.android);
            println!("Read-only: files are copied from the phone; nothing on it is changed.\n");
            let done = android::pull(&adb, &dest, everything, &mut |_| {}, &never)
                .map_err(anyhow::Error::msg)?;
            if let Some(m) = &done.manifest {
                println!("Copied {} files ({} bytes) of the phone's storage to {}/phone-storage.\nFingerprints (SHA-256) of every file: {m}", done.everything_files, done.everything_bytes, dest.display());
            }
            if done.trash + done.thumbnails + done.gallery_trash == 0 {
                println!(
                    "Nothing left behind was found in the phone's trash or thumbnail folders."
                );
                println!("Check Google Photos' Bin, the gallery's Recycle bin and your cloud backups next.");
                return Ok(());
            }
            if done.trash > 0 {
                println!(
                    "{} trashed files put back under their own names in:\n  {}",
                    done.trash, done.restored_dir
                );
                if let Some(t) = &done.expires {
                    println!("Android deletes the oldest of them for good on {t}.");
                }
            }
            if done.gallery_trash > 0 {
                println!(
                    "{} files from gallery trash folders, in {}",
                    done.gallery_trash, done.left_dir
                );
            }
            if done.thumbnails > 0 {
                println!("{} thumbnails (small previews, some of deleted photos), in {}/DCIM/.thumbnails", done.thumbnails, done.left_dir);
            }
            if done.failed > 0 {
                println!("{} files could not be copied.", done.failed);
            }
            Ok(())
        }
        AndroidCmd::Analyze {
            dest,
            device,
            plugin,
        } => {
            let adb = connect(&device)?;
            let facts = android::facts(&adb);
            describe(&facts);
            println!();
            let phone = android::Phone { adb: &adb, facts };
            run_analyzers(
                "android",
                &analyzer::Target {
                    device: Some(&phone),
                    image: None,
                    backup: None,
                },
                &dest,
                plugin.as_deref(),
                &[],
            )
        }
        AndroidCmd::Image {
            dest,
            device,
            partition,
            whole,
            all,
            list,
            scan_to,
            even_if_encrypted,
        } => {
            let adb = connect(&device)?;
            let f = android::facts(&adb);
            describe(&f);
            println!();
            if list {
                if let Some(c) = &f.storage {
                    println!("Storage chip: {} ({} bytes)  <- --whole", c.name, c.size);
                }
                for p in &f.partitions {
                    println!("  {:<24} {:>14} bytes", p.name, p.size);
                }
                return Ok(());
            }
            if all {
                return image_all(&adb, &f, &dest, even_if_encrypted);
            }
            let target = if whole {
                let chip = f.storage.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("The phone did not list a storage chip (needs root).")
                })?;
                android::Target::Device(chip.name.clone())
            } else {
                android::Target::Partition(partition)
            };
            let report = image(&adb, &f, &dest, &target, even_if_encrypted)?;
            if let Some(dir) = scan_to {
                scan(&report, &dir)?;
            } else {
                println!("\nScan it with:  reperio scan {} -o <folder>", report);
            }
            Ok(())
        }
    }
}

fn image_all(adb: &Adb, f: &Facts, dir: &Path, even: bool) -> Result<()> {
    println!(
        "Imaging every partition into {} ... read-only on the phone.",
        dir.display()
    );
    let never = AtomicBool::new(false);
    let mut shown = String::new();
    let done = android::image_all(
        adb,
        f,
        dir,
        even,
        &mut |d, total, name| {
            if shown != name {
                shown = name.to_string();
                eprint!(
                    "\r  {:5.1}%  {name}                 ",
                    d as f64 * 100.0 / total.max(1) as f64
                );
            }
        },
        &never,
    )
    .map_err(anyhow::Error::msg)?;
    eprintln!();
    println!(
        "{} partitions imaged ({} bytes). Sizes and hashes: {}",
        done.done, done.bytes, done.manifest
    );
    if !done.failed.is_empty() {
        println!("Not complete: {}", done.failed.join(", "));
    }
    println!(
        "Scan the data partition with:  reperio scan {}/userdata.img -o <folder>",
        done.dir
    );
    Ok(())
}

fn image(
    adb: &Adb,
    f: &Facts,
    dest: &Path,
    target: &android::Target,
    even: bool,
) -> Result<String> {
    println!(
        "Imaging {} to {} ... read-only on the phone.",
        target.label(),
        dest.display()
    );
    let never = AtomicBool::new(false);
    let mut last = 0;
    let done = android::image(
        adb,
        f,
        dest,
        target,
        even,
        &mut |done, size| {
            let pct = (done * 1000 / size.max(1)) as u32;
            if pct != last {
                last = pct;
                eprint!("\r  {:5.1}%   ", f64::from(pct) / 10.0);
            }
        },
        &never,
    )
    .map_err(anyhow::Error::msg)?;
    eprintln!();
    if !done.complete {
        eprintln!(
            "Warning: expected {} bytes but received {}. The image is incomplete.",
            done.size, done.received
        );
    }
    println!("Image: {}\nReport with hashes: {}", done.path, done.report);
    println!("  MD5: {}\n  SHA-256: {}", done.md5, done.sha256);
    Ok(done.path)
}

fn scan(image: &str, dir: &Path) -> Result<()> {
    println!("\nScanning the image ...");
    let status = Command::new(std::env::current_exe()?)
        .arg("scan")
        .arg(image)
        .arg("-o")
        .arg(dir)
        .status()?;
    if !status.success() {
        bail!("the scan failed");
    }
    Ok(())
}

pub fn run_analyzers(
    target_name: &str,
    target: &analyzer::Target,
    dest: &Path,
    only: Option<&str>,
    args: &[String],
) -> Result<()> {
    let (_, failed) = run_analyzers_found(target_name, target, dest, only, args)?;
    if failed {
        bail!("a plug-in did not finish");
    }
    Ok(())
}

/// The recoverable files each plug-in offered, by plug-in name.
type FoundByPlugin = Vec<(String, Vec<reperio_engine::fs::FsFile>)>;

/// Runs the analyzers (all that can work on `target_name`, or the one named)
/// and prints what they found. Returns the recoverable files they offered,
/// by plug-in, and whether any did not finish.
pub fn run_analyzers_found(
    target_name: &str,
    target: &analyzer::Target,
    dest: &Path,
    only: Option<&str>,
    args: &[String],
) -> Result<(FoundByPlugin, bool)> {
    let mut all_found = Vec::new();
    let plugins: Vec<_> = analyzer::runnable_on(target_name)
        .into_iter()
        .filter(|p| only.is_none_or(|n| p.name == n))
        .collect();
    if plugins.is_empty() {
        bail!(
            "No forensic plug-ins {}for {target_name}. Put one in a plug-ins folder (`reperio plugins` lists them); see docs/forensic-plugins.md.",
            only.map(|n| format!("named {n} ")).unwrap_or_default()
        );
    }
    let never = AtomicBool::new(false);
    let mut failed = false;
    for p in plugins {
        println!("== {}", p.name);
        let report = analyzer::run(
            &p,
            target,
            target_name,
            dest,
            &analyzer::RunOptions {
                args: args.to_vec(),
            },
            &mut |pr| {
                if !pr.text.is_empty() {
                    eprint!("\r  {}                    ", pr.text);
                }
            },
            &never,
        );
        eprintln!();
        if !report.found.is_empty() {
            println!("  {} recoverable files offered:", report.found.len());
            for f in report.found.iter().take(20) {
                println!(
                    "    {}{}  {} bytes",
                    if f.dir.is_empty() {
                        String::new()
                    } else {
                        format!("{}/", f.dir)
                    },
                    f.name,
                    f.size
                );
            }
            all_found.push((p.name.clone(), report.found.clone()));
        }
        for f in &report.findings {
            let mark = match f.severity.as_str() {
                "important" => "!!",
                "warn" => " !",
                _ => "  ",
            };
            let count = f.count.map(|c| format!(" ({c})")).unwrap_or_default();
            println!("{mark} {}{count}", f.title);
            if !f.detail.is_empty() {
                println!("     {}", f.detail);
            }
        }
        for o in &report.outputs {
            println!(
                "   file: {}/{}  ({} bytes)  {}",
                report.dir, o.path, o.size, o.title
            );
        }
        if !report.console.is_empty() {
            println!(
                "   --- what the plug-in printed ({}) ---",
                report.console_log
            );
            for line in report.console.lines() {
                println!("   {line}");
            }
            println!("   ---");
        }
        println!(
            "   audit log: {}  ({} requests, {} refused)",
            report.audit_log, report.requests, report.refused
        );
        if let Some(e) = &report.error {
            println!("   ERROR: {e}");
            failed = true;
        }
    }
    Ok((all_found, failed))
}

/// `reperio analyze IMAGE DEST`.
pub fn analyze_image(source: &Path, dest: &Path, only: Option<&str>) -> Result<()> {
    let src = crate::open_source(source)?;
    println!("Analyzing {} (Reperio reads it only) ...", source.display());
    run_analyzers(
        "image",
        &analyzer::Target {
            device: None,
            image: Some(&src),
            backup: None,
        },
        dest,
        only,
        &[],
    )
}

/// `reperio run PLUGIN [ARGS...]`: any plug-in, on a phone, an image, or
/// nothing at all.
pub fn run_plugin(
    plugin: &str,
    args: &[String],
    dest: &Path,
    image: Option<&Path>,
    phone: bool,
    device: Option<&str>,
) -> Result<()> {
    if phone {
        let adb = Adb::connect(device).map_err(anyhow::Error::msg)?;
        let facts = android::facts(&adb);
        describe(&facts);
        println!();
        let phone = android::Phone { adb: &adb, facts };
        return run_analyzers(
            "android",
            &analyzer::Target {
                device: Some(&phone),
                image: None,
                backup: None,
            },
            dest,
            Some(plugin),
            args,
        );
    }
    if let Some(img) = image {
        let src = crate::open_source(img)?;
        return run_analyzers(
            "image",
            &analyzer::Target {
                device: None,
                image: Some(&src),
                backup: None,
            },
            dest,
            Some(plugin),
            args,
        );
    }
    run_analyzers(
        "none",
        &analyzer::Target {
            device: None,
            image: None,
            backup: None,
        },
        dest,
        Some(plugin),
        args,
    )
}

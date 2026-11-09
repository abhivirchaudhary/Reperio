//! `reperio menu`: a plain text menu for the Reperio rescue USB.
//!
//! It walks someone with a computer that no longer starts through the whole
//! job: pick the internal drive, scan it, choose what to bring back, choose
//! another drive to put it on (mounted for them), and recover. Or make a
//! forensic image of the drive instead. Numbered choices only, so it works
//! on any console with nothing but a keyboard; `b` goes back and `q` quits
//! at every prompt.
//!
//! The drive being recovered is only ever read. The destination is a
//! partition of a different drive, never the source, and is never
//! formatted: if it cannot be mounted for writing, the menu says why.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;

use anyhow::{bail, Context, Result};
use reperio_engine::forensic::{CaseInfo, ForensicOptions};
use reperio_engine::fs::{self as vfs, FsType};
use reperio_engine::imaging::ImageOptions;
use reperio_engine::session::{self, Event, Health, Item, Layout, Origin, Phase, ScanOptions};
use reperio_engine::{health, DeviceSource, Source};

use super::{open_source, safety, size, Meter, KEEP_RUNNING, STOP};

/// Where the destination drive is mounted, unless `REPERIO_MOUNT_ROOT` says
/// otherwise.
const MOUNT_ROOT: &str = "/mnt/reperio";
/// The label the rescue USB's own partition carries, so it is not offered
/// as a drive to recover from or to save onto.
const RESCUE_LABEL: &str = "REPERIO";
/// FAT32 holds no file of 4 GiB or more; images to it are split below that.
const FAT_SPLIT: u64 = 2 << 30;

pub fn run() -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::env::var_os("REPERIO_MENU_SCRIPTED").is_none()
    {
        bail!("`reperio menu` needs a keyboard: run it in a terminal.");
    }
    KEEP_RUNNING.store(true, Ordering::Relaxed);
    let mut ui = Ui::new(std::io::stdin().lock());
    match main_menu(&mut ui) {
        Ok(()) | Err(Nav::Quit) | Err(Nav::Back) => {
            ui.say("\nGoodbye. Nothing on the drive you recovered from was changed.");
            Ok(())
        }
        Err(Nav::Failed(e)) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

/// Why a screen ended.
#[derive(Debug)]
enum Nav {
    Back,
    Quit,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Nav {
    fn from(e: anyhow::Error) -> Self {
        Nav::Failed(e)
    }
}

impl From<std::io::Error> for Nav {
    fn from(e: std::io::Error) -> Self {
        Nav::Failed(e.into())
    }
}

type Step<T> = std::result::Result<T, Nav>;

/// Going back from a sub-screen continues in the screen that called it.
fn back_ok(r: Step<()>) -> Step<()> {
    match r {
        Err(Nav::Back) => Ok(()),
        other => other,
    }
}

struct Ui<R: BufRead> {
    input: R,
}

impl<R: BufRead> Ui<R> {
    fn new(input: R) -> Self {
        Self { input }
    }

    fn say(&self, text: &str) {
        println!("{text}");
    }

    /// Reads one line. `q` quits, `b` goes back; so does the end of input.
    fn line(&mut self, prompt: &str) -> Step<String> {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        // Ctrl-C at a prompt only sets the stop flag of the task before.
        STOP.store(false, Ordering::Relaxed);
        let mut buf = String::new();
        if self.input.read_line(&mut buf)? == 0 {
            return Err(Nav::Quit);
        }
        let t = buf.trim();
        match t.to_ascii_lowercase().as_str() {
            "q" | "quit" | "exit" => Err(Nav::Quit),
            "b" | "back" => Err(Nav::Back),
            _ => Ok(t.to_string()),
        }
    }

    /// Free text, which may be empty. Unlike [`Ui::line`] it does not
    /// treat `b` or `q` as commands: case notes may say anything.
    fn text(&mut self, prompt: &str) -> Step<String> {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        if self.input.read_line(&mut buf)? == 0 {
            return Err(Nav::Quit);
        }
        Ok(buf.trim().to_string())
    }

    /// A numbered list; returns the chosen index.
    fn choose(&mut self, title: &str, options: &[String]) -> Step<usize> {
        loop {
            println!("\n{title}\n");
            for (n, o) in options.iter().enumerate() {
                println!("  {:>2}) {o}", n + 1);
            }
            println!("   b) Go back      q) Quit");
            let reply = self.line("\nType a number and press Enter: ")?;
            match reply.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => println!("Please type a number from 1 to {}.", options.len()),
            }
        }
    }

    fn confirm(&mut self, question: &str, default: bool) -> Step<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            let r = self.line(&format!("{question} {hint} "))?;
            match r.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => println!("Please type y or n."),
            }
        }
    }

    /// Several numbers from a list ("1,3" or "2-4"), or Enter for all.
    fn choose_many(&mut self, what: &str, count: usize) -> Step<Vec<usize>> {
        loop {
            let r = self.line(&format!(
                "Which {what}? Type numbers like 1,3 or 2-4, or press Enter for all: "
            ))?;
            if r.is_empty() {
                return Ok((0..count).collect());
            }
            match parse_selection(&r, count) {
                Some(v) if !v.is_empty() => return Ok(v),
                _ => println!("Please type numbers from 1 to {count}."),
            }
        }
    }
}

/// Parses "1,3,5-7" into zero-based indexes, in order, without repeats.
fn parse_selection(text: &str, count: usize) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in text.split([',', ' ']).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?),
            None => {
                let n = part.parse::<usize>().ok()?;
                (n, n)
            }
        };
        if a == 0 || a > b || b > count {
            return None;
        }
        for n in a..=b {
            if !out.contains(&(n - 1)) {
                out.push(n - 1);
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Drives
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Part {
    /// `sda1`.
    name: String,
    dev: PathBuf,
    size: u64,
    fs: Option<FsType>,
    /// Where it is mounted now, if it is.
    mounted: Option<PathBuf>,
    label: Option<String>,
}

#[derive(Debug, Clone)]
struct Disk {
    /// `sda`.
    name: String,
    dev: PathBuf,
    size: u64,
    model: String,
    removable: bool,
    usb: bool,
    /// The rescue USB itself.
    rescue: bool,
    parts: Vec<Part>,
}

impl Disk {
    fn title(&self) -> String {
        let what = if self.rescue {
            "the rescue USB you started from"
        } else if self.usb || self.removable {
            "external or USB drive"
        } else {
            "internal drive"
        };
        let model = if self.model.is_empty() {
            "Unknown model"
        } else {
            &self.model
        };
        format!("{model}, {} ({}; {})", size(self.size), what, self.name)
    }
}

/// One line per partition, for showing what a drive holds.
fn part_line(p: &Part) -> String {
    let fs = p.fs.map_or("unrecognised", |f| f.name());
    let label = p
        .label
        .as_deref()
        .map(|l| format!(" \"{l}\""))
        .unwrap_or_default();
    format!("{} {}{label}, {}", p.name, fs, size(p.size))
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether `/sys/block/NAME` is a real drive to offer.
fn offered(name: &str) -> bool {
    let skip = ["ram", "zram", "sr", "fd", "nbd", "md", "dm-", "mtdblock"];
    if skip.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // Loop devices are disk images; only offered when asked for (tests).
    !name.starts_with("loop") || std::env::var_os("REPERIO_MENU_SHOW_LOOP").is_some()
}

/// The drives in `sys` (`/sys/block`), with `dev` as the directory holding
/// their device nodes.
fn list_disks(sys: &Path, dev: &Path, mounts: &[Mount], rescue: Option<&str>) -> Vec<Disk> {
    let mut names: Vec<String> = std::fs::read_dir(sys)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| offered(n))
        .collect();
    names.sort();
    let mut disks = Vec::new();
    for name in names {
        let base = sys.join(&name);
        let sectors: u64 = read_trim(base.join("size"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if sectors == 0 {
            continue; // an empty card reader slot
        }
        let model = [
            read_trim(base.join("device/vendor")),
            read_trim(base.join("device/model")),
        ]
        .into_iter()
        .flatten()
        .filter(|v| v != "ATA" && !v.starts_with("0x"))
        .collect::<Vec<_>>()
        .join(" ");
        let real = base.canonicalize().unwrap_or_default();
        let mut parts: Vec<Part> = std::fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("partition").exists())
            .map(|e| {
                let pname = e.file_name().to_string_lossy().into_owned();
                let node = dev.join(&pname);
                let psectors: u64 = read_trim(e.path().join("size"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                Part {
                    fs: detect_fs(&node),
                    mounted: mount_of(mounts, &node),
                    label: None,
                    name: pname,
                    dev: node,
                    size: psectors * 512,
                }
            })
            .collect();
        parts.sort_by_key(|a| natural(&a.name));
        let node = dev.join(&name);
        // A drive formatted without a partition table holds its file
        // system directly.
        if parts.is_empty() {
            parts.push(Part {
                fs: detect_fs(&node),
                mounted: mount_of(mounts, &node),
                label: None,
                name: name.clone(),
                dev: node.clone(),
                size: sectors * 512,
            });
        }
        let is_rescue = rescue.is_some_and(|r| r == name);
        disks.push(Disk {
            usb: real.to_string_lossy().contains("/usb"),
            removable: read_trim(base.join("removable")).as_deref() == Some("1"),
            rescue: is_rescue,
            name,
            dev: node,
            size: sectors * 512,
            model,
            parts,
        });
    }
    disks
}

/// Sorts `sda2` before `sda10`.
fn natural(name: &str) -> (String, u64) {
    let digits: String = name
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    let stem = name[..name.len() - digits.len()].to_string();
    (
        stem,
        digits
            .chars()
            .rev()
            .collect::<String>()
            .parse()
            .unwrap_or(0),
    )
}

/// The file system on a device, read with the engine's own detector so the
/// menu needs no `blkid`.
fn detect_fs(dev: &Path) -> Option<FsType> {
    let src = DeviceSource::open(dev).ok()?;
    vfs::detect(&src, 0)
}

/// The disk the rescue USB runs from: the one with a partition labelled
/// `REPERIO` (see `scripts/make-rescue-usb.sh`).
fn rescue_disk() -> Option<String> {
    // The rescue system's boot script records the device it started from
    // (there is no udev to make /dev/disk/by-label there).
    let node = std::fs::read_to_string("/run/reperio/rescue-dev")
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.exists())
        .or_else(|| {
            Path::new("/dev/disk/by-label")
                .join(RESCUE_LABEL)
                .canonicalize()
                .ok()
        })?;
    let name = node.file_name()?.to_string_lossy().into_owned();
    let sys = Path::new("/sys/class/block").join(&name);
    if sys.join("partition").exists() {
        let parent = sys.canonicalize().ok()?.parent()?.file_name()?.to_owned();
        return Some(parent.to_string_lossy().into_owned());
    }
    Some(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    dev: PathBuf,
    dir: PathBuf,
    writable: bool,
}

/// Reads `/proc/mounts`. Spaces in paths are written as `\040`.
fn parse_mounts(text: &str) -> Vec<Mount> {
    let unescape = |s: &str| s.replace("\\040", " ");
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4 && f[0].starts_with('/')).then(|| Mount {
                dev: PathBuf::from(unescape(f[0])),
                dir: PathBuf::from(unescape(f[1])),
                writable: f[3].split(',').any(|o| o == "rw"),
            })
        })
        .collect()
}

fn current_mounts() -> Vec<Mount> {
    parse_mounts(&std::fs::read_to_string("/proc/mounts").unwrap_or_default())
}

fn mount_of(mounts: &[Mount], dev: &Path) -> Option<PathBuf> {
    let real = dev.canonicalize().ok()?;
    mounts
        .iter()
        .find(|m| m.dev.canonicalize().ok().as_deref() == Some(real.as_path()))
        .map(|m| m.dir.clone())
}

fn disks_now() -> Vec<Disk> {
    list_disks(
        Path::new("/sys/block"),
        Path::new("/dev"),
        &current_mounts(),
        rescue_disk().as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------

fn main_menu<R: BufRead>(ui: &mut Ui<R>) -> Step<()> {
    ui.say(
        "\n=== Reperio rescue ===\n\n\
         This gets your files off a computer that no longer starts.\n\
         The drive you recover from is only read, never changed.\n\
         You will need a second drive (a USB disk or stick) to save the files on.",
    );
    loop {
        let disks: Vec<Disk> = disks_now();
        let candidates: Vec<&Disk> = disks.iter().filter(|d| !d.rescue).collect();
        if candidates.is_empty() {
            ui.say("\nNo drives were found. If a drive is plugged in, give it a moment.");
        }
        let mut options: Vec<String> = candidates.iter().map(|d| d.title()).collect();
        options.push("Look for drives again".into());
        let n = ui.choose("Which drive do you want to recover files from?", &options)?;
        let Some(disk) = candidates.get(n) else {
            continue;
        };
        back_ok(disk_menu(ui, disk))?;
    }
}

fn disk_menu<R: BufRead>(ui: &mut Ui<R>, disk: &Disk) -> Step<()> {
    loop {
        ui.say(&format!("\n{}", disk.title()));
        for p in &disk.parts {
            ui.say(&format!("    {}", part_line(p)));
        }
        let n = ui.choose(
            "What would you like to do?",
            &[
                "Find and recover files".to_string(),
                "Make a complete copy of this drive (forensic image)".to_string(),
                "Check the drive's health".to_string(),
            ],
        )?;
        match n {
            0 => back_ok(recover_flow(ui, disk))?,
            1 => back_ok(image_flow(ui, disk))?,
            _ => show_health(ui, disk),
        }
    }
}

fn show_health<R: BufRead>(ui: &Ui<R>, disk: &Disk) {
    let r = health::report(&disk.dev);
    ui.say(&format!("\nHealth of {}", disk.title()));
    ui.say(&format!("  Verdict: {:?}", r.verdict));
    for reason in r.reasons.iter().chain(&r.notes) {
        ui.say(&format!("  - {reason}"));
    }
    if let Some(t) = r.temperature {
        ui.say(&format!("  Temperature: {t} C"));
    }
    if let Some(h) = r.power_on_hours {
        ui.say(&format!("  Hours in use: {h}"));
    }
}

fn recover_flow<R: BufRead>(ui: &mut Ui<R>, disk: &Disk) -> Step<()> {
    let n = ui.choose(
        "How thorough should the search be?",
        &[
            "Quick: read the drive's lists of files (minutes). Finds files that are still there \
             and recently deleted ones, with their names and folders."
                .to_string(),
            "Thorough: also search every part of the drive for lost and deleted files \
             (can take hours). Needed after a format or a damaged file system."
                .to_string(),
        ],
    )?;
    let deep = n == 1;
    let existing = ui.confirm(
        "Bring back files that are not deleted too (what is simply still on the drive)?",
        true,
    )?;
    let damaged = ui.confirm("Include damaged files (they may only open partly)?", false)?;

    let src = open_source(&disk.dev).map_err(Nav::Failed)?;
    println!(
        "\nSearching {} ({}). Read-only: nothing on it is changed. Ctrl-C stops.",
        disk.dev.display(),
        size(src.len())
    );
    let opts = ScanOptions {
        deep,
        existing,
        formats: Vec::new(),
        resume: None,
        find_partitions: false,

        plugins: None,
    };
    let meter = RefCell::new(Meter::new());
    let phase = RefCell::new("Reading file system");
    let mut found = 0u64;
    STOP.store(false, Ordering::Relaxed);
    super::stop_on_ctrl_c();
    let result = session::scan(
        &src,
        &disk.dev.display().to_string(),
        &opts,
        &STOP,
        |ev| match ev {
            Event::Phase(p) => {
                *phase.borrow_mut() = match p {
                    Phase::FileSystem => "Reading file system",
                    Phase::DeepScan => "Searching the whole drive",
                    Phase::Repair => "Rejoining files stored in pieces",
                }
            }
            Event::Item(_) => found += 1,
            Event::Progress { done, total, .. } => {
                meter
                    .borrow_mut()
                    .update(&phase.borrow(), done, total, found)
            }
            _ => {}
        },
    );
    meter.borrow_mut().clear();
    if result.cancelled {
        println!("Search stopped; showing what was found so far.");
    }

    let items: Vec<&Item> = result
        .items
        .iter()
        .filter(|i| existing || i.origin != Origin::Existing)
        .filter(|i| {
            damaged || matches!(i.health, Health::Good | Health::Unknown | Health::Repaired)
        })
        .collect();
    if items.is_empty() {
        ui.say(if deep {
            "\nNothing recoverable was found on this drive."
        } else {
            "\nNothing was found by the quick search. Try the thorough search."
        });
        return Ok(());
    }
    let chosen = choose_files(ui, &items)?;
    if chosen.is_empty() {
        return Ok(());
    }
    let bytes: u64 = chosen.iter().map(|i| i.size).sum();

    let dest = choose_destination(ui, disk, bytes.saturating_add(bytes / 50))?;
    let layout = match ui.choose(
        "How should the recovered files be arranged?",
        &[
            "Original folders and names (files found by the thorough search go in \"Found by deep scan\")"
                .to_string(),
            "Sorted by type: Photos, Videos, Documents...".to_string(),
            "Sorted by type and year".to_string(),
        ],
    )? {
        0 => Layout::Original,
        1 => Layout::ByType,
        _ => Layout::ByDate,
    };
    let out = unique_dir(&dest.dir.join("Reperio recovered"));
    safety::check_output(&disk.dev, &out).map_err(Nav::Failed)?;
    std::fs::create_dir_all(&out).with_context(|| format!("could not create {}", out.display()))?;

    println!(
        "\nRecovering {} files ({}) to {}",
        chosen.len(),
        size(bytes),
        out.display()
    );
    let mut m = Meter::new();
    STOP.store(false, Ordering::Relaxed);
    let recovered = session::recover(&src, &chosen, &out, layout, &STOP, |done, total, _| {
        m.update("Recovering", done, total, 0)
    });
    m.clear();
    let recovered = recovered.context("recovering files failed")?;
    session::write_report(&out.join("report.csv"), &out, &chosen, &recovered)
        .context("could not write the report")?;
    let bad: u64 = recovered.iter().map(|r| r.bad_sectors).sum();
    println!(
        "\nDone: {} files saved to {}",
        recovered.len(),
        out.display()
    );
    if recovered.len() < chosen.len() {
        println!(
            "Stopped early: {} files were not recovered.",
            chosen.len() - recovered.len()
        );
    }
    if bad > 0 || result.bad_sectors > 0 {
        println!(
            "Warning: {} unreadable sectors. The drive may be failing: make a complete copy of it \
             first (main menu, \"Make a complete copy\") and recover from the copy.",
            bad.max(result.bad_sectors)
        );
    }
    finish_destination(ui, dest);
    Ok(())
}

/// What the person wants out of a scan's results.
fn choose_files<'a, R: BufRead>(ui: &mut Ui<R>, items: &[&'a Item]) -> Step<Vec<&'a Item>> {
    let total: u64 = items.iter().map(|i| i.size).sum();
    let deleted = items
        .iter()
        .filter(|i| i.origin != Origin::Existing)
        .count();
    ui.say(&format!(
        "\nFound {} files ({}); {} of them were deleted or lost.",
        items.len(),
        size(total),
        deleted
    ));
    let mut options = vec![format!(
        "Everything ({} files, {})",
        items.len(),
        size(total)
    )];
    options.push("Only some kinds of file (photos, documents...)".into());
    options.push("Only some folders".into());
    if deleted > 0 && deleted < items.len() {
        options.push(format!("Only the {deleted} deleted or lost files"));
    }
    let n = ui.choose("What do you want to bring back?", &options)?;
    Ok(match n {
        0 => items.to_vec(),
        1 => {
            let groups = group_by(items, |i| i.kind.folder().to_string());
            pick_groups(ui, "kinds of file", &groups)?
        }
        2 => {
            let groups = group_by(items, folder_of);
            pick_groups(ui, "folders", &groups)?
        }
        _ => items
            .iter()
            .copied()
            .filter(|i| i.origin != Origin::Existing)
            .collect(),
    })
}

/// The top-level folder a file was in, for grouping.
fn folder_of(i: &Item) -> String {
    if i.origin == Origin::Carved {
        return "Found by deep scan".into();
    }
    match i.dir.split('/').find(|d| !d.is_empty()) {
        Some(d) => d.to_string(),
        None => "(top level)".into(),
    }
}

fn group_by<'a>(items: &[&'a Item], key: impl Fn(&Item) -> String) -> Vec<(String, Vec<&'a Item>)> {
    let mut map: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
    for i in items {
        map.entry(key(i)).or_default().push(i);
    }
    let mut v: Vec<_> = map.into_iter().collect();
    // Biggest first: that is what people are after.
    v.sort_by_key(|(_, g)| std::cmp::Reverse(g.iter().map(|i| i.size).sum::<u64>()));
    v
}

fn pick_groups<'a, R: BufRead>(
    ui: &mut Ui<R>,
    what: &str,
    groups: &[(String, Vec<&'a Item>)],
) -> Step<Vec<&'a Item>> {
    ui.say("");
    for (n, (name, g)) in groups.iter().enumerate() {
        let bytes: u64 = g.iter().map(|i| i.size).sum();
        ui.say(&format!(
            "  {:>2}) {name}: {} files, {}",
            n + 1,
            g.len(),
            size(bytes)
        ));
    }
    let picks = ui.choose_many(what, groups.len())?;
    Ok(picks
        .into_iter()
        .flat_map(|n| groups[n].1.iter().copied())
        .collect())
}

fn image_flow<R: BufRead>(ui: &mut Ui<R>, disk: &Disk) -> Step<()> {
    ui.say(
        "\nA complete copy reads every sector of the drive, healthy parts first, into one file \
         (like ddrescue), then records MD5, SHA-1 and SHA-256 hashes and a report. Do this first \
         for a drive that is failing, then recover from the copy.",
    );
    let case_number = ui.text("Case number (optional): ")?;
    let examiner = ui.text("Your name (optional): ")?;
    let notes = ui.text("Notes (optional): ")?;
    let verify = ui.confirm(
        "Read the drive a second time to check the copy matches?",
        false,
    )?;

    let dest = choose_destination(ui, disk, disk.size + disk.size / 100)?;
    let dir = unique_dir(&dest.dir.join("Reperio images"));
    safety::check_output(&disk.dev, &dir).map_err(Nav::Failed)?;
    std::fs::create_dir_all(&dir)?;
    let output = dir.join(format!("{}.img", disk.name));
    let fat = dest
        .fs
        .is_some_and(|f| matches!(f, FsType::Fat12 | FsType::Fat16 | FsType::Fat32));
    let opts = ImageOptions {
        split: fat.then_some(FAT_SPLIT),
        ..ImageOptions::default()
    };
    if fat {
        ui.say("The destination is FAT32, which cannot hold files over 4 GB: the copy is written in 2 GB pieces.");
    }
    let given = |s: String| Some(s).filter(|s| !s.is_empty());
    let forensic = ForensicOptions {
        case: CaseInfo {
            case_number: given(case_number),
            examiner: given(examiner),
            evidence: Some(disk.title()),
            notes: given(notes),
        },
        verify,
        ..ForensicOptions::default()
    };
    println!(
        "\nCopying {} to {}. Ctrl-C stops; the copy can be continued.",
        disk.dev.display(),
        output.display()
    );
    STOP.store(false, Ordering::Relaxed);
    let code = super::image(&disk.dev, &output, None, false, &opts, Some(forensic))
        .map_err(Nav::Failed)?;
    match code {
        0 => println!("\nThe copy is in {}", dir.display()),
        super::MISMATCH => println!(
            "\nThe copy was made, but it did not match the drive when read again. See the report."
        ),
        _ => println!("\nStopped. Run this again with the same destination to continue."),
    }
    finish_destination(ui, dest);
    Ok(())
}

// ---------------------------------------------------------------------------
// Destination
// ---------------------------------------------------------------------------

struct Destination {
    /// A folder on the mounted drive.
    dir: PathBuf,
    fs: Option<FsType>,
    /// Set if the menu mounted it, so it can unmount it at the end.
    mounted_by_us: Option<PathBuf>,
}

/// The ways to mount a file system for writing, in the order to try: a
/// command and its arguments. ntfs-3g and exfat-fuse are called directly
/// so they work without a `mount` that knows their helpers.
fn mount_commands(fs: FsType, dev: &Path, dir: &Path) -> Vec<Vec<std::ffi::OsString>> {
    let kernel = |ty: &str| -> Vec<std::ffi::OsString> {
        vec![
            "mount".into(),
            "-t".into(),
            ty.into(),
            "-o".into(),
            "rw".into(),
            dev.into(),
            dir.into(),
        ]
    };
    let direct = |cmd: &str| -> Vec<std::ffi::OsString> {
        vec![cmd.into(), dev.into(), dir.into(), "-o".into(), "rw".into()]
    };
    match fs {
        FsType::Fat12 | FsType::Fat16 | FsType::Fat32 => vec![kernel("vfat")],
        FsType::ExFat => vec![kernel("exfat"), direct("mount.exfat-fuse")],
        FsType::Ntfs => vec![direct("ntfs-3g"), kernel("ntfs3")],
        FsType::Ext2 | FsType::Ext3 | FsType::Ext4 => {
            vec![kernel("ext4"), kernel("ext3"), kernel("ext2")]
        }
        // Linux writes neither safely.
        FsType::HfsPlus | FsType::Apfs | FsType::Other => Vec::new(),
    }
}

/// Whether a file system can be offered as a destination.
fn can_write(fs: FsType) -> bool {
    !mount_commands(fs, Path::new("/dev/null"), Path::new("/")).is_empty()
}

fn mount_root() -> PathBuf {
    std::env::var_os("REPERIO_MOUNT_ROOT").map_or_else(|| PathBuf::from(MOUNT_ROOT), PathBuf::from)
}

/// Lets the person pick a drive to save onto, mounts it for writing, and
/// checks it has `needed` bytes free. Never offers the source drive.
fn choose_destination<R: BufRead>(ui: &mut Ui<R>, source: &Disk, needed: u64) -> Step<Destination> {
    loop {
        // Looked at again each time: a USB drive may have just been plugged in.
        let fresh = disks_now();
        let mut cands: Vec<(&Disk, &Part)> = Vec::new();
        for d in fresh.iter().filter(|d| d.name != source.name && !d.rescue) {
            for p in &d.parts {
                if p.fs.is_some_and(can_write) {
                    cands.push((d, p));
                }
            }
        }
        let mut options: Vec<String> = cands
            .iter()
            .map(|(d, p)| {
                format!(
                    "{} on {}",
                    part_line(p),
                    if d.model.is_empty() {
                        &d.name
                    } else {
                        &d.model
                    }
                )
            })
            .collect();
        options.push("Look for drives again (plug in a USB drive first)".into());
        options.push("Type the path of a folder that is already mounted".into());
        let hint = format!(
            "Where should the files be saved? You need {} free, on a different drive.",
            size(needed)
        );
        let n = ui.choose(&hint, &options)?;
        if n == cands.len() {
            continue;
        }
        let dest = if n == cands.len() + 1 {
            let path = PathBuf::from(ui.line("Folder path: ")?);
            custom_folder(&path, source)
        } else {
            let (_, part) = cands[n];
            mount_partition(part)
        };
        let dest = match dest {
            Ok(d) => d,
            Err(e) => {
                println!("\n{e:#}");
                continue;
            }
        };
        let free = free_space(&dest.dir).unwrap_or(0);
        if free < needed {
            println!(
                "\nThere is only {} free there, and {} is needed. Choose another drive.",
                size(free),
                size(needed)
            );
            finish_destination(ui, dest);
            continue;
        }
        println!("\nSaving to {} ({} free).", dest.dir.display(), size(free));
        return Ok(dest);
    }
}

fn custom_folder(path: &Path, source: &Disk) -> Result<Destination> {
    if !path.is_dir() {
        bail!("{} is not a folder.", path.display());
    }
    writable(path)?;
    // The folder must not be on the drive being recovered.
    safety::check_output(&source.dev, path)?;
    Ok(Destination {
        dir: path.to_path_buf(),
        fs: None,
        mounted_by_us: None,
    })
}

/// Mounts a partition read-write (or uses it where it is already mounted
/// read-write) and proves it can be written to.
fn mount_partition(p: &Part) -> Result<Destination> {
    if let Some(dir) = &p.mounted {
        writable(dir).with_context(|| {
            format!(
                "{} is already mounted at {} but cannot be written to",
                p.name,
                dir.display()
            )
        })?;
        return Ok(Destination {
            dir: dir.clone(),
            fs: p.fs,
            mounted_by_us: None,
        });
    }
    let fs = p.fs.context("this drive's file system is not recognised")?;
    let dir = mount_root().join(&p.name);
    std::fs::create_dir_all(&dir)?;
    let mut last = String::new();
    for cmd in mount_commands(fs, &p.dev, &dir) {
        let out = match Command::new(&cmd[0]).args(&cmd[1..]).output() {
            Ok(o) => o,
            Err(e) => {
                last = format!("{}: {e}", cmd[0].to_string_lossy());
                continue;
            }
        };
        if out.status.success() {
            return match writable(&dir) {
                Ok(()) => Ok(Destination {
                    dir: dir.clone(),
                    fs: p.fs,
                    mounted_by_us: Some(dir),
                }),
                Err(e) => {
                    let _ = Command::new("umount").arg(&dir).status();
                    Err(e.context(format!(
                        "{} mounted, but only for reading. Windows drives that were not shut \
                         down cleanly (or are hibernated) open read-only; start Windows once and \
                         shut it down fully, or choose another drive.",
                        p.name
                    )))
                }
            };
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    let _ = std::fs::remove_dir(&dir);
    bail!("Could not open {} for writing: {last}", p.name)
}

/// Proves a folder accepts new files.
fn writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".reperio-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"x").with_context(|| format!("{} is read-only", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct and reads the C string.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut s) == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
    }
}

/// A folder name that does not exist yet: `name`, `name (2)`, ...
fn unique_dir(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    (2..)
        .map(|n| PathBuf::from(format!("{} ({n})", base.display())))
        .find(|p| !p.exists())
        .unwrap_or_else(|| base.to_path_buf())
}

/// Flushes and, if the menu mounted the drive, unmounts it so it can be
/// unplugged.
fn finish_destination<R: BufRead>(ui: &Ui<R>, dest: Destination) {
    let _ = Command::new("sync").status();
    let Some(dir) = dest.mounted_by_us else {
        ui.say("The drive is still in use by the system; unmount it before unplugging.");
        return;
    };
    match Command::new("umount").arg(&dir).output() {
        Ok(o) if o.status.success() => {
            let _ = std::fs::remove_dir(&dir);
            ui.say("The drive was unmounted. It is safe to unplug it.");
        }
        _ => ui.say(&format!(
            "Could not unmount {}; wait a moment before unplugging it.",
            dir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_number_selections() {
        assert_eq!(parse_selection("1,3", 4), Some(vec![0, 2]));
        assert_eq!(parse_selection("2-4", 4), Some(vec![1, 2, 3]));
        assert_eq!(parse_selection("3 1 3", 4), Some(vec![2, 0]));
        assert_eq!(parse_selection("5", 4), None);
        assert_eq!(parse_selection("0", 4), None);
        assert_eq!(parse_selection("3-2", 4), None);
        assert_eq!(parse_selection("a", 4), None);
    }

    #[test]
    fn reads_proc_mounts() {
        let m = parse_mounts(
            "/dev/sda1 /mnt/my\\040usb vfat rw,relatime 0 0\n\
             tmpfs /run tmpfs rw 0 0\n\
             /dev/sdb2 /media/x ext4 ro,noatime 0 0\n",
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].dir, PathBuf::from("/mnt/my usb"));
        assert!(m[0].writable && !m[1].writable);
    }

    #[test]
    fn sorts_partitions_naturally() {
        let mut v = vec!["sda10", "sda2", "sda1"];
        v.sort_by_key(|n| natural(n));
        assert_eq!(v, ["sda1", "sda2", "sda10"]);
    }

    #[test]
    fn groups_by_folder_and_kind() {
        use reperio_engine::formats::Kind;
        let mk = |dir: &str, origin, kind, size| Item {
            id: 0,
            name: "f".into(),
            dir: dir.into(),
            ext: "x".into(),
            kind,
            origin,
            health: Health::Good,
            size,
            taken: None,
            modified: None,
            extents: Vec::new(),
            volume: None,
            found_by: None,
            fix: None,
        };
        let a = mk("Users/me/Pictures", Origin::Existing, Kind::Photo, 10);
        let b = mk("", Origin::Existing, Kind::Document, 500);
        let c = mk("", Origin::Carved, Kind::Photo, 20);
        assert_eq!(folder_of(&a), "Users");
        assert_eq!(folder_of(&b), "(top level)");
        assert_eq!(folder_of(&c), "Found by deep scan");
        let items = [&a, &b, &c];
        let g = group_by(&items, folder_of);
        assert_eq!(g[0].0, "(top level)", "biggest group first");
        assert_eq!(g.len(), 3);
    }

    #[test]
    fn mounts_choose_drivers() {
        let (dev, dir) = (Path::new("/dev/sdb1"), Path::new("/mnt/x"));
        let first = |fs| {
            mount_commands(fs, dev, dir)[0][0]
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(first(FsType::Fat32), "mount");
        assert_eq!(first(FsType::Ntfs), "ntfs-3g");
        assert!(can_write(FsType::ExFat) && !can_write(FsType::Apfs));
    }

    #[test]
    fn lists_drives_from_a_fake_sysfs() {
        let root = std::env::temp_dir().join(format!("reperio-menu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (sys, dev) = (root.join("sys"), root.join("dev"));
        std::fs::create_dir_all(&dev).unwrap();
        let w = |p: PathBuf, v: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, v).unwrap();
        };
        w(sys.join("sda/size"), "2048\n");
        w(sys.join("sda/removable"), "0\n");
        w(sys.join("sda/device/model"), "FAKE DISK 1\n");
        w(sys.join("sda/sda1/size"), "1024\n");
        w(sys.join("sda/sda1/partition"), "1\n");
        w(sys.join("sda/sda2/size"), "512\n");
        w(sys.join("sda/sda2/partition"), "2\n");
        w(sys.join("sdb/size"), "0\n"); // empty reader slot
        w(sys.join("zram0/size"), "99999\n");
        w(sys.join("sr0/size"), "99999\n");
        w(sys.join("sdc/size"), "4096\n"); // no partition table
        let disks = list_disks(&sys, &dev, &[], Some("sdc"));
        let names: Vec<_> = disks.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["sda", "sdc"]);
        assert_eq!(disks[0].size, 2048 * 512);
        assert_eq!(disks[0].model, "FAKE DISK 1");
        assert_eq!(disks[0].parts.len(), 2);
        assert_eq!(disks[0].parts[1].size, 512 * 512);
        assert!(!disks[0].rescue && disks[1].rescue);
        assert_eq!(
            disks[1].parts.len(),
            1,
            "a drive without a table is its own volume"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

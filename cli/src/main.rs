//! `reperio`: recover deleted photos, videos and files from memory cards,
//! disks and disk images.

#[cfg(target_os = "linux")]
mod menu;
#[cfg(not(target_os = "linux"))]
mod menu {
    pub fn run() -> anyhow::Result<()> {
        anyhow::bail!("`reperio menu` is for the Linux rescue USB; use the Reperio app instead")
    }
}
mod android;
mod case_cmd;
mod ios;
mod nsrl_cmd;
mod plugin;
mod recipe;
mod safety;

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reperio_engine::forensic;
use reperio_engine::imaging::{self, ImageOptions, ImageProgress, Mapfile, Pass};
use reperio_engine::raid::detect::{detect, Hints, LevelChoice};
use reperio_engine::raid::RaidSource;
use reperio_engine::session::{
    self, Event, Health, Item, Layout, Origin, Phase, ScanOptions, ScanResult,
};
use reperio_engine::{DeviceSource, Scanner, Source};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a card, disk or image and recover what was deleted from it.
    Scan {
        /// What to scan, e.g. /dev/rdisk4 on a Mac, /dev/sdb on Linux, or an image file.
        #[arg(required_unless_present = "raid")]
        source: Option<PathBuf>,
        /// Scan drives of a RAID set (or a spanned or striped volume)
        /// combined into one: --raid /dev/sdb,/dev/sdc,/dev/sdd. The layout
        /// is read from md superblocks or worked out; see `reperio raid`.
        #[arg(long, value_delimiter = ',', conflicts_with = "source")]
        raid: Vec<PathBuf>,
        #[command(flatten)]
        array: ArrayArgs,
        /// Folder to save recovered files into. Must not be on the card being scanned.
        #[arg(short, long, default_value = "recovered")]
        output: PathBuf,
        /// Only list what was found; write nothing.
        #[arg(long)]
        list: bool,
        /// Skip the deep scan and only read the filesystem (fast, but finds
        /// nothing after a format).
        #[arg(long)]
        quick: bool,
        /// Also recover files that were never deleted.
        #[arg(long)]
        existing: bool,
        /// Recover damaged and partly overwritten files too.
        #[arg(long)]
        damaged: bool,
        /// How to arrange recovered files.
        #[arg(long, value_enum, default_value_t = LayoutArg::Original)]
        layout: LayoutArg,
        /// Deep-scan only for these formats (default: all).
        #[arg(long, value_delimiter = ',', value_parser = clap::builder::PossibleValuesParser::new(Scanner::format_names()))]
        types: Vec<String>,
        /// Save the scan results to this file, for the Reperio app to open.
        #[arg(long)]
        save: Option<PathBuf>,
        /// A Recovery Vault record of this drive (from `reperio vault save`):
        /// files found by the deep scan get their names back from it.
        #[arg(long)]
        vault: Vec<PathBuf>,
        /// Search every sector of the drive for partitions that were deleted
        /// or lost from the partition table, not just the likely places
        /// outside readable volumes (always searched). Reads the whole drive.
        #[arg(long)]
        find_partitions: bool,
        /// Use only this plug-in to read volumes (repeat for several). By
        /// default every installed plug-in is used; see `reperio plugins`.
        #[arg(long = "plugin", value_name = "NAME", conflicts_with = "no_plugins")]
        plugin: Vec<String>,
        /// Use no plug-ins.
        #[arg(long)]
        no_plugins: bool,
        /// Run this analyzer plug-in on the source after the scan (repeat for
        /// several). Files it offers join the results and are recovered with
        /// the rest; its report goes in --reports.
        #[arg(long = "analyze", value_name = "NAME")]
        analyze: Vec<String>,
        /// Where analyzer reports go (default: a folder inside the output).
        #[arg(long)]
        reports: Option<PathBuf>,
    },
    /// Show how drives from a RAID set (a NAS, a RAID enclosure, a striped
    /// volume) were arranged. Reads md superblocks when there are any, and
    /// otherwise works the layout out from the data. Reads only.
    Raid {
        /// The drives, or images of them, in any order. If one is missing,
        /// say so with --missing.
        #[arg(required = true)]
        drives: Vec<PathBuf>,
        #[command(flatten)]
        array: ArrayArgs,
        /// Show the layouts as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Record a mounted drive's files in a Recovery Vault record, so they
    /// can be named after a delete or format. Run it again to refresh.
    Vault {
        /// The mounted drive, e.g. /Volumes/CARD.
        drive: PathBuf,
        /// The record file to create or refresh.
        record: PathBuf,
    },
    /// Copy a whole card or disk to an image file, healthy areas first and
    /// damaged ones after, like GNU ddrescue. Do this first for a card that
    /// may be failing, then scan the image. Ctrl-C stops; --resume carries on.
    Image {
        source: PathBuf,
        output: PathBuf,
        /// Where to keep track of what has been read (default: OUTPUT.map).
        /// GNU ddrescue can read and continue it.
        #[arg(long)]
        map: Option<PathBuf>,
        /// Continue an earlier copy into OUTPUT from its map file.
        #[arg(long)]
        resume: bool,
        /// Go back over unreadable sectors this many more times at the end.
        #[arg(long, default_value_t = 0)]
        retries: u32,
        /// Retry from the end of the card towards the start.
        #[arg(long)]
        reverse: bool,
        /// Don't read damaged areas sector by sector: faster, recovers less.
        /// Run again later with --resume to finish them.
        #[arg(long)]
        no_scrape: bool,
        /// Write the image in pieces of this size (e.g. 2G, 650M):
        /// OUTPUT.001, OUTPUT.002, ... For FAT32 drives, which cannot hold
        /// files over 4 GB, or for archiving.
        #[arg(long, value_parser = parse_segment_size)]
        split: Option<u64>,
        #[command(flatten)]
        forensic: ForensicArgs,
    },
    /// Step-by-step text menu: pick a disk, scan it, choose what to recover
    /// and where to (another drive, mounted for you), or make a forensic
    /// image of it. What the Reperio rescue USB starts into.
    Menu,
    /// Read the whole drive (or part of it), timing every block, to find
    /// slow and unreadable areas. Only reads; Ctrl-C stops.
    Surface {
        source: PathBuf,
        /// Part to test, as START..END: bytes, with K, M, G or T (1000s),
        /// KiB, MiB, GiB or TiB (1024s), or % of the drive. For example
        /// 10G..20G, 50%..100%, ..1T.
        #[arg(long)]
        range: Option<String>,
        /// Bytes read and timed at a time (default 1MiB).
        #[arg(long)]
        block: Option<String>,
        /// Keep the test's map in this file: continue from it if it
        /// exists, and save to it when stopped or done.
        #[arg(long)]
        state: Option<PathBuf>,
    },
    /// List your plug-ins (recovery code dropped in a folder) and where to
    /// put them.
    Plugins,
    /// List the devices your source plug-ins can reach (an iPhone, a camera,
    /// a network share, anything you wrote a plug-in for). Use one as
    /// `plugin:NAME:DEVICE` wherever a disk or image is expected.
    Devices,
    /// Run forensic plug-ins ("analyzers") on a disk image. Reads only; the
    /// plug-ins' reports go in DEST/<plug-in name>/ with an audit log.
    Analyze {
        /// The image (or device) to analyze.
        source: PathBuf,
        /// Folder for the reports. Made if it does not exist.
        dest: PathBuf,
        /// Run only this plug-in (default: every analyzer that works on images).
        #[arg(long)]
        plugin: Option<String>,
    },
    /// Run the red team forensic plugin suite against a disk image or Android
    /// device. Aggregates findings from all redteam analyzers into a unified
    /// report and timeline. The plug-ins are not part of the open-source
    /// release; see plugins/README.md.
    Redteam {
        /// The image (or device) to analyze.
        source: PathBuf,
        /// Folder for the reports. Made if it does not exist.
        dest: PathBuf,
        /// Run only plugins in this category (credentials, lateral_movement,
        /// persistence, anti_forensics, location, comms, cloud).
        #[arg(long)]
        category: Option<String>,
        /// Only include findings with confidence >= this level (low, medium, high).
        #[arg(long, default_value = "low")]
        confidence: String,
        /// Also scan Volume Shadow Copy snapshots (VSS) on Windows images.
        #[arg(long)]
        aggressive: bool,
    },
    /// Run any plug-in of your own (see `reperio plugins`), by name, on a
    /// connected Android phone (`--phone`), on an image (`--image`), or on
    /// nothing. Everything it prints is shown and kept. Plug-ins run with
    /// full access: Reperio does not limit them. Put the options (-o, --phone,
    /// --image) before the plug-in's own arguments.
    Run {
        /// The plug-in's name.
        plugin: String,
        /// Arguments passed to the plug-in.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Folder for the plug-in's files, report and logs.
        #[arg(short, long)]
        output: PathBuf,
        /// Run it on this image or device.
        #[arg(long, conflicts_with = "phone")]
        image: Option<PathBuf>,
        /// Run it on the connected Android phone.
        #[arg(long)]
        phone: bool,
        /// Which phone, if several are connected.
        #[arg(long, requires = "phone")]
        device: Option<String>,
    },
    /// Android phones over USB (adb): what they keep after a delete, and
    /// an image of a rooted phone.
    Android {
        #[command(subcommand)]
        cmd: android::AndroidCmd,
    },
    /// Make, test and configure plug-ins: `reperio plugin new`, `test`, `set`.
    Plugin {
        #[command(subcommand)]
        cmd: plugin::PluginCmd,
    },
    /// Run a whole job from one file: a source (a disk, an image or a device
    /// a plug-in reaches), the readers and analyzers to chain, and where to
    /// recover to. `reperio recipe example` prints one.
    Recipe {
        #[command(subcommand)]
        cmd: recipe::RecipeCmd,
    },
    /// iPhones and iPads: back one up and recover from the backup (photos
    /// including Recently Deleted, messages, files). Needs `brew install
    /// libimobiledevice`. Nothing is written to the phone.
    Ios {
        #[command(subcommand)]
        cmd: ios::IosCmd,
    },
    /// Show what a drive says about its own health (SMART).
    Health { device: PathBuf },
    /// Show how Reperio sees deleted files whose name contains NAME: where
    /// their data is and why they check out or not. For reporting problems.
    Inspect {
        source: PathBuf,
        name: String,
        /// Search every sector for lost partitions, as `scan --find-partitions`.
        #[arg(long)]
        find_partitions: bool,
    },
    /// Repair a damaged photo, document or video, found by its content:
    /// a video whose index is missing, a JPEG whose header is lost or which
    /// was cut short, a HEIC cut short, a Word, Excel or PowerPoint file
    /// or PDF that won't open, or an old Word document. Writes a new file;
    /// the damaged one is never changed.
    Repair {
        /// The damaged file.
        file: PathBuf,
        /// A good file from the same camera (videos: optional when the
        /// camera repeats its settings in the stream; JPEGs whose header
        /// is lost: a photo of the same size).
        #[arg(long, short)]
        reference: Option<PathBuf>,
        /// Where to write the repaired file (default: next to the damaged one).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Repair a video that won't play because its index is missing (a
    /// recording cut short, or a recovered video missing its end), using a
    /// good video from the same camera as a guide.
    FixVideo {
        /// The video that won't play.
        broken: PathBuf,
        /// A video that plays, recorded by the same camera with the same settings.
        #[arg(long, short)]
        reference: PathBuf,
        /// Where to write the repaired video (default: next to the broken one).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Manage forensic cases: create, list, add evidence, and track chain of custody.
    Case {
        #[command(subcommand)]
        cmd: case_cmd::CaseCmd,
    },
    /// Manage the Known File Filter (NSRL) hash database.
    Nsrl {
        #[command(subcommand)]
        cmd: nsrl_cmd::NsrlCmd,
    },
}

/// Forensic options for `image`. Giving any of them hashes the finished
/// image and writes an acquisition report.
#[derive(clap::Args, Default, Clone)]
pub struct ForensicArgs {
    /// Case number, for the acquisition report.
    #[arg(long = "case", help_heading = "Forensic report")]
    case_number: Option<String>,
    /// Who made the copy.
    #[arg(long, help_heading = "Forensic report")]
    examiner: Option<String>,
    /// What the evidence is, e.g. "Item 3: 64 GB SD card, red".
    #[arg(long, help_heading = "Forensic report")]
    evidence: Option<String>,
    /// Free-form notes.
    #[arg(long, help_heading = "Forensic report")]
    notes: Option<String>,
    /// Hashes of the image to compute [default: md5,sha1,sha256].
    #[arg(long, value_parser = parse_hashes, help_heading = "Forensic report")]
    hash: Option<forensic::Algorithms>,
    /// Read the source again afterwards and check it matches the image.
    #[arg(long, help_heading = "Forensic report")]
    verify: bool,
    /// Where to write the report (PATH.txt and PATH.json) [default: OUTPUT.report].
    #[arg(long, help_heading = "Forensic report")]
    report: Option<PathBuf>,
}

impl ForensicArgs {
    /// The forensic options, if any were given.
    pub fn options(&self) -> Option<forensic::ForensicOptions> {
        let given = self.case_number.is_some()
            || self.examiner.is_some()
            || self.evidence.is_some()
            || self.notes.is_some()
            || self.hash.is_some()
            || self.verify
            || self.report.is_some();
        given.then(|| forensic::ForensicOptions {
            case: forensic::CaseInfo {
                case_number: self.case_number.clone(),
                examiner: self.examiner.clone(),
                evidence: self.evidence.clone(),
                notes: self.notes.clone(),
            },
            algorithms: self.hash.unwrap_or_default(),
            verify: self.verify,
            report: self.report.clone(),
        })
    }
}

/// What is already known about a RAID set; the rest is worked out.
#[derive(clap::Args, Clone, Default)]
struct ArrayArgs {
    /// RAID level: 0, 1, 5, 10 or jbod.
    #[arg(long, value_name = "LEVEL")]
    level: Option<String>,
    /// Chunk (stripe) size, e.g. 64K or 512K.
    #[arg(long, value_name = "SIZE")]
    chunk: Option<String>,
    /// RAID 5 parity layout: left-symmetric, left-asymmetric,
    /// right-symmetric or right-asymmetric.
    #[arg(long, value_name = "NAME")]
    parity: Option<String>,
    /// Order of the drives in the array, as drive numbers counted from 1 in
    /// the order given; `-` marks a missing one: --order 2,1,-,3
    #[arg(long, value_name = "LIST")]
    order: Option<String>,
    /// Where the data starts on each drive (one size for all, or one per
    /// drive, comma separated), e.g. 1M. Default: found out.
    #[arg(long, value_name = "SIZE", value_delimiter = ',')]
    offset: Vec<String>,
    /// How many drives of the set are missing (RAID 5 is rebuilt around one).
    #[arg(long, default_value_t = 0)]
    missing: usize,
    /// Work the layout out from the data even if md superblocks are present.
    #[arg(long)]
    ignore_metadata: bool,
}

impl ArrayArgs {
    fn hints(&self, drives: usize) -> Result<Hints> {
        use reperio_engine::raid::{parse_order, parse_size, Parity};
        let level = match &self.level {
            Some(l) => Some(
                LevelChoice::parse(l)
                    .with_context(|| format!("unknown RAID level {l} (use 0, 1, 5, 10 or jbod)"))?,
            ),
            None => None,
        };
        let chunk = match &self.chunk {
            Some(c) => Some(parse_size(c).with_context(|| format!("bad chunk size {c}"))?),
            None => None,
        };
        let parity = match &self.parity {
            Some(p) => {
                Some(Parity::parse(p).with_context(|| format!("unknown parity layout {p}"))?)
            }
            None => None,
        };
        let order = match &self.order {
            Some(o) => Some(parse_order(o, drives).map_err(anyhow::Error::msg)?),
            None => None,
        };
        let offsets = if self.offset.is_empty() {
            None
        } else {
            let v: Option<Vec<u64>> = self.offset.iter().map(|o| parse_size(o)).collect();
            Some(v.context("bad data offset")?)
        };
        Ok(Hints {
            level,
            parity,
            chunk,
            order,
            missing: self.missing,
            offsets,
            ignore_metadata: self.ignore_metadata,
        })
    }
}

fn parse_hashes(s: &str) -> Result<forensic::Algorithms, String> {
    forensic::Algorithms::parse(s)
}

/// Parses a size such as `2G`, `650M`, `4096` (powers of 1024, as split(1)
/// reads them). Segments must hold whole sectors.
pub fn parse_segment_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_uppercase();
    let t = t.strip_suffix("IB").or(t.strip_suffix('B')).unwrap_or(&t);
    let (num, shift) = match t.chars().last() {
        Some('K') => (&t[..t.len() - 1], 10),
        Some('M') => (&t[..t.len() - 1], 20),
        Some('G') => (&t[..t.len() - 1], 30),
        Some('T') => (&t[..t.len() - 1], 40),
        _ => (t, 0),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("\"{s}\" is not a size like 2G or 650M"))?;
    let bytes = n
        .checked_shl(shift)
        .filter(|b| b >> shift == n)
        .ok_or("that size is too large")?;
    if bytes == 0 || bytes % 512 != 0 {
        return Err("the size must be a whole number of 512-byte sectors".into());
    }
    Ok(bytes)
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    /// Original folders and names where known; deep-scan finds by type.
    Original,
    /// Photos/2004, Videos/2006, ...
    ByDate,
    /// Photos, Videos, ...
    ByType,
}

impl From<LayoutArg> for Layout {
    fn from(l: LayoutArg) -> Self {
        match l {
            LayoutArg::Original => Layout::Original,
            LayoutArg::ByDate => Layout::ByDate,
            LayoutArg::ByType => Layout::ByType,
        }
    }
}

pub(crate) struct ScanArgs {
    /// Analyzer plug-ins to run on the source after the scan; the files they
    /// offer join the results.
    pub(crate) analyzers: Vec<String>,
    pub(crate) reports: Option<PathBuf>,
    pub(crate) output: PathBuf,
    pub(crate) list: bool,
    pub(crate) existing: bool,
    pub(crate) damaged: bool,
    pub(crate) layout: Layout,
    pub(crate) save: Option<PathBuf>,
    pub(crate) vault: Vec<PathBuf>,
}

fn main() -> Result<()> {
    // Reperio puts no limit on plug-ins. To stop ones that go quiet or run too
    // long, set REPERIO_PLUGIN_IDLE_SECS and REPERIO_PLUGIN_MAX_SECS.
    let secs = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .map(Duration::from_secs)
    };
    reperio_engine::plugin::set_limits(reperio_engine::plugin::Limits {
        idle: secs("REPERIO_PLUGIN_IDLE_SECS"),
        hard: secs("REPERIO_PLUGIN_MAX_SECS"),
    });
    // What a plug-in says is shown as it says it.
    if std::env::var_os("REPERIO_PLUGIN_QUIET").is_none() {
        reperio_engine::plugin::set_log_sink(Some(Arc::new(|plugin: &str, line: &str| {
            eprintln!("[{plugin}] {line}");
        })));
    }
    match Cli::parse().command {
        Command::Scan {
            source,
            raid,
            array,
            output,
            list,
            quick,
            existing,
            damaged,
            layout,
            types,
            save,
            vault,
            find_partitions,
            plugin,
            no_plugins,
            analyze,
            reports,
        } => {
            let opts = ScanOptions {
                plugins: if no_plugins {
                    Some(Vec::new())
                } else if plugin.is_empty() {
                    None
                } else {
                    Some(plugin)
                },
                deep: !quick,
                existing,
                formats: types,
                resume: None,
                find_partitions,
                case_id: None,
                evidence_id: None,
                nsrl_db: None,
            };
            let args = ScanArgs {
                analyzers: analyze,
                reports,
                output,
                list,
                existing,
                damaged,
                layout: layout.into(),
                save,
                vault,
            };
            match source {
                Some(source) => {
                    let src = open_source(&source)?;
                    scan(&source.display().to_string(), &[source], &src, &opts, &args)
                }
                None => {
                    let (src, label) = open_raid(&raid, &array)?;
                    scan(&label, &raid, &src, &opts, &args)
                }
            }
        }
        Command::Raid {
            drives,
            array,
            json,
        } => raid_report(&drives, &array, json),
        Command::Image {
            source,
            output,
            map,
            resume,
            retries,
            reverse,
            no_scrape,
            split,
            forensic,
        } => {
            let opts = ImageOptions {
                retries,
                reverse_retries: reverse,
                scrape: !no_scrape,
                split,
                ..ImageOptions::default()
            };
            let code = image(&source, &output, map, resume, &opts, forensic.options())?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Menu => menu::run(),
        Command::Surface {
            source,
            range,
            block,
            state,
        } => surface(&source, range.as_deref(), block.as_deref(), state),
        Command::Plugins => {
            let folders = reperio_engine::plugin::folders();
            println!("Put plug-ins (a script or program) in any of these folders:");
            for f in &folders {
                println!(
                    "  {}{}",
                    f.display(),
                    if f.is_dir() { "" } else { "  (create it)" }
                );
            }
            let discovery = reperio_engine::plugin::discovery();
            let found = discovery.plugins;
            if found.is_empty() {
                println!("\nNo plug-ins found. See plugins/examples/ for ones to start from.");
            }
            for p in &discovery.problems {
                println!("\nNot usable: {} {}", p.file, p.reason);
            }
            for p in found {
                if p.kind == "source" {
                    println!(
                        "\n{}  (source: reaches devices Reperio can't; see `reperio devices`)\n  file: {}\n  {}",
                        p.name,
                        p.source.as_ref().unwrap_or(&p.path).display(),
                        p.description
                    );
                } else if p.kind == "analyzer" {
                    println!(
                        "\n{}  (forensic analyzer)\n  file: {}\n  works on: {}\n  {}",
                        p.name,
                        p.source.as_ref().unwrap_or(&p.path).display(),
                        p.targets.join(", "),
                        p.description
                    );
                } else {
                    println!(
                        "\n{}\n  file: {}\n  volumes: {}",
                        p.name,
                        p.source.as_ref().unwrap_or(&p.path).display(),
                        p.fs.join(", ")
                    );
                }
                if !p.version.is_empty() || !p.author.is_empty() {
                    println!(
                        "  version: {} {}",
                        if p.version.is_empty() {
                            "-"
                        } else {
                            &p.version
                        },
                        if p.author.is_empty() {
                            String::new()
                        } else {
                            format!("by {}", p.author)
                        }
                    );
                }
                if !p.permissions.is_empty() {
                    println!("  says it: {}", p.permissions.join("; "));
                }
                let (state, sha) = reperio_engine::plugin_install::trust_state(&p);
                match state {
                    "new" => println!("  NEW: you have not looked at this one yet (SHA-256 {sha}). Run `reperio plugin trust {}` to say you have.", p.name),
                    "changed" => println!("  CHANGED since you last used it (SHA-256 {sha}). Run `reperio plugin trust {}` if that is expected.", p.name),
                    _ => {}
                }
                if !p.options.is_empty() {
                    let now = reperio_engine::plugin::options_for(&p);
                    for o in &p.options {
                        println!(
                            "  option {} ({}): {} = {}",
                            o.id,
                            o.kind,
                            o.label,
                            now.get(&o.id).map(|v| v.to_string()).unwrap_or_default()
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Devices => {
            let devices = reperio_engine::devices::list();
            if devices.is_empty() {
                println!("No devices. Source plug-ins (kind \"source\") list the devices they can reach; see docs/plugins.md and plugins/examples/source_file.py.");
            }
            for d in devices {
                println!(
                    "plugin:{}:{}\n  {} ({}, {}){}",
                    d.plugin,
                    d.id,
                    d.name,
                    if d.kind.is_empty() { "device" } else { &d.kind },
                    if d.size > 0 {
                        format!("{:.1} GB", d.size as f64 / 1e9)
                    } else {
                        "size unknown".into()
                    },
                    if d.detail.is_empty() {
                        String::new()
                    } else {
                        format!(". {}", d.detail)
                    }
                );
            }
            Ok(())
        }
        Command::Android { cmd } => android::run(cmd),
        Command::Ios { cmd } => ios::run(cmd),
        Command::Plugin { cmd } => plugin::run(cmd),
        Command::Recipe { cmd } => recipe::run(cmd),
        Command::Run {
            plugin,
            args,
            output,
            image,
            phone,
            device,
        } => android::run_plugin(
            &plugin,
            &args,
            &output,
            image.as_deref(),
            phone,
            device.as_deref(),
        ),
        Command::Analyze {
            source,
            dest,
            plugin,
        } => android::analyze_image(&source, &dest, plugin.as_deref()),
        Command::Redteam {
            source,
            dest,
            category,
            confidence,
            aggressive,
        } => run_redteam(&source, &dest, category.as_deref(), &confidence, aggressive),
        Command::Health { device } => health(&device),
        Command::Inspect {
            source,
            name,
            find_partitions,
        } => inspect(&source, &name, find_partitions),
        Command::Vault { drive, record } => vault_save(&drive, &record),
        Command::FixVideo {
            broken,
            reference,
            output,
        } => fix_video(&broken, &reference, output),
        Command::Repair {
            file,
            reference,
            output,
        } => repair(&file, reference.as_deref(), output),
        Command::Case { cmd } => case_cmd::run(cmd),
        Command::Nsrl { cmd } => nsrl_cmd::run(cmd),
    }
}

/// `name (repaired).ext` beside `path`.
fn repaired_name(path: &Path, ext: Option<&str>) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = ext.map(str::to_string).unwrap_or_else(|| {
        path.extension()
            .map_or("bin".into(), |e| e.to_string_lossy().into_owned())
    });
    path.with_file_name(format!("{stem} (repaired).{ext}"))
}

fn repair(file: &Path, reference: Option<&Path>, output: Option<PathBuf>) -> Result<()> {
    use reperio_engine::fixfile::{self, Repaired};
    let src = open_source(file)?;
    let reference = reference.map(open_source).transpose()?;
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let fixed = fixfile::repair(&src, &ext, reference.as_ref().map(|r| r as &dyn Source))
        .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let output = output.unwrap_or_else(|| repaired_name(file, fixed.ext()));
    if output.exists() {
        bail!("{} already exists", output.display());
    }
    let mut out = BufWriter::new(File::create(&output)?);
    let bad = fixed.write_to(&src, &mut out)?;
    out.flush()?;
    println!("{} Written to {}.", fixed.describe(), output.display());
    if let Repaired::Document {
        text: Some(text), ..
    } = &fixed
    {
        let txt = output.with_extension("txt");
        if txt.exists() {
            bail!("{} already exists", txt.display());
        }
        std::fs::write(&txt, text)?;
        println!(
            "Text that could be read from the damaged part: {}.",
            txt.display()
        );
    }
    if bad > 0 {
        println!("{bad} unreadable sectors were filled with zeros.");
    }
    Ok(())
}

fn vault_save(drive: &Path, record: &Path) -> Result<()> {
    use reperio_engine::vault::Vault;
    let old = if record.exists() {
        Vault::load(record)?
    } else {
        Vault::default()
    };
    let mut fresh = old.refresh(drive)?;
    if fresh.label.is_empty() {
        fresh.label = drive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    fresh.save(record)?;
    println!(
        "Recorded {} files from {} in {}.",
        fresh.files.len(),
        drive.display(),
        record.display()
    );
    Ok(())
}

fn fix_video(broken: &Path, reference: &Path, output: Option<PathBuf>) -> Result<()> {
    use reperio_engine::videofix::{rebuild, Reference};
    let guide = open_source(reference)?;
    let Some(guide) = Reference::from_video(&guide) else {
        bail!(
            "{} is not an MP4 or MOV with H.264 or H.265 video that plays",
            reference.display()
        );
    };
    let src = open_source(broken)?;
    let Some(fixed) = rebuild(&src, &guide) else {
        bail!(
            "No video frames that match the reference were found in {}",
            broken.display()
        );
    };
    let output = output.unwrap_or_else(|| {
        let stem = broken.file_stem().unwrap_or_default().to_string_lossy();
        let ext = broken
            .extension()
            .map_or("mp4".into(), |e| e.to_string_lossy().into_owned());
        broken.with_file_name(format!("{stem} (repaired).{ext}"))
    });
    if output.exists() {
        bail!("{} already exists", output.display());
    }
    let mut out = BufWriter::new(File::create(&output)?);
    let bad = fixed.write_to(&src, &mut out)?;
    out.flush()?;
    println!(
        "Repaired: {} frames, {:.1} seconds of video, written to {}.",
        fixed.frames,
        fixed.seconds,
        output.display()
    );
    println!("The sound could not be rebuilt; the repaired video plays without it.");
    if bad > 0 {
        println!("{bad} unreadable sectors were filled with zeros.");
    }
    Ok(())
}

/// Opens a disk, an image, or (as `plugin:NAME:DEVICE`, see `reperio devices`)
/// a device served by a source plug-in.
pub fn open_source(path: &Path) -> Result<Arc<dyn Source>> {
    if let Some(rest) = path.to_str().and_then(|p| p.strip_prefix("plugin:")) {
        let (plugin, device) = rest
            .split_once(':')
            .context("use plugin:NAME:DEVICE (see `reperio devices`)")?;
        let src = reperio_engine::devices::PluginSource::open(plugin, device)
            .map_err(|e| anyhow::anyhow!(e))?;
        return Ok(Arc::new(src));
    }
    open_file_source(path).map(|s| Arc::new(s) as Arc<dyn Source>)
}

fn open_file_source(path: &Path) -> Result<DeviceSource> {
    DeviceSource::open(path).with_context(|| {
        let mut msg = format!("could not open {}", path.display());
        if path.starts_with("/dev") {
            msg.push_str(" (raw devices need sudo, and on a Mac the card must be unmounted: diskutil unmountDisk ...)");
        }
        msg
    })
}

fn open_drives(paths: &[PathBuf]) -> Result<Vec<Arc<dyn Source>>> {
    paths.iter().map(|p| open_source(p)).collect()
}

/// Puts the drives of a RAID set together, using the best layout found.
fn open_raid(paths: &[PathBuf], array: &ArrayArgs) -> Result<(RaidSource, String)> {
    let drives = open_drives(paths)?;
    let hints = array.hints(drives.len())?;
    let found = detect(&drives, &hints, &AtomicBool::new(false));
    for n in &found.notes {
        eprintln!("  {n}");
    }
    let Some(best) = found.candidates.first() else {
        bail!("could not work out how these drives fit together; try `reperio raid` with --level, --chunk and --order");
    };
    eprintln!(
        "Combined {} drives as {} (confidence {:.0}%).",
        paths.len(),
        best.description,
        best.confidence * 100.0
    );
    if best.confidence < 0.5 {
        eprintln!("  That is a weak match; check `reperio raid` and give --level, --chunk and --order if you know them.");
    }
    for n in &best.notes {
        eprintln!("  {n}");
    }
    let src = best.layout.build(&drives).map_err(anyhow::Error::msg)?;
    let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    Ok((src, format!("raid:{}", names.join("+"))))
}

fn raid_report(paths: &[PathBuf], array: &ArrayArgs, json: bool) -> Result<()> {
    let drives = open_drives(paths)?;
    let hints = array.hints(drives.len())?;
    let found = detect(&drives, &hints, &AtomicBool::new(false));
    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(());
    }
    for (i, p) in paths.iter().enumerate() {
        println!(
            "Drive {}: {} ({})",
            i + 1,
            p.display(),
            size(drives[i].len())
        );
    }
    for n in &found.notes {
        println!("{n}");
    }
    if found.candidates.is_empty() {
        println!("No layout found.");
    }
    for (rank, c) in found.candidates.iter().enumerate() {
        println!();
        println!(
            "{}. {} - {:.0}% sure{}",
            rank + 1,
            c.description,
            c.confidence * 100.0,
            if c.from_metadata {
                " (from metadata)"
            } else {
                ""
            }
        );
        let order: Vec<String> = c
            .layout
            .order
            .iter()
            .map(|o| o.map_or("missing".into(), |i| format!("drive {}", i + 1)))
            .collect();
        println!("   order: {}", order.join(", "));
        if c.layout.offsets.iter().any(|&o| o > 0) {
            let offs: Vec<String> = c.layout.offsets.iter().map(|&o| size(o)).collect();
            println!("   data starts at: {}", offs.join(", "));
        }
        println!("   combined size: {}", size(c.size));
        if !c.found.is_empty() {
            println!("   holds: {}", c.found.join("; "));
        }
        for n in &c.notes {
            println!("   note: {n}");
        }
    }
    if let Some(best) = found.candidates.first() {
        println!();
        let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        println!(
            "To scan it: reperio scan --raid {}{}",
            list.join(","),
            if best.confidence < 0.5 {
                "  (add --level/--chunk/--order if you know them)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

pub(crate) fn scan(
    label: &str,
    sources: &[PathBuf],
    src: &dyn Source,
    opts: &ScanOptions,
    args: &ScanArgs,
) -> Result<()> {
    if !args.list {
        for s in sources {
            safety::check_output(s, &args.output)?;
        }
    }

    eprintln!(
        "Scanning {label} ({}) — read-only, nothing on it will be changed.",
        size(src.len())
    );
    let meter = RefCell::new(Meter::new());
    let phase = RefCell::new("Reading file system");
    let cancel = AtomicBool::new(false);
    let mut found = 0u64;
    let mut result = session::scan(src, label, opts, &cancel, |ev| match ev {
        Event::Volume(v) => {
            meter.borrow_mut().clear();
            let label = v
                .label
                .as_deref()
                .map(|l| format!(" \"{l}\""))
                .unwrap_or_default();
            let note = if v.readable { "" } else { " (deep scan only)" };
            let found = if v.found.is_empty() {
                String::new()
            } else {
                format!(" at byte {} — {}", v.offset, v.found)
            };
            eprintln!("  Volume: {}{label}, {}{note}{found}", v.fs, size(v.len));
            if !v.signature.is_empty() {
                eprintln!("    Signature at {}: {}", v.offset, v.signature);
            }
            let n = v.names;
            if !n.is_empty() {
                eprintln!(
                    "    Records of reused files: {} whole from the journal, {} names from the change journal, {} from old folder entries",
                    n.logfile, n.journal, n.index
                );
            }
        }
        Event::Phase(p) => {
            *phase.borrow_mut() = match p {
                Phase::FileSystem => "Reading file system",
                Phase::DeepScan => "Deep scan",
                Phase::Repair => "Rejoining files stored in pieces",
            }
        }
        Event::FreeSpace(c) => {
            if c.erased && opts.deep {
                meter.borrow_mut().clear();
                eprintln!(
                    "  Free space: {:.0}% of {} blocks sampled are blank. This drive has already \
                         erased its free space (TRIM), so deleted files are very likely gone; the \
                         deep scan is unlikely to find anything. Ctrl-C stops.",
                    c.blank_fraction * 100.0,
                    c.sampled
                );
            }
        }
        Event::Item(_) => found += 1,
        Event::Updated(_) => {}
        Event::Progress { done, total, .. } => {
            meter
                .borrow_mut()
                .update(&phase.borrow(), done, total, found)
        }
    });
    for path in &args.vault {
        let record = reperio_engine::vault::Vault::load(path)
            .with_context(|| format!("could not read the vault record {}", path.display()))?;
        let named = record.name_found_files(src, &mut result.items);
        if named > 0 {
            eprintln!("  Recovery Vault: {named} files found by deep scan got their names back.");
        }
    }
    meter.borrow_mut().clear();

    // Analyzer plug-ins look at the source, and files they offer join the results.
    if !args.analyzers.is_empty() {
        let dest = args
            .reports
            .clone()
            .unwrap_or_else(|| args.output.join("plug-in reports"));
        for name in &args.analyzers {
            let (offered, _) = android::run_analyzers_found(
                "image",
                &reperio_engine::analyzer::Target {
                    device: None,
                    image: Some(src),
                    backup: None,
                },
                &dest,
                Some(name),
                &[],
            )?;
            for (who, files) in offered {
                let first = result.items.len() as u32;
                let items = session::plugin_items(src, &files, &who, first);
                eprintln!(
                    "  {who}: {} recoverable files joined the results.",
                    items.len()
                );
                result.items.extend(items);
            }
        }
    }

    if let Some(path) = &args.save {
        result.save(path)?;
        safety::give_to_user(path);
    }

    let chosen: Vec<&Item> = result
        .items
        .iter()
        .filter(|i| args.existing || i.origin != Origin::Existing)
        .filter(|i| {
            args.damaged || matches!(i.health, Health::Good | Health::Unknown | Health::Repaired)
        })
        .collect();
    summarize(&result, &chosen);

    if args.list {
        for i in &chosen {
            let date = i.date().map(|d| d.to_string()).unwrap_or_default();
            println!(
                "{:<8} {:<11} {:>10}  {:<19}  {}",
                format!("{:?}", i.origin).to_lowercase(),
                format!("{:?}", i.health).to_lowercase(),
                size(i.size),
                date,
                i.path()
            );
        }
        return Ok(());
    }
    if chosen.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("could not create {}", args.output.display()))?;
    let mut meter = Meter::new();
    let recovered = session::recover(
        src,
        &chosen,
        &args.output,
        args.layout,
        &cancel,
        |done, total, _| meter.update("Recovering", done, total, 0),
    )?;
    meter.clear();
    session::write_report(
        &args.output.join("report.csv"),
        &args.output,
        &chosen,
        &recovered,
    )?;
    safety::give_tree_to_user(&args.output);

    let bad: u64 = recovered.iter().map(|r| r.bad_sectors).sum();
    eprintln!(
        "Recovered {} files to {}",
        recovered.len(),
        args.output.display()
    );
    if bad > 0 || result.bad_sectors > 0 {
        eprintln!(
            "Warning: {} unreadable sectors. The card may be failing; image it with `reperio image` and scan the image.",
            bad.max(result.bad_sectors)
        );
    } else if result.slow_reads > 0 {
        eprintln!(
            "Warning: {} very slow reads. The card may be starting to fail; image it with `reperio image` and scan the image.",
            result.slow_reads
        );
    }
    Ok(())
}

fn summarize(result: &ScanResult, chosen: &[&Item]) {
    let count = |o: Origin| result.items.iter().filter(|i| i.origin == o).count();
    eprintln!(
        "Found {} deleted files by name, {} more by deep scan, {} still on the card.",
        count(Origin::Deleted),
        count(Origin::Carved),
        count(Origin::Existing)
    );
    let named: usize = result.volumes.iter().map(|v| v.names.named_finds).sum();
    if named > 0 {
        eprintln!("{named} files found by deep scan were given back their names from the file system's journals or old folder entries.");
    }
    let kept = count(Origin::Snapshot);
    if kept > 0 {
        eprintln!("{kept} more deleted or changed files are kept by snapshots of the drive.");
    }
    let damaged = result
        .items
        .iter()
        .filter(|i| matches!(i.health, Health::Damaged | Health::Overwritten))
        .count();
    if damaged > 0 {
        eprintln!("{damaged} of them are damaged or partly overwritten (use --damaged to recover them anyway).");
    }
    let bytes: u64 = chosen.iter().map(|i| i.size).sum();
    eprintln!("Selected {} files, {}.", chosen.len(), size(bytes));
}

/// Set by Ctrl-C to stop imaging cleanly, with the map saved.
static STOP: AtomicBool = AtomicBool::new(false);
/// In the menu, Ctrl-C only ever stops the task at hand, never the menu.
static KEEP_RUNNING: AtomicBool = AtomicBool::new(false);

/// Exit code of `image` when stopped by Ctrl-C.
pub const STOPPED: i32 = 130;
/// Exit code of `image` when verification found the image differs.
pub const MISMATCH: i32 = 2;

/// Images `source` to `output`. Returns the exit code: 0, [`STOPPED`] or
/// [`MISMATCH`].
pub fn image(
    source: &Path,
    output: &Path,
    map: Option<PathBuf>,
    resume: bool,
    opts: &ImageOptions,
    forensic: Option<forensic::ForensicOptions>,
) -> Result<i32> {
    let src = open_source(source)?;
    let segments = imaging::segment_paths(output, src.len(), opts.split);
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    safety::check_output(source, parent)?;
    let map = map.unwrap_or_else(|| {
        let mut m = output.as_os_str().to_owned();
        m.push(".map");
        PathBuf::from(m)
    });
    for path in segments.iter().chain([&map]) {
        if same_file(path, source) {
            bail!("{} is the disk being imaged", path.display());
        }
    }
    let existing = segments.iter().find(|p| p.exists());
    if resume {
        if let (false, Some(out)) = (map.exists(), existing) {
            bail!(
                "there is no map file at {}, so what {} holds is unknown; give its map with --map",
                map.display(),
                out.display()
            );
        }
    } else if let Some(out) = existing {
        bail!(
            "{} already exists; choose a new file name, or add --resume to continue copying into it",
            out.display()
        );
    } else if map.exists() {
        bail!(
            "{} already exists from an earlier copy; add --resume to continue it, or delete it",
            map.display()
        );
    }

    if map.exists() {
        let m = Mapfile::load(&map, src.len())
            .with_context(|| format!("could not use {}", map.display()))?;
        eprintln!(
            "Resuming the copy of {} ({}) to {}: {} copied so far.",
            source.display(),
            size(src.len()),
            output.display(),
            size(m.total(imaging::Status::Finished))
        );
    } else {
        eprintln!(
            "Imaging {} ({}) to {} — read-only, nothing on it will be changed.",
            source.display(),
            size(src.len()),
            output.display()
        );
    }
    eprintln!(
        "Progress is kept in {}. Ctrl-C stops; --resume continues.",
        map.display()
    );
    stop_on_ctrl_c();

    if opts.split.is_some() {
        eprintln!(
            "The image is written in {} pieces: {} to {}.",
            segments.len(),
            segments[0].display(),
            segments[segments.len() - 1].display()
        );
    }

    let mut meter = Meter::new();
    let result = match &forensic {
        None => imaging::image(&src, output, &map, opts, &STOP, |p| {
            meter.show(&image_status(p))
        })
        .map(|o| (o, None)),
        Some(f) => forensic::acquire(&src, source, output, &map, opts, f, &STOP, |p| match p {
            forensic::Progress::Imaging(p) => meter.show(&image_status(p)),
            forensic::Progress::Checking(c) => meter.show(&check_status(c)),
        })
        .map(|a| (a.outcome, Some((a.report, a.report_paths)))),
    };
    meter.clear();
    for path in segments.iter().chain([&map]) {
        safety::give_to_user(path);
    }
    let (out, report) =
        result.with_context(|| format!("imaging to {} failed", output.display()))?;
    if let Some((_, Some((json, txt)))) = &report {
        safety::give_to_user(json);
        safety::give_to_user(txt);
    }

    if out.cancelled {
        eprintln!(
            "Stopped: {} copied, {} unreadable, {} still to read. Run the same command with --resume to continue.",
            size(out.finished),
            size(out.bad),
            size(out.unread)
        );
        return Ok(STOPPED);
    }
    eprintln!(
        "Done: {} copied, {} unreadable ({} sectors) left as zeros in the image.",
        size(out.finished),
        size(out.bad),
        out.bad.div_ceil(opts.sector)
    );
    if out.unread > 0 {
        eprintln!(
            "{} in damaged areas was not read (--no-scrape). Run again with --resume to read it.",
            size(out.unread)
        );
    }
    const LIST: usize = 20;
    if !out.bad_ranges.is_empty() {
        eprintln!("Unreadable areas:");
        for &(pos, len) in out.bad_ranges.iter().take(LIST) {
            eprintln!(
                "  bytes {pos}..{} ({}), sectors {}..{}",
                pos + len,
                size(len),
                pos / opts.sector,
                (pos + len).div_ceil(opts.sector) - 1
            );
        }
        if out.bad_ranges.len() > LIST {
            eprintln!(
                "  ...and {} more; all are listed in {}.",
                out.bad_ranges.len() - LIST,
                map.display()
            );
        }
    }
    match report {
        None => {}
        Some((None, _)) => {
            eprintln!("Hashing was stopped; run the same command with --resume to hash and write the report.");
            return Ok(STOPPED);
        }
        Some((Some(r), paths)) => {
            for (name, hex) in r.hashes.list() {
                println!("{name:<8} {hex}");
            }
            if let Some(v) = &r.verification {
                if v.matched {
                    eprintln!("Verified: the drive read again gives the same hashes as the image.");
                } else {
                    eprintln!(
                        "Verification FAILED: {} areas differ and {} could not be read again; they are listed in the report.",
                        v.differing.len(),
                        v.unreadable_now.len()
                    );
                }
            }
            if let Some((json, txt)) = paths {
                eprintln!("Report: {} and {}", txt.display(), json.display());
            }
            if r.verification.as_ref().is_some_and(|v| !v.matched) {
                return Ok(MISMATCH);
            }
        }
    }
    Ok(0)
}

fn check_status(c: &forensic::CheckProgress) -> String {
    let what = match c.stage {
        forensic::Stage::Hashing => "Hashing the image",
        forensic::Stage::Verifying => "Verifying against the source",
    };
    let pct = if c.total == 0 {
        100.0
    } else {
        c.done as f64 * 100.0 / c.total as f64
    };
    format!("{what}: {pct:5.1}% of {}", size(c.total))
}

fn image_status(p: &ImageProgress) -> String {
    let pass = match p.pass {
        Pass::Copy => "Copying".to_string(),
        Pass::Trim => "Trimming damaged areas".to_string(),
        Pass::Scrape => "Scraping damaged areas".to_string(),
        Pass::Retry => format!("Retrying (pass {})", p.pass_number),
        Pass::Finished => "Finishing".to_string(),
    };
    let pct = if p.total == 0 {
        100.0
    } else {
        p.done() as f64 * 100.0 / p.total as f64
    };
    format!(
        "{pass}: {pct:5.1}% | {} copied, {} bad, {} to read | {}/s | {} errors | at {}",
        size(p.finished),
        size(p.bad),
        size(p.non_tried + p.pending),
        size(p.rate as u64),
        p.errors,
        size(p.pos)
    )
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Makes Ctrl-C stop imaging at the next read, so the map is saved. A
/// second Ctrl-C quits at once, for a card stuck on a read (except in the
/// menu, see [`KEEP_RUNNING`]).
fn stop_on_ctrl_c() {
    #[cfg(unix)]
    {
        extern "C" fn on_signal(sig: libc::c_int) {
            STOP.store(true, Ordering::Relaxed);
            if !KEEP_RUNNING.load(Ordering::Relaxed) {
                // SAFETY: signal() is async-signal-safe.
                unsafe {
                    libc::signal(sig, libc::SIG_DFL);
                }
            }
        }
        // SAFETY: the handler only stores an atomic and resets itself.
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
    }
}

/// A size or position: bytes, with K/M/G/T (1000s), KiB/MiB/GiB/TiB
/// (1024s), or a percentage of `whole`.
fn parse_size(text: &str, whole: u64) -> Result<u64> {
    let t = text.trim();
    if let Some(pct) = t.strip_suffix('%') {
        let p: f64 = pct.trim().parse().context("not a percentage")?;
        if !(0.0..=100.0).contains(&p) {
            bail!("{text}: a percentage is 0 to 100");
        }
        return Ok((whole as f64 * p / 100.0) as u64);
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().with_context(|| format!("{text}: not a size"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "t" | "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => bail!("{text}: unknown unit {unit:?}"),
    };
    let v = n * mult as f64;
    if !v.is_finite() || v < 0.0 || v > u64::MAX as f64 {
        bail!("{text}: out of range");
    }
    Ok(v as u64)
}

fn surface(
    source: &Path,
    range: Option<&str>,
    block: Option<&str>,
    state: Option<PathBuf>,
) -> Result<()> {
    use reperio_engine::surface::{self, Class, SurfaceMap, SurfaceOptions};

    let src = open_source(source)?;
    let len = src.len();
    let mut opts = SurfaceOptions::default();
    if let Some(r) = range {
        let (a, b) = r
            .split_once("..")
            .context("--range is START..END, for example 10G..20G")?;
        opts.start = if a.trim().is_empty() {
            0
        } else {
            parse_size(a, len)?
        };
        opts.end = if b.trim().is_empty() {
            None
        } else {
            Some(parse_size(b, len)?)
        };
        if opts.end.is_some_and(|e| e <= opts.start) {
            bail!("--range ends before it starts");
        }
    }
    if let Some(b) = block {
        opts.block = parse_size(b, len)?;
    }

    let mut map = match state.as_deref().filter(|p| p.exists()) {
        Some(path) => {
            let json = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let m = SurfaceMap::from_json(&json, len)
                .map_err(|e| anyhow::anyhow!("can't continue from {}: {e}", path.display()))?;
            if range.is_some() || block.is_some() {
                eprintln!(
                    "Continuing the test saved in {}; its range and block size are kept.",
                    path.display()
                );
            }
            eprintln!("Continuing from {} of {}.", size(m.done()), size(m.total()));
            m
        }
        None => SurfaceMap::new(len, &opts),
    };
    if map.blocks() == 0 {
        bail!("nothing to test in that range");
    }
    eprintln!(
        "Testing {} of {} from {} ({}, {} blocks) — read-only, nothing on it will be changed. Ctrl-C stops.",
        size(map.total()),
        source.display(),
        size(map.start),
        size(map.block),
        map.blocks()
    );
    stop_on_ctrl_c();
    let mut meter = Meter::new();
    let finished = surface::run(&src, &mut map, &STOP, |m| {
        let s = m.summary();
        let pct = m.done() as f64 * 100.0 / m.total().max(1) as f64;
        let rate = s
            .median_rate
            .map_or(String::new(), |r| format!(" | {}/s", size(r as u64)));
        meter.show(&format!(
            "{pct:5.1}%{rate} | {} slow, {} very slow, {} unreadable",
            s.slow,
            s.very_slow,
            s.unreadable + s.skipped
        ));
    });
    meter.clear();
    if let Some(path) = &state {
        std::fs::write(path, map.to_json())
            .with_context(|| format!("could not save {}", path.display()))?;
        safety::give_to_user(path);
    }

    let s = map.summary();
    let blocks = |n: u64| size(n * map.block);
    println!(
        "{} {} of {} read{}.",
        if finished { "Done:" } else { "Stopped:" },
        size(s.tested),
        size(s.total),
        s.median_rate.map_or(String::new(), |r| format!(
            ", usually at {}/s",
            size(r as u64)
        ))
    );
    println!("  normal      {:>10}  ({} blocks)", blocks(s.fast), s.fast);
    println!("  slow        {:>10}  ({} blocks)", blocks(s.slow), s.slow);
    println!(
        "  very slow   {:>10}  ({} blocks)",
        blocks(s.very_slow),
        s.very_slow
    );
    println!(
        "  unreadable  {:>10}  ({} blocks, {} bad sectors)",
        blocks(s.unreadable),
        s.unreadable,
        s.bad_sectors
    );
    if s.skipped > 0 {
        println!(
            "  skipped     {:>10}  (inside damaged areas; most likely unreadable too)",
            blocks(s.skipped)
        );
    }
    if let Some((at, us)) = s.slowest {
        println!("  slowest block at {} took {} ms", size(at), us / 1000);
    }

    // The map: 64 columns, each character an equal slice of the range.
    const COLS: usize = 64;
    let coarse = map.coarse(COLS * 16);
    let per = map.total() / coarse.len().max(1) as u64;
    println!();
    println!(
        "Map ({} per character; . normal, s slow, S very slow, X unreadable, ? skipped, blank not read):",
        size(per)
    );
    for (row, line) in coarse.as_bytes().chunks(COLS).enumerate() {
        let line: String = line
            .iter()
            .map(|&c| {
                if c == Class::Untested.symbol() as u8 {
                    ' '
                } else {
                    c as char
                }
            })
            .collect();
        println!(
            "  {:>9} |{line}|",
            size(map.start + (row * COLS) as u64 * per)
        );
    }
    if !s.unreadable_ranges.is_empty() {
        println!();
        println!("Unreadable areas:");
        for &(at, n) in s.unreadable_ranges.iter().take(20) {
            println!("  bytes {at}..{} ({})", at + n, size(n));
        }
        if s.damaged_areas > 20 {
            println!("  ...and {} more", s.damaged_areas - 20);
        }
    }
    if s.unreadable + s.skipped > 0 || s.very_slow > 0 {
        println!();
        println!(
            "This drive is struggling. Copy it first (reperio image), then recover from the copy."
        );
    }
    if !finished {
        match &state {
            Some(p) => eprintln!("Run again with --state {} to continue.", p.display()),
            None => eprintln!("Add --state FILE to be able to continue a stopped test."),
        }
        std::process::exit(130);
    }
    Ok(())
}

fn health(device: &Path) -> Result<()> {
    use reperio_engine::health::{self, Media, Verdict};
    let r = health::report(device);
    let verdict = match r.verdict {
        Verdict::Good => "Good",
        Verdict::Watch => "Watch it",
        Verdict::Failing => "FAILING",
        Verdict::Unknown => "Unknown",
    };
    println!("{}: {verdict}", device.display());
    for why in &r.reasons {
        println!("  {why}");
    }
    println!();
    let row = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            println!("  {k:<22} {v}");
        }
    };
    row("Model", r.model.clone());
    row("Serial", r.serial.clone());
    row("Firmware", r.firmware.clone());
    row("Capacity", r.capacity.map(size));
    row("Interface", r.interface.clone());
    row(
        "Type",
        r.media.map(|m| match m {
            Media::Ssd => "SSD".to_string(),
            Media::Hdd => "Hard drive".to_string(),
        }),
    );
    row("Temperature", r.temperature.map(|t| format!("{t} °C")));
    row("Hours in use", r.power_on_hours.map(|h| h.to_string()));
    row("Times switched on", r.power_cycles.map(|h| h.to_string()));
    row(
        "Replaced sectors",
        r.reallocated_sectors.map(|h| h.to_string()),
    );
    row("Pending sectors", r.pending_sectors.map(|h| h.to_string()));
    row(
        "Unreadable sectors",
        r.uncorrectable_sectors.map(|h| h.to_string()),
    );
    row("Wear", r.percentage_used.map(|h| format!("{h}% used")));
    row(
        "Spare blocks",
        r.available_spare
            .map(|s| match r.available_spare_threshold {
                Some(t) => format!("{s}% (critical at {t}%)"),
                None => format!("{s}%"),
            }),
    );
    row("Data errors", r.media_errors.map(|h| h.to_string()));
    for n in &r.notes {
        println!();
        println!("Note: {n}");
    }
    Ok(())
}

fn inspect(source: &Path, name: &str, every_sector: bool) -> Result<()> {
    use reperio_engine::extent::MappedSource;
    use reperio_engine::formats;
    use reperio_engine::fs::{self as vfs, partition};
    use reperio_engine::reader::Reader;

    let src = open_source(source)?;
    let all = formats::all();
    println!("{} ({} bytes)", source.display(), src.len());
    // The partition table's volumes, then volumes found by searching the
    // drive where none of those could be read (or all of it, if asked).
    let mut parts: Vec<_> = partition::partitions(&src)
        .into_iter()
        .map(|p| {
            let fs = vfs::detect_volume(&src, p.offset, p.len);
            let vol = vfs::open(&src, p.offset, p.len);
            (p.offset, p.len, fs, p.label, vol)
        })
        .collect();
    let read: Vec<(u64, u64)> = parts
        .iter()
        .filter_map(|p| p.4.as_ref().map(|v| (v.offset, v.offset + v.len)))
        .collect();
    let mut areas = Vec::new();
    let mut pos = 0;
    let mut sorted = read.clone();
    sorted.sort_unstable();
    for (a, e) in sorted {
        if a > pos {
            areas.push((pos, a));
        }
        pos = pos.max(e);
    }
    areas.push((pos, src.len()));
    if every_sector {
        areas = vec![(0, src.len())];
    }
    let known: Vec<u64> = read.iter().map(|r| r.0).collect();
    let found = vfs::lost::find(
        &src,
        &areas,
        every_sector,
        &known,
        &AtomicBool::new(false),
        &mut |_, _| {},
    );
    for f in found {
        let how = if f.backup {
            "Found partition, by a backup copy of its first sector"
        } else {
            "Found partition"
        };
        let v = f.volume;
        parts.push((v.offset, v.len, Some(v.fs), how.to_string(), Some(v)));
    }
    for (offset, len, fs, label, vol) in parts {
        println!(
            "partition at {} ({} bytes): {} [{}]",
            offset,
            len,
            fs.map_or("unknown", |f| f.name()),
            label
        );
        let Some(vol) = vol else {
            continue;
        };
        println!(
            "  {} label={:?} cluster={} files={}",
            vol.fs.name(),
            vol.label,
            vol.cluster_size,
            vol.files.len()
        );
        let needle = name.to_lowercase();
        for f in vol
            .files
            .iter()
            .filter(|f| f.name.to_lowercase().contains(&needle))
        {
            println!();
            println!(
                "  {}  size={} deleted={} overwritten={}",
                vfs::join_path(&f.dir, &f.name),
                f.size,
                f.deleted,
                f.overwritten
            );
            let layouts = std::iter::once(("recorded", &f.extents))
                .chain(f.alternatives.iter().map(|a| ("alternative", a)));
            for (n, (what, extents)) in layouts.enumerate() {
                let view = MappedSource::new(&src, extents.clone());
                let mut head = vec![0u8; formats::HEAD_LEN];
                let got = view.read_at(0, &mut head).unwrap_or(0);
                let hex: Vec<String> = head[..got.min(24)]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                let pieces: Vec<String> = extents
                    .iter()
                    .take(6)
                    .map(|e| format!("{}+{}", e.offset, e.len))
                    .collect();
                print!(
                    "    layout {n} ({what}): {} piece(s) [{}{}] starts {}",
                    extents.len(),
                    pieces.join(", "),
                    if extents.len() > 6 { ", ..." } else { "" },
                    hex.join(" ")
                );
                match formats::identify(&all, &head) {
                    None => println!(" -> not a format Reperio recognises"),
                    Some(fmt) => {
                        let limit = view.len().min(fmt.max_len());
                        match fmt.carve(&mut Reader::new(&view, 0), 0, limit) {
                            Some(found) => println!(
                                " -> {} file of {} bytes (recorded size {})",
                                found.ext, found.len, f.size
                            ),
                            None => {
                                println!(" -> starts like {} but its structure breaks", fmt.name())
                            }
                        }
                        if fmt.name() == "mp4" {
                            print_boxes(&view);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Lists an MP4/MOV's top-level boxes, for `inspect`.
fn print_boxes(view: &dyn Source) {
    let mut pos = 0u64;
    for _ in 0..16 {
        let mut h = [0u8; 16];
        if view.read_at(pos, &mut h).unwrap_or(0) < 8 {
            break;
        }
        let size32 = u32::from_be_bytes(h[..4].try_into().unwrap());
        let size = if size32 == 1 {
            u64::from_be_bytes(h[8..16].try_into().unwrap())
        } else {
            u64::from(size32)
        };
        println!(
            "      box at {pos}: {:?} size {size}",
            String::from_utf8_lossy(&h[4..8])
        );
        if size < 8 {
            break;
        }
        pos += size;
    }
}

/// A single self-updating status line on stderr.
pub struct Meter {
    started: Instant,
    last: Instant,
    shown: bool,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now - Duration::from_secs(1),
            shown: false,
        }
    }

    pub fn update(&mut self, what: &str, done: u64, total: u64, found: u64) {
        if self.last.elapsed() < Duration::from_millis(250) {
            return;
        }
        let pct = if total == 0 {
            100.0
        } else {
            done as f64 * 100.0 / total as f64
        };
        let found = if found > 0 {
            format!(" | {found} found")
        } else {
            String::new()
        };
        self.show(&format!("{what}: {pct:5.1}%{found}"));
    }

    /// Shows `status` with the time elapsed, at most four times a second.
    fn show(&mut self, status: &str) {
        if self.last.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last = Instant::now();
        eprint!(
            "\r\x1b[K{status} | {} elapsed",
            duration(self.started.elapsed().as_secs_f64())
        );
        self.shown = true;
    }

    pub fn clear(&mut self) {
        if self.shown {
            eprint!("\r\x1b[K");
            self.shown = false;
        }
    }
}

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn duration(secs: f64) -> String {
    let s = secs as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

fn run_redteam(
    source: &Path,
    dest: &Path,
    category: Option<&str>,
    confidence: &str,
    aggressive: bool,
) -> Result<()> {
    let src = crate::open_source(source)?;
    println!(
        "Red Team Forensic Suite: analyzing {} ...",
        source.display()
    );

    let target = reperio_engine::analyzer::Target {
        device: None,
        image: Some(&src),
        backup: None,
    };

    let redteam_plugins: Vec<&str> = vec![
        "registry-deep-dive",
        "browser-forensics",
        "crypto-hunter",
        "anti-forensics",
        "windows-artifact-hunter",
        "ntfs-redteam",
        "evtx-carver",
        "memory-hunter",
        "lateral-movement-hunter",
        "geo-correlator",
        "stego-misdirection",
        "comm-app-hunter",
        "cloud-sync-hunter",
        "ios-deep-extract",
        "android-deep-extract",
    ];

    std::fs::create_dir_all(dest)?;
    let mut all_findings: Vec<serde_json::Value> = Vec::new();
    let mut failed = false;
    let never = AtomicBool::new(false);

    let plugins: Vec<_> = reperio_engine::analyzer::runnable_on("image")
        .into_iter()
        .filter(|p| redteam_plugins.contains(&p.name.as_str()))
        .collect();

    if plugins.is_empty() {
        bail!("No redteam plug-ins found. They are not part of the open-source release (see plugins/README.md); install them in a plug-ins folder (`reperio plugins` lists them).");
    }

    for p in plugins {
        println!("== {}", p.name);
        let report = reperio_engine::analyzer::run(
            &p,
            &target,
            "image",
            dest,
            &reperio_engine::analyzer::RunOptions { args: vec![] },
            &mut |pr| {
                if !pr.text.is_empty() {
                    eprint!("\r  {}                    ", pr.text);
                }
            },
            &never,
        );
        eprintln!();

        if let Some(e) = &report.error {
            println!("   ERROR: {e}");
            failed = true;
        }

        for f in &report.findings {
            let f_conf = f.confidence.as_deref().unwrap_or("low");
            let keep = matches!(
                (confidence, f_conf),
                ("high", "high") | ("medium", "high") | ("medium", "medium") | ("low", _)
            );
            let cat_ok = category.is_none_or(|cat| f.tags.iter().any(|t| t == cat));
            if keep && cat_ok {
                all_findings.push(serde_json::json!({
                    "plugin": p.name,
                    "title": f.title,
                    "detail": f.detail,
                    "severity": f.severity,
                    "count": f.count,
                    "tags": f.tags,
                    "confidence": f.confidence,
                }));
            }
        }
    }

    let summary = serde_json::json!({
        "source": source.to_string_lossy(),
        "aggressive": aggressive,
        "findings": all_findings,
    });
    let summary_path = dest.join("redteam-summary.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;
    println!("Red Team summary written to {}", summary_path.display());

    if failed {
        bail!("one or more redteam plugins did not finish");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_sizes() {
        assert_eq!(parse_segment_size("2G"), Ok(2 << 30));
        assert_eq!(parse_segment_size("650M"), Ok(650 << 20));
        assert_eq!(parse_segment_size("4gib"), Ok(4 << 30));
        assert_eq!(parse_segment_size("1024"), Ok(1024));
        assert!(parse_segment_size("1000").is_err(), "not whole sectors");
        assert!(parse_segment_size("0").is_err());
        assert!(parse_segment_size("2X").is_err());
        assert!(parse_segment_size("99999999999T").is_err());
    }

    #[test]
    fn forensic_mode_only_when_asked() {
        assert!(ForensicArgs::default().options().is_none());
        let f = ForensicArgs {
            verify: true,
            ..ForensicArgs::default()
        };
        let o = f.options().unwrap();
        assert!(o.verify && o.algorithms == forensic::Algorithms::ALL);
    }
}

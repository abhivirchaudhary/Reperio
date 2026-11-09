//! A complete recovery scan of one source, and recovery of its results.
//!
//! A scan runs in two phases:
//!
//! 1. **File system.** Every partition is checked for a filesystem Reperio
//!    can read. Its directories give deleted files back with their names,
//!    folders and dates; each one is checked against its format so damaged
//!    files are flagged instead of silently returned.
//! 2. **Deep scan.** The free space of every readable volume, and all space
//!    outside readable volumes, is stitched into one stream and carved.
//!    Carving only free space skips over the clusters of files still on the
//!    card, so a photo that was written around them comes back whole.
//!    Results that duplicate a file from phase 1 are merged into it.

use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::docfix::{self, DocRepair};
use crate::extent::{total_len, Extent, MappedSource};
use crate::formats::{self, jpeg, DateTime, Format, Found, Kind, HEAD_LEN};
use crate::fs::{
    self as volume_fs, partition, FsFile, KnownName, NameSources, SnapshotFile, Volume,
};
use crate::photofix::{self, PhotoRepair};
use crate::reader::Reader;
use crate::repair;
use crate::scan::Scanner;
use crate::source::{read_tolerant, Source};
use crate::videofix;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// A deleted file found in the filesystem, with its original name.
    Deleted,
    /// A file that is still on the card.
    Existing,
    /// Found by the deep scan; the filesystem no longer knows it.
    Carved,
    /// Gone from the volume, but kept by one of its snapshots (APFS
    /// snapshots, Windows shadow copies), with its original name.
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// The file's structure checks out from start to end.
    Good,
    /// The file's structure is broken; it may open partially or not at all.
    Damaged,
    /// Some of the file's space has been reused by a newer file.
    Overwritten,
    /// Not a format Reperio can check.
    Unknown,
    /// Damaged, but Reperio rebuilt it (a video's missing index, a photo's
    /// header, a document's table of contents) or rescued what it could
    /// (a photo's preview, a document's text); see [`Item::fix`].
    Repaired,
}

/// How a file is put back together on recovery when its data alone won't
/// open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Fix {
    /// A video whose index is rebuilt (see [`crate::videofix`]): using a
    /// good video from the same camera as a guide, or, with no guide,
    /// from the settings the camera repeats in the video itself.
    Video {
        #[serde(default)]
        reference: Vec<Extent>,
        #[serde(default)]
        reference_name: String,
        /// The repaired video plays, but without sound.
        silent: bool,
        /// The camera, when recognised.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        camera: Option<String>,
    },
    /// A photo repaired as [`crate::photofix`] describes. `reference` is
    /// the good photo whose header was used, if one was.
    Photo {
        how: PhotoRepair,
        #[serde(default)]
        reference: Vec<Extent>,
    },
    /// A document repaired as [`crate::docfix`] describes. `has_text`: the
    /// text of parts that couldn't be repaired is kept beside it.
    Document {
        how: DocRepair,
        #[serde(default)]
        has_text: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: u32,
    pub name: String,
    /// Original folder, `/` separated; empty for carved files.
    pub dir: String,
    pub ext: String,
    pub kind: Kind,
    pub origin: Origin,
    pub health: Health,
    pub size: u64,
    /// When the photo was taken or the video recorded, from the file itself.
    pub taken: Option<DateTime>,
    /// Last modified, from the filesystem.
    pub modified: Option<DateTime>,
    pub extents: Vec<Extent>,
    /// Index into [`ScanResult::volumes`].
    pub volume: Option<u32>,
    /// How to rebuild the file on recovery, for repaired files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
    /// The plug-in that found it, for files a plug-in reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found_by: Option<String>,
    /// NSRL known-file status: known = system file, unknown = not in database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsrl_status: Option<String>,
}

impl Item {
    /// The best date for sorting and naming.
    pub fn date(&self) -> Option<DateTime> {
        self.taken.or(self.modified)
    }

    pub fn path(&self) -> String {
        volume_fs::join_path(&self.dir, &self.name)
    }

    /// The extension the damaged file had: a document whose text alone was
    /// rescued is named `.txt` but was read from a Word, PDF or Office file.
    fn ext_before_repair(&self) -> String {
        match &self.fix {
            Some(Fix::Document {
                how: DocRepair::Text,
                ..
            }) => "doc".into(),
            _ => self.ext.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub fs: String,
    pub label: Option<String>,
    pub offset: u64,
    pub len: u64,
    /// Reperio read this volume's directories.
    pub readable: bool,
    /// Files its directories list, still there and deleted.
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub deleted: usize,
    /// Bytes of free space, which the deep scan searches.
    #[serde(default)]
    pub free: u64,
    /// For a volume that could not be read, the bytes that identify file
    /// systems (its first sector, and where APFS and HFS+ sign theirs), so
    /// a report of it says what is there.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
    /// Deleted files found other than through the file table (on NTFS:
    /// its journals and old folder index entries), and how many deep-scan
    /// finds took one of their names.
    #[serde(default, skip_serializing_if = "NameSources::is_empty")]
    pub names: NameSources,
    /// For a volume no partition table lists, found by searching the drive
    /// for file system structures (see [`volume_fs::lost`]): "Found
    /// partition", and how. Empty for the partition table's own volumes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub found: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    FileSystem,
    DeepScan,
    /// Rejoining photos and videos found in pieces.
    Repair,
}

/// What sampling the free space of a drive's volumes found, right after
/// their file systems were read. SSDs (and some cards) erase freed space
/// soon after files are deleted (TRIM); then the deep scan has nothing
/// left to find, and people should hear so before waiting hours for it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FreeSpaceCheck {
    /// Blocks of free space read, and how many were blank (all zeros or
    /// all 0xFF, as erased flash reads).
    pub sampled: u32,
    pub blank: u32,
    pub blank_fraction: f64,
    /// Deleted files whose first block was read, and how many of those
    /// were blank.
    #[serde(default)]
    pub deleted_sampled: u32,
    #[serde(default)]
    pub deleted_blank: u32,
    /// Nearly all of the free space is blank: it has been erased, and
    /// deleted files are most likely gone.
    pub erased: bool,
}

pub enum Event<'a> {
    Phase(Phase),
    Volume(&'a VolumeInfo),
    /// The free space was sampled; see [`FreeSpaceCheck`].
    FreeSpace(&'a FreeSpaceCheck),
    Item(&'a Item),
    /// An item found earlier was improved (for example, a damaged deleted
    /// file whose intact copy the deep scan found).
    Updated(&'a Item),
    Progress {
        done: u64,
        total: u64,
        /// Where on the source the scan is reading, during the deep scan.
        at: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanOptions {
    /// Run the deep scan after reading the filesystem.
    pub deep: bool,
    /// List files that are still on the card, too.
    pub existing: bool,
    /// Restrict the deep scan to these format names (see
    /// [`Scanner::format_names`]); empty means all.
    pub formats: Vec<String>,
    /// Continue a deep scan that stopped partway.
    #[serde(default)]
    pub resume: Option<Resume>,
    /// Search every sector of the drive for volumes no partition table
    /// lists, not just the likely places outside readable volumes (which
    /// are always searched). Slower: it reads the whole drive once more.
    #[serde(default)]
    pub find_partitions: bool,
    /// Which plug-ins (by name) may read volumes in this scan; `None` means
    /// all that are installed, an empty list none.
    #[serde(default)]
    pub plugins: Option<Vec<String>>,
    /// Track this scan under a case and evidence item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_id: Option<u64>,
    /// Path to an NSRL SQLite database for filtering known files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsrl_db: Option<std::path::PathBuf>,
}

/// Where an interrupted deep scan had got to, so it can continue there
/// instead of starting over.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Resume {
    /// Position reached in the deep scan's stream of free space.
    pub deep_from: u64,
    /// Length of that stream; if the card has changed since, it differs and
    /// the scan starts over.
    pub deep_total: u64,
    /// Files the deep scan had found.
    pub carved: Vec<Item>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            deep: true,
            existing: true,
            formats: Vec::new(),
            resume: None,
            find_partitions: false,
            plugins: None,
            case_id: None,
            evidence_id: None,
            nsrl_db: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanResult {
    pub source: String,
    pub source_len: u64,
    pub volumes: Vec<VolumeInfo>,
    pub items: Vec<Item>,
    pub bad_sectors: u64,
    /// Reads that worked but were very slow; see [`crate::scan::Outcome`].
    #[serde(default)]
    pub slow_reads: u64,
    pub cancelled: bool,
    /// Whether the free space has already been erased (TRIM); `None` when
    /// there was too little to tell from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_space: Option<FreeSpaceCheck>,
}

impl ScanResult {
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let f = BufWriter::new(File::create(path)?);
        serde_json::to_writer(f, self).map_err(io::Error::other)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let f = io::BufReader::new(File::open(path)?);
        serde_json::from_reader(f).map_err(io::Error::other)
    }
}

/// Runs a full scan. `cancel` may be set from another thread to stop early;
/// the result then holds what was found so far.
pub fn scan(
    src: &dyn Source,
    name: &str,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> ScanResult {
    crate::plugin::set_enabled(opts.plugins.clone());
    let formats = formats::all();
    let mut result = ScanResult {
        source: name.to_string(),
        source_len: src.len(),
        ..Default::default()
    };

    on_event(Event::Phase(Phase::FileSystem));
    // Volumes the partition table lists, and whether they could be read.
    let mut volumes: Vec<(VolumeInfo, Option<Volume>, bool)> = Vec::new();
    for part in partition::partitions(src) {
        let fs_type = volume_fs::detect_volume(src, part.offset, part.len);
        let volume = fs_type
            .filter(|t| t.readable())
            .and_then(|_| volume_fs::open(src, part.offset, part.len))
            // Custom readers and plug-ins for what Reperio cannot read.
            .or_else(|| volume_fs::open_foreign(src, part.offset, part.len));
        let mut info = volume_info(src, volume.as_ref(), part.offset, part.len);
        if let (None, Some(t)) = (&volume, fs_type) {
            info.fs = t.name().to_string();
        }
        let whole = part.label == "Whole disk";
        volumes.push((info, volume, whole));
    }

    // Volumes the table does not list: always searched for where no
    // readable volume is, and over the whole drive when asked.
    let read: Vec<(u64, u64)> = volumes
        .iter()
        .filter_map(|(_, v, _)| {
            v.as_ref()
                .map(|v| (v.offset, v.offset.saturating_add(v.len)))
        })
        .collect();
    let areas = if opts.find_partitions {
        vec![(0, src.len())]
    } else {
        outside(&read, src.len())
    };
    let known: Vec<u64> = read.iter().map(|r| r.0).collect();
    let lost = volume_fs::lost::find(
        src,
        &areas,
        opts.find_partitions,
        &known,
        cancel,
        &mut |done, total| {
            on_event(Event::Progress {
                done,
                total,
                at: None,
            })
        },
    );
    if cancel.load(Ordering::Relaxed) {
        result.cancelled = true;
        return result;
    }
    if !lost.is_empty() {
        // "Whole disk" only stood for a drive without a partition table.
        volumes.retain(|(_, v, whole)| v.is_some() || !whole);
    }
    for f in lost {
        let v = f.volume;
        let mut info = volume_info(src, Some(&v), v.offset, v.len);
        info.found = if f.backup {
            "Found partition, by a backup copy of its first sector".into()
        } else {
            "Found partition".into()
        };
        // A listed partition that could not be read is this volume.
        match volumes
            .iter_mut()
            .find(|(i, vol, _)| vol.is_none() && i.offset == v.offset)
        {
            Some(slot) => *slot = (info, Some(v), false),
            None => volumes.push((info, Some(v), false)),
        }
    }
    volumes.sort_by_key(|(i, _, _)| i.offset);

    let mut readable = Vec::new();
    for (info, volume, _) in volumes {
        on_event(Event::Volume(&info));
        result.volumes.push(info);
        if let Some(v) = volume {
            readable.push((result.volumes.len() as u32 - 1, v));
        }
    }

    result.free_space = check_free_space(src, &readable, cancel);
    if let Some(check) = &result.free_space {
        on_event(Event::FreeSpace(check));
    }

    let total_files: u64 = readable
        .iter()
        .map(|(_, v)| (v.files.len() + v.snapshots.len()) as u64)
        .sum();
    // Small files sit side by side (on NTFS, inside the file table itself):
    // checking them one by one is many small reads close together.
    let cached = crate::source::Cached::new(src);
    let mut seen: HashMap<(u64, u64), usize> = HashMap::new();
    let mut done = 0;
    for (index, volume) in &readable {
        for file in &volume.files {
            if cancel.load(Ordering::Relaxed) {
                result.cancelled = true;
                return result;
            }
            done += 1;
            if file.deleted || opts.existing {
                if let Some(mut item) =
                    fs_item(&cached, &formats, file, *index, result.items.len() as u32)
                {
                    // Moving a file to the Trash and emptying it leaves two
                    // deleted entries for the same data. List it once, in
                    // its original folder.
                    let key = (item.extents[0].offset, item.size);
                    match seen.get(&key) {
                        Some(&n)
                            if item.origin == Origin::Deleted
                                && result.items[n].origin == Origin::Deleted =>
                        {
                            if volume_fs::in_trash(&result.items[n].dir)
                                && !volume_fs::in_trash(&item.dir)
                            {
                                item.id = result.items[n].id;
                                result.items[n] = item;
                                on_event(Event::Updated(&result.items[n]));
                            }
                        }
                        _ => {
                            seen.insert(key, result.items.len());
                            on_event(Event::Item(&item));
                            result.items.push(item);
                        }
                    }
                }
            }
            on_event(Event::Progress {
                done,
                total: total_files,
                at: None,
            });
        }
        // Files only a snapshot still has. One kept by several snapshots
        // is listed once, from the newest (they come newest first).
        let mut listed: HashSet<(&str, &str, &[Extent])> = HashSet::new();
        for snap in &volume.snapshots {
            if cancel.load(Ordering::Relaxed) {
                result.cancelled = true;
                return result;
            }
            done += 1;
            let f = &snap.file;
            if listed.insert((&f.dir, &f.name, &f.extents)) {
                let id = result.items.len() as u32;
                if let Some(item) = snapshot_item(&cached, &formats, snap, *index, id) {
                    // Data a deleted entry already gives back is not
                    // listed twice.
                    let key = (item.extents[0].offset, item.size);
                    if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key) {
                        e.insert(result.items.len());
                        on_event(Event::Item(&item));
                        result.items.push(item);
                    }
                }
            }
            on_event(Event::Progress {
                done,
                total: total_files,
                at: None,
            });
        }
    }

    mark_reused_space(&mut result.items, &mut on_event);

    let mut unrepaired = Unrepaired::default();
    if opts.deep {
        on_event(Event::Phase(Phase::DeepScan));
        unrepaired = deep_scan(src, opts, &readable, cancel, &mut result, &mut on_event);
    }
    if !cancel.load(Ordering::Relaxed) {
        repair_videos(src, &mut result, &unrepaired.videos, cancel, &mut on_event);
    }
    if !cancel.load(Ordering::Relaxed) {
        repair_files(src, &mut result, &unrepaired.photos, cancel, &mut on_event);
    }
    if !cancel.load(Ordering::Relaxed) {
        pair_proxies(src, &mut result, &mut on_event);
    }
    result
}

/// What the scan reports about a volume, read or not.
fn volume_info(src: &dyn Source, volume: Option<&Volume>, offset: u64, len: u64) -> VolumeInfo {
    VolumeInfo {
        fs: volume.map_or_else(|| "Unknown".to_string(), |v| v.fs.name().to_string()),
        label: volume.and_then(|v| v.label.clone()),
        offset,
        len,
        readable: volume.is_some(),
        files: volume.map_or(0, |v| v.files.len()),
        deleted: volume.map_or(0, |v| v.files.iter().filter(|f| f.deleted).count()),
        free: volume.map_or(0, |v| v.free.iter().map(|e| e.len).sum()),
        signature: if volume.is_some() {
            String::new()
        } else {
            signature(src, offset)
        },
        found: String::new(),
        names: volume.map(|v| v.name_sources).unwrap_or_default(),
    }
}

/// The parts of a `len`-byte drive outside `ranges` (start and end).
fn outside(ranges: &[(u64, u64)], len: u64) -> Vec<(u64, u64)> {
    let mut ranges = ranges.to_vec();
    ranges.sort_unstable();
    let mut out = Vec::new();
    let mut pos = 0;
    for (start, end) in ranges {
        if start > pos {
            out.push((pos, start.min(len)));
        }
        pos = pos.max(end);
    }
    if pos < len {
        out.push((pos, len));
    }
    out.retain(|&(a, e)| a < e);
    out
}

/// Blocks of free space read to tell whether it has been erased.
const FREE_SAMPLES: u64 = 320;
/// Fewer blocks read than this (on a damaged drive) say nothing.
const FREE_MIN_SAMPLES: u32 = 256;
/// Bytes read per sample: one flash page, one file system block.
const FREE_SAMPLE_LEN: usize = 4096;
/// Less free space than this is too little to judge by.
const FREE_MIN_LEN: u64 = 16 << 20;
/// A volume with fewer files than this may simply never have been
/// written to (freshly formatted), which is blank without any erasing.
const FREE_MIN_FILES: usize = 8;
/// Share of blank samples above which the free space counts as erased.
/// Some free space on any well-used drive is blank (the tails of files,
/// space never written), but not this much.
const FREE_ERASED: f64 = 0.95;
/// Deleted files whose start is read as a second opinion.
const DELETED_SAMPLES: usize = 32;
/// Longest the check may take, on a slow or failing drive.
const FREE_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

fn is_blank(b: &[u8]) -> bool {
    matches!(b.first(), Some(0x00 | 0xFF)) && b.iter().all(|&x| x == b[0])
}

/// Samples the free space of the volumes whose files were read, to tell
/// whether it has been erased. Only volumes with files take part, and
/// only a large sample is judged, so a freshly formatted card (blank
/// because nothing was ever written) is not taken for an erased one.
/// The samples are spread evenly and read in order, so a hard drive's
/// head sweeps across once.
fn check_free_space(
    src: &dyn Source,
    readable: &[(u32, Volume)],
    cancel: &AtomicBool,
) -> Option<FreeSpaceCheck> {
    let used: Vec<&Volume> = readable
        .iter()
        .map(|(_, v)| v)
        .filter(|v| v.files.len() >= FREE_MIN_FILES)
        .collect();
    let mut free: Vec<Extent> = used
        .iter()
        .flat_map(|v| &v.free)
        .filter_map(|e| {
            let end = e.end().min(src.len());
            (e.offset < end).then(|| Extent::new(e.offset, end - e.offset))
        })
        .collect();
    free.sort_unstable();
    let view = MappedSource::new(src, free);
    let total = view.len();
    if total < FREE_MIN_LEN {
        return None;
    }
    let began = std::time::Instant::now();
    let over = || cancel.load(Ordering::Relaxed) || began.elapsed() > FREE_BUDGET;
    let mut buf = vec![0u8; FREE_SAMPLE_LEN];
    let mut read = |src: &dyn Source, at: u64| -> Option<bool> {
        let n = src.read_at(at, &mut buf).ok()?;
        (n == buf.len()).then(|| is_blank(&buf))
    };

    let (mut sampled, mut blank) = (0u32, 0u32);
    let stride = total / FREE_SAMPLES;
    for k in 0..FREE_SAMPLES {
        if over() {
            break;
        }
        // The middle of each stretch, on a block boundary.
        let at = (k * stride + stride / 2) / FREE_SAMPLE_LEN as u64 * FREE_SAMPLE_LEN as u64;
        if let Some(b) = read(&view, at) {
            sampled += 1;
            blank += u32::from(b);
        }
    }

    // The starts of deleted files, as a second opinion: on a drive that
    // does not erase, they still hold their data.
    let (mut deleted_sampled, mut deleted_blank) = (0u32, 0u32);
    let mut starts: Vec<u64> = used
        .iter()
        .flat_map(|v| &v.files)
        .filter(|f| f.deleted && !f.overwritten && f.size >= FREE_SAMPLE_LEN as u64)
        .filter_map(|f| f.extents.first())
        .filter(|e| e.len >= FREE_SAMPLE_LEN as u64)
        .map(|e| e.offset)
        .collect();
    starts.sort_unstable();
    starts.dedup();
    let every = starts.len().div_ceil(DELETED_SAMPLES).max(1);
    for &at in starts.iter().step_by(every) {
        if over() {
            break;
        }
        if let Some(b) = read(src, at) {
            deleted_sampled += 1;
            deleted_blank += u32::from(b);
        }
    }

    let blank_fraction = if sampled == 0 {
        0.0
    } else {
        f64::from(blank) / f64::from(sampled)
    };
    // Deleted files whose data is mostly still there outweigh blank free
    // space: whatever made it blank, deleted files can be recovered.
    let deleted_kept = deleted_sampled >= 4 && deleted_blank * 2 < deleted_sampled;
    Some(FreeSpaceCheck {
        sampled,
        blank,
        blank_fraction,
        deleted_sampled,
        deleted_blank,
        erased: sampled >= FREE_MIN_SAMPLES && blank_fraction >= FREE_ERASED && !deleted_kept,
    })
}

/// The bytes of a volume that say which file system it is, in hex.
fn signature(src: &dyn Source, offset: u64) -> String {
    let mut head = vec![0u8; 4096];
    let (n, _) = read_tolerant(src, offset, &mut head);
    let hex = |r: std::ops::Range<usize>| {
        head.get(r)
            .unwrap_or_default()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    if n == 0 {
        return "unreadable".into();
    }
    format!(
        "0:{} 32:{} 510:{} 1024:{}",
        hex(0..16),
        hex(32..40),
        hex(510..512),
        hex(1024..1032)
    )
}

/// Starts, on the source, of photos and videos the deep scan found the
/// beginning of but could not complete.
#[derive(Debug, Default)]
struct Unrepaired {
    videos: Vec<u64>,
    photos: Vec<u64>,
}

/// Video extensions whose index Reperio can rebuild.
fn is_rebuildable_video(item: &Item) -> bool {
    item.kind == Kind::Video
        && matches!(
            item.ext.as_str(),
            "mp4" | "mov" | "m4v" | "3gp" | "lrf" | "lrv"
        )
}

fn is_proxy(item: &Item) -> bool {
    matches!(item.ext.as_str(), "lrf" | "lrv")
}

/// A good video found on the card, to guide repairs.
struct Guide {
    reference: videofix::Reference,
    extents: Vec<Extent>,
    name: String,
}

/// Intact videos on the card, most likely from the camera that made the
/// damaged ones. Different settings (a 4K and a 1080p clip) need
/// different guides, so several are kept.
///
/// Low-resolution copies (`.LRF`, `.LRV`) are guides for each other only:
/// their settings are not those of the full-size videos.
fn video_guides(src: &dyn Source, items: &[Item], proxies: bool) -> Vec<Guide> {
    let mut out = Vec::new();
    for item in items
        .iter()
        .filter(|i| i.health == Health::Good && is_rebuildable_video(i) && is_proxy(i) == proxies)
    {
        if out.len() >= MAX_GUIDES {
            break;
        }
        let view = MappedSource::new(src, item.extents.clone());
        if let Some(reference) = videofix::Reference::from_video(&view) {
            out.push(Guide {
                reference,
                extents: item.extents.clone(),
                name: item.name.clone(),
            });
        }
    }
    out
}

/// Guides tried per video.
const MAX_GUIDES: usize = 6;
/// A repair must give back at least this many frames to count.
const MIN_FRAMES: usize = 15;
/// Longest a broken video found by the deep scan is assumed to run.
const MAX_VIDEO: u64 = 64 << 30;

/// The best repair of `broken`: from the settings in its own stream if the
/// camera repeats them there (the guide is then `None`), or with one of
/// the guides. More frames win, then having sound, then no guide needed.
fn best_repair<'g>(
    broken: &dyn Source,
    guides: &'g [Guide],
) -> Option<(videofix::Rebuilt, Option<&'g Guide>, Option<&'static str>)> {
    let mut options: Vec<(videofix::Rebuilt, Option<&'g Guide>, Option<&'static str>)> = Vec::new();
    if let Some(r) = videofix::Reference::from_stream(broken) {
        if let Some(fixed) = videofix::rebuild(broken, &r) {
            options.push((fixed, None, r.camera.map(|c| c.name)));
        }
    }
    for g in guides {
        if let Some(fixed) = videofix::rebuild(broken, &g.reference) {
            options.push((fixed, Some(g), g.reference.camera.map(|c| c.name)));
        }
    }
    options
        .into_iter()
        .filter(|(r, _, _)| r.frames >= MIN_FRAMES)
        .max_by_key(|(r, g, _)| (r.frames, r.sound_samples > 0, g.is_none()))
}

fn video_fix(fixed: &videofix::Rebuilt, guide: Option<&Guide>, camera: Option<&str>) -> Fix {
    Fix::Video {
        reference: guide.map(|g| g.extents.clone()).unwrap_or_default(),
        reference_name: guide.map(|g| g.name.clone()).unwrap_or_default(),
        silent: fixed.sound_samples == 0,
        camera: camera.map(str::to_string),
    }
}

/// Rebuilds the index of damaged videos, and of videos the deep scan found
/// only the start of (`unrepaired`: their header offsets on the source).
fn repair_videos(
    src: &dyn Source,
    result: &mut ScanResult,
    unrepaired: &[u64],
    cancel: &AtomicBool,
    on_event: &mut impl FnMut(Event),
) {
    let damaged: Vec<usize> = (0..result.items.len())
        .filter(|&n| {
            let i = &result.items[n];
            is_rebuildable_video(i) && matches!(i.health, Health::Damaged | Health::Overwritten)
        })
        .collect();
    if damaged.is_empty() && unrepaired.is_empty() {
        return;
    }
    let guides = video_guides(src, &result.items, false);
    let proxy_guides = video_guides(src, &result.items, true);
    on_event(Event::Phase(Phase::Repair));
    let total = (damaged.len() + unrepaired.len()) as u64;
    for (k, &n) in damaged.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        on_event(Event::Progress {
            done: k as u64,
            total,
            at: None,
        });
        let view = MappedSource::new(src, result.items[n].extents.clone());
        let own = if is_proxy(&result.items[n]) {
            &proxy_guides
        } else {
            &guides
        };
        let Some((fixed, guide, camera)) = best_repair(&view, own) else {
            continue;
        };
        let item = &mut result.items[n];
        item.fix = Some(video_fix(&fixed, guide, camera));
        item.health = Health::Repaired;
        item.size = fixed.len();
        on_event(Event::Updated(item));
    }

    // Starts of videos the deep scan could not complete. Each runs at most
    // to where the next file found begins.
    let mut starts: Vec<u64> = result
        .items
        .iter()
        .filter_map(|i| i.extents.first().map(|e| e.offset))
        .collect();
    starts.sort_unstable();
    for (k, &start) in unrepaired.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        on_event(Event::Progress {
            done: (damaged.len() + k) as u64,
            total,
            at: None,
        });
        let taken = result.items.iter().any(|i| {
            i.extents
                .iter()
                .any(|e| e.offset <= start && start < e.end())
        });
        if taken {
            continue;
        }
        let next = starts
            .iter()
            .copied()
            .find(|&s| s > start)
            .unwrap_or(src.len());
        let end = next.min(start.saturating_add(MAX_VIDEO)).min(src.len());
        let window = MappedSource::new(src, vec![Extent::new(start, end - start)]);
        let Some((fixed, guide, camera)) = best_repair(&window, &guides) else {
            continue;
        };
        let ext = guide.map_or_else(|| "mp4".to_string(), |g| guide_ext(&g.name));
        let item = Item {
            id: result.items.len() as u32,
            name: format!("Video {} (repaired).{ext}", start / 512),
            dir: String::new(),
            ext,
            kind: Kind::Video,
            origin: Origin::Carved,
            health: Health::Repaired,
            size: fixed.len(),
            taken: None,
            modified: None,
            extents: vec![Extent::new(start, fixed.data.1)],
            volume: None,
            fix: Some(video_fix(&fixed, guide, camera)),
            found_by: None,
            nsrl_status: None,
        };
        on_event(Event::Item(&item));
        result.items.push(item);
    }
}

/// Largest photo or document repaired in memory.
const MAX_FILE_REPAIR: u64 = 128 << 20;
/// Good photos kept to take headers from, and how much of each.
const MAX_PHOTO_GUIDES: usize = 12;
const GUIDE_HEAD: usize = 256 << 10;

fn is_repairable_photo(item: &Item) -> bool {
    matches!(
        item.ext.as_str(),
        "jpg" | "jpeg" | "jpe" | "jfif" | "heic" | "heif"
    )
}

fn is_repairable_document(item: &Item) -> bool {
    matches!(item.ext.as_str(), "docx" | "xlsx" | "pptx" | "pdf" | "doc")
}

/// Rebuilds the pieces damaged photos and documents can be put back
/// together from (see [`crate::photofix`] and [`crate::docfix`]), and
/// finishes photos the deep scan found the start of (`photos`: their
/// offsets on the source).
fn repair_files(
    src: &dyn Source,
    result: &mut ScanResult,
    photos: &[u64],
    cancel: &AtomicBool,
    on_event: &mut impl FnMut(Event),
) {
    let damaged: Vec<usize> = (0..result.items.len())
        .filter(|&n| {
            let i = &result.items[n];
            matches!(i.health, Health::Damaged | Health::Overwritten)
                && i.fix.is_none()
                && i.size <= MAX_FILE_REPAIR
                && (is_repairable_photo(i) || is_repairable_document(i))
        })
        .collect();
    if damaged.is_empty() && photos.is_empty() {
        return;
    }
    on_event(Event::Phase(Phase::Repair));
    let total = (damaged.len() + photos.len()) as u64;

    // Good photos: the headers lost ones can be given. One per camera and
    // picture size is enough.
    let mut guides: Vec<(Vec<Extent>, Vec<u8>)> = Vec::new();
    let mut seen = Vec::new();
    for item in result.items.iter().filter(|i| {
        i.health == Health::Good && matches!(i.ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif")
    }) {
        if guides.len() >= MAX_PHOTO_GUIDES {
            break;
        }
        let view = MappedSource::new(src, item.extents.clone());
        let mut head = vec![0u8; GUIDE_HEAD.min(view.len() as usize)];
        let n = view.read_at(0, &mut head).unwrap_or(0);
        head.truncate(n);
        let Some(key) = photofix::reference_key(&head) else {
            continue;
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        guides.push((item.extents.clone(), head));
    }
    let heads: Vec<&[u8]> = guides.iter().map(|g| g.1.as_slice()).collect();

    for (k, &n) in damaged.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        on_event(Event::Progress {
            done: k as u64,
            total,
            at: None,
        });
        let item = &result.items[n];
        let view = MappedSource::new(src, item.extents.clone());
        let Ok(data) = crate::fixfile::read_all(&view) else {
            continue;
        };
        let outcome = if is_repairable_photo(item) {
            photofix::repair_with(&data, &heads).map(|(d, how, used)| {
                let reference = used.map(|u| guides[u].0.clone()).unwrap_or_default();
                (
                    d.len() as u64,
                    Fix::Photo { how, reference },
                    how.ext(),
                    how.suffix(),
                )
            })
        } else {
            docfix::repair(&data, &item.ext).map(|f| {
                let has_text = f.text.is_some();
                (
                    f.data.len() as u64,
                    Fix::Document {
                        how: f.repair,
                        has_text,
                    },
                    f.repair.ext(),
                    f.repair.suffix(),
                )
            })
        };
        let Some((size, fix, ext, suffix)) = outcome else {
            continue;
        };
        let item = &mut result.items[n];
        apply_rescue(item, ext, suffix);
        item.fix = Some(fix);
        item.health = Health::Repaired;
        item.size = size;
        on_event(Event::Updated(item));
    }

    // Photos the deep scan found the start of and could not complete: keep
    // what there is, up to where the next file begins.
    let mut starts: Vec<u64> = result
        .items
        .iter()
        .filter_map(|i| i.extents.first().map(|e| e.offset))
        .collect();
    starts.sort_unstable();
    for (k, &start) in photos.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        on_event(Event::Progress {
            done: (damaged.len() + k) as u64,
            total,
            at: None,
        });
        let taken = result.items.iter().any(|i| {
            i.extents
                .iter()
                .any(|e| e.offset <= start && start < e.end())
        });
        if taken {
            continue;
        }
        let next = starts
            .iter()
            .copied()
            .find(|&s| s > start)
            .unwrap_or(src.len());
        let end = next
            .min(start.saturating_add(MAX_FILE_REPAIR))
            .min(src.len());
        let window = MappedSource::new(src, vec![Extent::new(start, end - start)]);
        let Ok(data) = crate::fixfile::read_all(&window) else {
            continue;
        };
        let Some((fixed, how, used)) = photofix::repair_with(&data, &heads) else {
            continue;
        };
        let reference = used.map(|u| guides[u].0.clone()).unwrap_or_default();
        let ext = how.ext().unwrap_or("jpg");
        let item = Item {
            id: result.items.len() as u32,
            name: format!(
                "Photo {} {}.{ext}",
                start / 512,
                if how.suffix().is_empty() {
                    "(repaired)"
                } else {
                    how.suffix()
                }
            ),
            dir: String::new(),
            ext: ext.to_string(),
            kind: Kind::Photo,
            origin: Origin::Carved,
            health: Health::Repaired,
            size: fixed.len() as u64,
            taken: None,
            modified: None,
            extents: vec![Extent::new(start, end - start)],
            volume: None,
            fix: Some(Fix::Photo { how, reference }),
            found_by: None,
            nsrl_status: None,
        };
        on_event(Event::Item(&item));
        result.items.push(item);
    }
}

/// Renames an item for what was rescued of it: " (preview).jpg", " (text).txt".
fn apply_rescue(item: &mut Item, ext: Option<&str>, suffix: &str) {
    let stem = Path::new(&item.name)
        .file_stem()
        .map_or(item.name.clone(), |s| s.to_string_lossy().into_owned());
    let own = Path::new(&item.name)
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    let ext = ext.map(str::to_string).or(own).unwrap_or_default();
    item.name = if suffix.is_empty() {
        format!("{stem}.{ext}")
    } else {
        format!("{stem} {suffix}.{ext}")
    };
    item.ext = ext.to_ascii_lowercase();
    if item.ext == "txt" {
        item.kind = Kind::Document;
    }
}

/// Pairs each video that couldn't be repaired with the low-resolution
/// copy its camera recorded beside it (DJI `.LRF`, GoPro `.LRV`): by name
/// when the file system has them, otherwise by when they were recorded
/// and how long they run. The copy is offered under the video's name.
fn pair_proxies(src: &dyn Source, result: &mut ScanResult, on_event: &mut impl FnMut(Event)) {
    let usable = |i: &Item| matches!(i.health, Health::Good | Health::Repaired);
    let proxies: Vec<usize> = (0..result.items.len())
        .filter(|&n| {
            let i = &result.items[n];
            matches!(i.ext.as_str(), "lrf" | "lrv") && usable(i)
        })
        .collect();
    if proxies.is_empty() {
        return;
    }
    let lost: Vec<usize> = (0..result.items.len())
        .filter(|&n| {
            let i = &result.items[n];
            i.kind == Kind::Video
                && matches!(i.ext.as_str(), "mp4" | "mov" | "m4v")
                && matches!(i.health, Health::Damaged | Health::Overwritten)
        })
        .collect();
    let stem = |i: &Item| {
        Path::new(&i.name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    };
    let secs = |t: DateTime| {
        i64::from(t.year) * 31_536_000
            + i64::from(t.month) * 2_678_400
            + i64::from(t.day) * 86_400
            + i64::from(t.hour) * 3600
            + i64::from(t.minute) * 60
            + i64::from(t.second)
    };
    let duration = |i: &Item| videofix::duration_secs(&MappedSource::new(src, i.extents.clone()));
    let mut used = Vec::new();
    for &v in &lost {
        let video = &result.items[v];
        let by_name = proxies.iter().copied().find(|&p| {
            let proxy = &result.items[p];
            !used.contains(&p)
                && proxy.dir == video.dir
                && stem(proxy) == stem(video)
                && video.origin != Origin::Carved
        });
        let partner = by_name.or_else(|| {
            let t = video.taken?;
            let d = duration(video);
            proxies.iter().copied().find(|&p| {
                let proxy = &result.items[p];
                if used.contains(&p) {
                    return false;
                }
                let Some(pt) = proxy.taken else { return false };
                let close = (secs(t) - secs(pt)).abs() <= 3;
                let same_length = match (d, duration(proxy)) {
                    (Some(a), Some(b)) => (a - b).abs() <= 1.0,
                    _ => true,
                };
                close && same_length
            })
        });
        let Some(p) = partner else { continue };
        used.push(p);
        let proxy = result.items[p].clone();
        let stem_name = Path::new(&result.items[v].name)
            .file_stem()
            .map_or(String::new(), |s| s.to_string_lossy().into_owned());
        let item = Item {
            id: result.items.len() as u32,
            name: format!("{stem_name} (low-resolution copy).mp4"),
            dir: result.items[v].dir.clone(),
            ext: "mp4".into(),
            kind: Kind::Video,
            origin: proxy.origin,
            health: proxy.health,
            size: proxy.size,
            taken: proxy.taken.or(result.items[v].taken),
            modified: proxy.modified,
            extents: proxy.extents,
            volume: proxy.volume,
            fix: proxy.fix,
            found_by: None,
            nsrl_status: None,
        };
        on_event(Event::Item(&item));
        result.items.push(item);
    }
}

fn guide_ext(name: &str) -> String {
    Path::new(name).extension().map_or_else(
        || "mp4".to_string(),
        |e| e.to_string_lossy().to_ascii_lowercase(),
    )
}

/// Files that are copies of another file in `items`: same size and same
/// content at the start and end. For each set of copies the one most worth
/// keeping (original name, then good condition) is left out of the list.
pub fn duplicates(src: &dyn Source, items: &[Item]) -> Vec<u32> {
    let mut out: Vec<u32> = duplicate_groups(src, items)
        .into_iter()
        .flat_map(|g| g.into_iter().skip(1))
        .collect();
    out.sort_unstable();
    out
}

/// Sets of files in `items` that are copies of each other: same size and
/// same content at the start and end. Each set lists the copy most worth
/// keeping (original name, then good condition) first.
pub fn duplicate_groups(src: &dyn Source, items: &[Item]) -> Vec<Vec<u32>> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    const SAMPLE: u64 = 64 << 10;

    let mut by_size: HashMap<u64, Vec<&Item>> = HashMap::new();
    for i in items.iter().filter(|i| i.size > 0 && i.fix.is_none()) {
        by_size.entry(i.size).or_default().push(i);
    }
    let mut out = Vec::new();
    for group in by_size.values().filter(|g| g.len() > 1) {
        let mut by_content: HashMap<u64, Vec<&Item>> = HashMap::new();
        for &item in group {
            let view = MappedSource::new(src, item.extents.clone());
            let mut h = DefaultHasher::new();
            for at in [0, view.len().saturating_sub(SAMPLE)] {
                let mut buf = vec![0u8; SAMPLE.min(view.len()) as usize];
                let n = view.read_at(at, &mut buf).unwrap_or(0);
                buf[..n].hash(&mut h);
            }
            by_content.entry(h.finish()).or_default().push(item);
        }
        for copies in by_content.values().filter(|c| c.len() > 1) {
            let rank = |i: &Item| {
                let origin = match i.origin {
                    Origin::Deleted => 0,
                    Origin::Existing => 1,
                    Origin::Snapshot => 2,
                    Origin::Carved => 3,
                };
                let health = if i.health == Health::Good { 0 } else { 1 };
                (origin, health, i.id)
            };
            let mut copies = copies.clone();
            copies.sort_by_key(|i| rank(i));
            out.push(copies.iter().map(|i| i.id).collect());
        }
    }
    out.sort_unstable();
    out
}

/// Writes an item's content: its data, or for a repaired file the rebuilt
/// file. `progress` gets the bytes written so far. Returns the number of
/// unreadable sectors, written as zeros.
pub fn write_item(
    src: &dyn Source,
    item: &Item,
    out: &mut impl Write,
    mut progress: impl FnMut(u64),
) -> io::Result<u64> {
    let view = MappedSource::new(src, item.extents.clone());
    match &item.fix {
        Some(Fix::Video { reference, .. }) => {
            let guide = if reference.is_empty() {
                videofix::Reference::from_stream(&view)
            } else {
                videofix::Reference::from_video(&MappedSource::new(src, reference.clone()))
            };
            let fixed = guide
                .and_then(|r| videofix::rebuild(&view, &r))
                .ok_or_else(|| io::Error::other("the video could not be repaired again"))?;
            let mut counting = Counting {
                inner: out,
                written: 0,
                progress: &mut progress,
            };
            fixed.write_to(&view, &mut counting)
        }
        Some(Fix::Photo { reference, .. }) => {
            let data = read_for_repair(&view)?;
            let head = if reference.is_empty() {
                Vec::new()
            } else {
                let r = MappedSource::new(src, reference.clone());
                let mut head = vec![0u8; GUIDE_HEAD.min(r.len() as usize)];
                let n = r.read_at(0, &mut head)?;
                head.truncate(n);
                head
            };
            let refs: Vec<&[u8]> = if head.is_empty() {
                Vec::new()
            } else {
                vec![&head]
            };
            let (fixed, _) = photofix::repair(&data, &refs)
                .ok_or_else(|| io::Error::other("the photo could not be repaired again"))?;
            out.write_all(&fixed)?;
            progress(fixed.len() as u64);
            Ok(0)
        }
        Some(Fix::Document { .. }) => {
            let data = read_for_repair(&view)?;
            let fixed = docfix::repair(&data, &item.ext_before_repair())
                .ok_or_else(|| io::Error::other("the document could not be repaired again"))?;
            out.write_all(&fixed.data)?;
            progress(fixed.data.len() as u64);
            Ok(0)
        }
        None => {
            let mut buf = vec![0u8; 1 << 20];
            let mut bad = 0;
            let mut pos = 0;
            while pos < view.len() {
                let want = buf.len().min((view.len() - pos) as usize);
                let (n, b) = read_tolerant(&view, pos, &mut buf[..want]);
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                bad += b;
                pos += n as u64;
                progress(pos);
            }
            Ok(bad)
        }
    }
}

/// The text beside a repaired document: what could be read from parts
/// that could not be repaired.
pub fn companion_text(src: &dyn Source, item: &Item) -> Option<String> {
    let Some(Fix::Document { has_text: true, .. }) = &item.fix else {
        return None;
    };
    let view = MappedSource::new(src, item.extents.clone());
    let data = read_for_repair(&view).ok()?;
    docfix::repair(&data, &item.ext_before_repair())?.text
}

fn read_for_repair(view: &MappedSource) -> io::Result<Vec<u8>> {
    if view.len() > MAX_FILE_REPAIR {
        return Err(io::Error::other("too large to repair"));
    }
    crate::fixfile::read_all(view)
}

/// A writer that reports how much has gone through it.
struct Counting<'a, W: Write, F: FnMut(u64)> {
    inner: &'a mut W,
    written: u64,
    progress: &'a mut F,
}

impl<W: Write, F: FnMut(u64)> Write for Counting<'_, W, F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        (self.progress)(self.written);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Results from files a plug-in reported (an analyzer that found deleted
/// photos in an app's cache, say): each is checked and, if it is a known kind,
/// repaired, like a file the file system listed. `first_id` is the id the first
/// gets; a file that is empty or has nothing to read is skipped.
pub fn plugin_items(src: &dyn Source, files: &[FsFile], plugin: &str, first_id: u32) -> Vec<Item> {
    let formats = formats::all();
    let mut out = Vec::new();
    for file in files {
        // Volume 0 is only a placeholder; the item says which plug-in found it instead.
        if let Some(mut item) = fs_item(src, &formats, file, 0, first_id + out.len() as u32) {
            item.volume = None;
            item.found_by = Some(plugin.to_string());
            out.push(item);
        }
    }
    out
}

fn fs_item(
    src: &dyn Source,
    formats: &[Box<dyn Format>],
    file: &FsFile,
    volume: u32,
    id: u32,
) -> Option<Item> {
    if file.size == 0 || file.extents.is_empty() || is_system_clutter(file) {
        return None;
    }
    let ext = Path::new(&file.name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut item = Item {
        id,
        name: file.name.clone(),
        dir: file.dir.clone(),
        kind: Kind::from_ext(&ext),
        ext,
        origin: if file.deleted {
            Origin::Deleted
        } else {
            Origin::Existing
        },
        health: Health::Good,
        size: file.size,
        taken: None,
        modified: file.modified,
        extents: file.extents.clone(),
        volume: Some(volume),
        fix: None,
        found_by: None,
        nsrl_status: None,
    };

    if !file.deleted {
        // Live files are intact by definition; reading all of them would
        // take as long as a deep scan, so only pick up photo dates.
        // Only JPEGs carry a camera date there; a Windows drive has tens of
        // thousands of PNG and GIF icons not worth a read each.
        if matches!(item.ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif") {
            item.taken = jpeg::date_from_head(&MappedSource::new(src, item.extents.clone()));
        }
        return Some(item);
    }

    let checked = check(src, formats, &file.extents, file.size);
    match checked {
        Check::Valid(found) => item.taken = found.taken,
        _ => {
            let alt = file.alternatives.iter().find_map(|alt| {
                match check(src, formats, alt, file.size) {
                    Check::Valid(found) => Some((alt, found)),
                    _ => None,
                }
            });
            if let Some((alt, found)) = alt {
                item.extents = alt.clone();
                item.taken = found.taken;
            } else {
                item.health = match (checked, file.overwritten) {
                    (_, true) => Health::Overwritten,
                    (Check::Invalid, _) => Health::Damaged,
                    (_, false) if is_checkable(&item.ext) => Health::Damaged,
                    _ => Health::Unknown,
                };
            }
        }
    }
    Some(item)
}

/// An item for a file kept by a snapshot, in a folder naming the
/// snapshot. The snapshot holds its data, so it is intact if it can be
/// read: its extents are inside the source and its start and end read.
fn snapshot_item(
    src: &dyn Source,
    formats: &[Box<dyn Format>],
    snap: &SnapshotFile,
    volume: u32,
    id: u32,
) -> Option<Item> {
    let file = FsFile {
        deleted: false,
        ..snap.file.clone()
    };
    let mut item = fs_item(src, formats, &file, volume, id)?;
    item.origin = Origin::Snapshot;
    let folder = format!("Snapshots/{}", snap.snapshot);
    item.dir = if file.dir.is_empty() {
        folder
    } else {
        volume_fs::join_path(&folder, &file.dir)
    };
    let view = MappedSource::new(src, item.extents.clone());
    let inside = item.extents.iter().all(|e| e.end() <= src.len());
    let reads = |at: u64| {
        let mut buf = vec![0u8; 4096.min(view.len()) as usize];
        view.read_at(at, &mut buf).is_ok_and(|n| n == buf.len())
    };
    let readable = inside && reads(0) && reads(view.len().saturating_sub(4096));
    item.health = if readable && view.len() == item.size {
        Health::Good
    } else {
        Health::Damaged
    };
    Some(item)
}

/// A damaged deleted file sharing space with another deleted file lost
/// that space to it: say "partly overwritten", which is what happened.
fn mark_reused_space(items: &mut [Item], on_event: &mut impl FnMut(Event)) {
    let mut spans: Vec<(u64, u64, usize)> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.origin == Origin::Deleted)
        .flat_map(|(n, i)| i.extents.iter().map(move |e| (e.offset, e.end(), n)))
        .collect();
    spans.sort_unstable();
    let mut shared = vec![false; items.len()];
    let mut reach: Option<(u64, usize)> = None;
    for &(start, end, n) in &spans {
        if let Some((far, owner)) = reach {
            if start < far && owner != n {
                shared[n] = true;
                shared[owner] = true;
            }
        }
        if reach.is_none_or(|(far, _)| end > far) {
            reach = Some((end, n));
        }
    }
    for (n, item) in items.iter_mut().enumerate() {
        if shared[n] && item.health == Health::Damaged {
            item.health = Health::Overwritten;
            on_event(Event::Updated(item));
        }
    }
}

/// Files macOS and Windows put on removable disks for their own use: the
/// `._name` companion of every file on a non-Mac disk, Spotlight indexes,
/// file-event logs and the like. Listing them would bury the real files
/// under copies that look like broken photos and videos.
fn is_system_clutter(file: &FsFile) -> bool {
    const SYSTEM_DIRS: &[&str] = &[
        ".Spotlight-V100",
        ".fseventsd",
        ".TemporaryItems",
        ".DocumentRevisions-V100",
        "System Volume Information",
    ];
    file.name.starts_with("._")
        || file.name == ".DS_Store"
        // HFS+ journal files and hard-link store.
        || (file.dir.is_empty() && file.name.starts_with(".journal"))
        || file.dir.split('/').any(|d| {
            SYSTEM_DIRS.contains(&d) || d.starts_with('\0') || d.starts_with(".HFS+ Private")
        })
}

enum Check {
    /// A known format whose structure is intact.
    Valid(Found),
    /// Starts like a known format but its structure is broken.
    Invalid,
    /// Not a format Reperio recognises.
    Unrecognised,
}

/// Checks that the data in `extents` is a well-formed file of `size` bytes.
fn check(src: &dyn Source, formats: &[Box<dyn Format>], extents: &[Extent], size: u64) -> Check {
    let view = MappedSource::new(src, extents.to_vec());
    let mut head = vec![0u8; HEAD_LEN];
    if view.read_at(0, &mut head).is_err() {
        return Check::Invalid;
    }
    let Some(format) = formats::identify(formats, &head) else {
        return Check::Unrecognised;
    };
    let limit = view.len().min(format.max_len());
    match format.carve(&mut Reader::new(&view, 0), 0, limit) {
        Some(found) if found.len == size => Check::Valid(found),
        Some(found) if found.len < size && plausible_trailer(&view, found.len) => {
            Check::Valid(found)
        }
        _ => Check::Invalid,
    }
}

/// Whether the bytes after a file's own end are something a camera puts
/// there: a second preview image (Multi-Picture JPEGs), a motion photo's
/// video, text metadata, or padding.
/// Anything else means the space now holds another file's data, as when a
/// deleted photo's clusters were reused by a smaller photo.
fn plausible_trailer(view: &dyn Source, at: u64) -> bool {
    // Skip padding (zeros or 0xFF, including the unused end of the last
    // cluster), then expect the end of the file or an embedded image.
    let mut buf = vec![0u8; 64 << 10];
    let mut pos = at;
    while pos < view.len() {
        let n = view.read_at(pos, &mut buf).unwrap_or(0);
        if n == 0 {
            return false;
        }
        match buf[..n].iter().position(|&b| b != 0 && b != 0xFF) {
            None => pos += n as u64,
            Some(i) => {
                let first = pos + i as u64;
                let mut w = [0u8; 16];
                // Start a little early: the first data byte may be inside
                // an FF D8 FF signature or a box size's zero bytes.
                let start = first.saturating_sub(3).max(at);
                let n = view.read_at(start, &mut w).unwrap_or(0);
                let w = &w[..n];
                let jpeg = w.windows(3).take(4).any(|s| s == [0xFF, 0xD8, 0xFF]);
                // Motion photos (Samsung, Google) append an MP4 clip.
                let mp4 = w.windows(4).take(8).any(|s| s == b"ftyp");
                // Samsung and others append metadata as text.
                let text = w
                    .iter()
                    .filter(|b| b.is_ascii_graphic() || **b == b' ')
                    .count()
                    >= 12;
                return jpeg || mp4 || text;
            }
        }
    }
    true
}

/// Extensions whose content Reperio can verify.
fn is_checkable(ext: &str) -> bool {
    matches!(
        ext,
        "jpg"
            | "jpeg"
            | "png"
            | "mov"
            | "mp4"
            | "m4v"
            | "3gp"
            | "heic"
            | "heif"
            | "avif"
            | "cr3"
            | "cr2"
            | "nef"
            | "arw"
            | "dng"
            | "orf"
            | "rw2"
            | "pef"
            | "tif"
            | "tiff"
            | "avi"
            | "webp"
            | "wav"
            | "mts"
            | "m2ts"
            | "raf"
            | "gif"
            | "bmp"
            | "psd"
            | "pdf"
            | "docx"
            | "xlsx"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "epub"
            | "zip"
            | "mp3"
            | "mkv"
            | "webm"
            | "doc"
            | "xls"
            | "ppt"
            | "msg"
            | "msi"
            | "vsd"
            | "rar"
            | "7z"
            | "pst"
            | "ost"
            | "ttf"
            | "otf"
            | "ttc"
            | "mxf"
            | "wmv"
            | "wma"
            | "asf"
            | "flac"
    )
}

fn deep_scan(
    src: &dyn Source,
    opts: &ScanOptions,
    readable: &[(u32, Volume)],
    cancel: &AtomicBool,
    result: &mut ScanResult,
    on_event: &mut impl FnMut(Event),
) -> Unrepaired {
    // Free space inside readable volumes, plus everything outside them.
    let covered: Vec<(u64, u64)> = readable
        .iter()
        .map(|(_, v)| (v.offset, v.offset.saturating_add(v.len)))
        .collect();
    let mut regions: Vec<Extent> = outside(&covered, src.len())
        .into_iter()
        .map(|(a, e)| Extent::new(a, e - a))
        .collect();
    // A damaged boot sector can claim a volume larger than the card, and
    // its free space then runs past the end; scanning there would read
    // nothing for as long as the claimed size.
    for (_, v) in readable {
        regions.extend(v.free.iter().filter_map(|e| {
            let end = e.end().min(src.len());
            (e.offset < end).then(|| Extent::new(e.offset, end - e.offset))
        }));
    }
    regions.sort_unstable();
    // Volumes found by searching can overlap (an old volume under a new
    // partition): space free in both must be scanned once, not twice.
    let mut merged: Vec<Extent> = Vec::with_capacity(regions.len());
    for r in regions {
        match merged.last_mut() {
            Some(last) if r.offset <= last.end() => {
                last.len = last.len.max(r.end() - last.offset);
            }
            _ => merged.push(r),
        }
    }
    let view = MappedSource::new(src, merged);

    let volume_of = |offset: u64| {
        readable
            .iter()
            .find(|(_, v)| (v.offset..v.offset.saturating_add(v.len)).contains(&offset))
            .map(|(i, _)| *i)
    };
    // Deleted files from the filesystem, by where their data starts.
    let mut by_start: HashMap<u64, usize> = result
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i.origin, Origin::Deleted | Origin::Snapshot))
        .filter_map(|(n, i)| Some((i.extents.first()?.offset, n)))
        .collect();

    let mut scanner = Scanner::default();
    if !opts.formats.is_empty() {
        let names: Vec<&str> = opts.formats.iter().map(String::as_str).collect();
        scanner = scanner.only(&names);
    }

    let total = view.len();
    // Continuing an interrupted scan: what it found, then on from where it
    // stopped. A card that changed since gets a scan from the start.
    if let Some(r) = opts
        .resume
        .as_ref()
        .filter(|r| r.deep_total == total && r.deep_from < total)
    {
        scanner.start = r.deep_from;
        for item in &r.carved {
            let Some(start) = item.extents.first().map(|e| e.offset) else {
                continue;
            };
            if by_start.contains_key(&start) {
                continue;
            }
            let mut item = item.clone();
            item.id = result.items.len() as u32;
            on_event(Event::Item(&item));
            by_start.insert(start, result.items.len());
            result.items.push(item);
        }
    }
    // Both scan callbacks report events.
    let on_event = std::cell::RefCell::new(on_event);
    // Ranges of `view` owned by files found so far, for fragment repair.
    let mut claimed: Vec<(u64, u64)> = Vec::new();
    // APFS keeps old copies of its directory nodes in free space; they
    // name the files deleted since.
    let containers: Vec<(u32, &volume_fs::apfs::Container)> = readable
        .iter()
        .filter_map(|(i, v)| Some((*i, v.apfs.as_deref()?)))
        .collect();
    let mut old_nodes: Vec<volume_fs::apfs::OldNodes> =
        containers.iter().map(|_| Default::default()).collect();
    let outcome = scanner.scan_with(
        &view,
        |hit| {
            claimed.push((hit.offset, hit.offset + hit.len));
            let extents = view.physical(hit.offset, hit.len);
            let found = Found {
                len: hit.len,
                ext: hit.ext,
                kind: hit.kind,
                taken: hit.taken,
            };
            record(
                result,
                &by_start,
                extents,
                &found,
                &volume_of,
                &mut **on_event.borrow_mut(),
            );
            true
        },
        |p| {
            let at = view
                .physical(p.pos.min(total.saturating_sub(1)), 1)
                .first()
                .map(|e| e.offset);
            (on_event.borrow_mut())(Event::Progress {
                done: p.pos,
                total,
                at,
            });
            !cancel.load(Ordering::Relaxed)
        },
        |pos, buf| {
            if containers.is_empty() {
                return;
            }
            let mut off = 0;
            while off + 4096 <= buf.len() && off < SCAN_CHUNK {
                let b = &buf[off..];
                // Object type B-tree (node), subtype directory tree.
                if matches!(b[24..28], [2 | 3, 0, _, _]) && b[28..32] == [0x0E, 0, 0, 0] {
                    let at = view.physical(pos + off as u64, 1).first().map(|e| e.offset);
                    let found = containers
                        .iter()
                        .position(|(_, c)| at.is_some_and(|a| c.contains(a)));
                    if let Some(n) = found {
                        let size = containers[n].1.block as usize;
                        if off + size <= buf.len() {
                            volume_fs::apfs::Container::collect(&b[..size], &mut old_nodes[n]);
                        }
                    }
                }
                off += 512;
            }
        },
    );
    result.bad_sectors += outcome.bad_sectors;
    result.slow_reads += outcome.slow_reads;

    // Photos and videos that broke off partway: find their second piece.
    claimed.sort_unstable();
    let candidates: Vec<(u64, &str)> = outcome
        .broken
        .iter()
        .copied()
        .filter(|&(start, _)| !claimed.iter().any(|&(b, e)| b <= start && start < e))
        .take(MAX_REPAIRS)
        .collect();
    if !candidates.is_empty() {
        (on_event.borrow_mut())(Event::Phase(Phase::Repair));
    }
    let repair_total = candidates.len() as u64;
    let mut unrepaired = Unrepaired::default();
    let began = std::time::Instant::now();
    for (n, &(start, format)) in candidates.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) || began.elapsed() > REPAIR_BUDGET {
            break;
        }
        (on_event.borrow_mut())(Event::Progress {
            done: n as u64,
            total: repair_total,
            at: None,
        });
        if claimed.iter().any(|&(b, e)| b <= start && start < e) {
            continue;
        }
        let physical_start = view.physical(start, 1).first().map_or(0, |e| e.offset);
        let cluster = readable
            .iter()
            .find(|(_, v)| (v.offset..v.offset.saturating_add(v.len)).contains(&physical_start))
            .map_or(4096, |(_, v)| v.cluster_size);
        let repaired = match format {
            "jpeg" => repair::repair_jpeg(&view, start, &claimed, cluster),
            _ => repair::repair_mp4(&view, start, &claimed, &outcome.indexes, cluster),
        };
        let Some((pieces, found)) = repaired else {
            if let Some(e) = view.physical(start, 1).first() {
                if format == "jpeg" {
                    unrepaired.photos.push(e.offset);
                } else {
                    unrepaired.videos.push(e.offset);
                }
            }
            continue;
        };
        let mut extents = Vec::new();
        for p in &pieces {
            for e in view.physical(p.offset, p.len) {
                crate::extent::push_merged(&mut extents, e);
            }
            claimed.push((p.offset, p.end()));
        }
        claimed.sort_unstable();
        record(
            result,
            &by_start,
            extents,
            &found,
            &volume_of,
            &mut **on_event.borrow_mut(),
        );
    }

    // Deleted APFS files, by name, from the old nodes. Where the deep scan
    // already found the same file by its content, this replaces it.
    let formats = formats::all();
    let on_event = on_event.into_inner();
    for ((index, container), old) in containers.iter().zip(&old_nodes) {
        for file in container.deleted(old) {
            let id = result.items.len() as u32;
            let Some(mut item) = fs_item(src, &formats, &file, *index, id) else {
                continue;
            };
            let start = item.extents[0].offset;
            let same = result
                .items
                .iter()
                .position(|i| i.extents.first().is_some_and(|e| e.offset == start));
            match same {
                Some(n) if result.items[n].origin == Origin::Carved => {
                    item.id = result.items[n].id;
                    if item.health != Health::Good && result.items[n].health == Health::Good {
                        item.extents = result.items[n].extents.clone();
                        item.health = Health::Good;
                    }
                    item.taken = item.taken.or(result.items[n].taken);
                    result.items[n] = item;
                    on_event(Event::Updated(&result.items[n]));
                }
                Some(_) => {}
                None => {
                    on_event(Event::Item(&item));
                    result.items.push(item);
                }
            }
        }
    }
    name_from_volumes(result, readable, on_event);
    mark_reused_space(&mut result.items, on_event);
    result.cancelled = cancel.load(Ordering::Relaxed);
    unrepaired
}

/// Extensions that name the same kind of file.
fn same_ext(ext: &str) -> String {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "jpeg" | "jpe" | "jfif" => "jpg".into(),
        "tiff" => "tif".into(),
        "htm" => "html".into(),
        _ => ext,
    }
}

/// Gives deep-scan finds the names of deleted files that the file system
/// knows only by name (see [`Volume::names`]), in the way the Recovery
/// Vault does: a find takes a name when it has exactly that name's size
/// and either starts where that file started, or is the only find of its
/// size and type and that is the only name of that size and type. Where
/// two names fit, or two finds, none is guessed.
fn name_from_volumes(
    result: &mut ScanResult,
    readable: &[(u32, Volume)],
    on_event: &mut impl FnMut(Event),
) {
    for (index, volume) in readable {
        if volume.names.is_empty() {
            continue;
        }
        let mut by_start: HashMap<(u64, u64), Vec<&KnownName>> = HashMap::new();
        let mut by_type: HashMap<(u64, String), Vec<&KnownName>> = HashMap::new();
        for n in &volume.names {
            let Some(size) = n.size.filter(|&s| s > 0) else {
                continue;
            };
            if let Some(start) = n.start {
                by_start.entry((size, start)).or_default().push(n);
            }
            let ext = Path::new(&n.name)
                .extension()
                .map(|e| same_ext(&e.to_string_lossy()))
                .unwrap_or_default();
            if !ext.is_empty() {
                by_type.entry((size, ext)).or_default().push(n);
            }
        }
        let finds: Vec<usize> = (0..result.items.len())
            .filter(|&n| {
                let i = &result.items[n];
                i.origin == Origin::Carved && i.fix.is_none() && i.volume == Some(*index)
            })
            .collect();
        let mut finds_of_type: HashMap<(u64, String), usize> = HashMap::new();
        for &n in &finds {
            let i = &result.items[n];
            *finds_of_type.entry((i.size, same_ext(&i.ext))).or_default() += 1;
        }
        // Each name is given once.
        let mut given: std::collections::HashSet<(&str, &str)> = Default::default();
        let mut named = 0;
        // Finds that start where a named file started go first.
        let passes = finds.iter().map(|&n| (true, n));
        let passes = passes.chain(finds.iter().map(|&n| (false, n)));
        for (by_position, n) in passes {
            let item = &result.items[n];
            if item.origin != Origin::Carved {
                continue;
            }
            let Some(start) = item.extents.first().map(|e| e.offset) else {
                continue;
            };
            let type_key = (item.size, same_ext(&item.ext));
            let only = |v: Option<&Vec<&KnownName>>| -> Option<KnownName> {
                let v = v?;
                let first = v.first()?;
                v.iter()
                    .all(|k| k.dir == first.dir && k.name == first.name)
                    .then(|| (*first).clone())
            };
            let pick = if by_position {
                only(by_start.get(&(item.size, start)))
            } else if finds_of_type.get(&type_key) == Some(&1) {
                only(by_type.get(&type_key))
            } else {
                None
            };
            let Some(k) = pick else {
                continue;
            };
            let Some(owned) = volume
                .names
                .iter()
                .find(|n| n.dir == k.dir && n.name == k.name)
            else {
                continue;
            };
            if !given.insert((owned.dir.as_str(), owned.name.as_str())) {
                continue;
            }
            let item = &mut result.items[n];
            item.name = k.name;
            item.dir = k.dir;
            item.origin = Origin::Deleted;
            item.modified = item.modified.or(k.modified);
            named += 1;
            on_event(Event::Updated(item));
        }
        if let Some(v) = result.volumes.get_mut(*index as usize) {
            v.names.named_finds += named;
        }
    }
}

/// Bytes the scanner reads at a time; old APFS nodes are looked for at
/// positions inside each stretch only.
const SCAN_CHUNK: usize = crate::scan::CHUNK;

/// Broken photos repair is attempted on, bounding the time spent on cards
/// full of partly overwritten files.
const MAX_REPAIRS: usize = 2000;
/// Rejoining is a bonus on top of the scan; never let it hold up results
/// for long on a slow card.
const REPAIR_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// Adds a file found by the deep scan, or improves the filesystem's entry
/// for the same file.
fn record(
    result: &mut ScanResult,
    by_start: &HashMap<u64, usize>,
    extents: Vec<Extent>,
    found: &Found,
    volume_of: &dyn Fn(u64) -> Option<u32>,
    on_event: &mut dyn FnMut(Event),
) {
    let Some(start) = extents.first().map(|e| e.offset) else {
        return;
    };
    if let Some(&n) = by_start.get(&start) {
        // Already known by name. If the filesystem's version was damaged
        // and this one is intact, keep this one's layout.
        let item = &mut result.items[n];
        if item.health != Health::Good && found.len <= item.size {
            item.extents = extents;
            item.health = Health::Good;
            item.taken = item.taken.or(found.taken);
            on_event(Event::Updated(item));
        }
        return;
    }
    let name = match found.taken {
        Some(t) => format!("{t}.{}", found.ext),
        None => format!(
            "{} {}.{}",
            found.kind.folder().trim_end_matches('s'),
            start / 512,
            found.ext
        ),
    };
    let item = Item {
        id: result.items.len() as u32,
        name,
        dir: String::new(),
        ext: found.ext.to_string(),
        kind: found.kind,
        origin: Origin::Carved,
        health: Health::Good,
        size: total_len(&extents),
        taken: found.taken,
        modified: None,
        extents,
        volume: volume_of(start),
        fix: None,
        found_by: None,
        nsrl_status: None,
    };
    on_event(Event::Item(&item));
    result.items.push(item);
}

/// How recovered files are arranged in the destination folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Files with names go back into their original folders; deep-scan
    /// finds go into a "Found by deep scan" folder, sorted by type.
    Original,
    /// Everything sorted into Photos/2004, Videos/2006 and so on.
    ByDate,
    /// Everything sorted into Photos, Videos and so on.
    ByType,
}

#[derive(Debug, Clone)]
pub struct Recovered {
    pub id: u32,
    pub path: PathBuf,
    pub bad_sectors: u64,
}

/// Copies `items` out of `src` into `dest`. Never overwrites an existing
/// file: name clashes get a " (2)" suffix. Each file's modification date is
/// set to when it was taken, so photo apps sort it correctly.
pub fn recover(
    src: &dyn Source,
    items: &[&Item],
    dest: &Path,
    layout: Layout,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64, &Item),
) -> io::Result<Vec<Recovered>> {
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut done = 0;
    let mut out = Vec::new();
    for item in items {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let dir = dest.join(relative_dir(item, layout));
        create_dir_all(&dir)?;
        let path = unique_path(&dir, &sanitize(&item.name));
        let mut file = BufWriter::new(File::create(&path)?);
        let bad = write_item(src, item, &mut file, |pos| {
            on_progress(done + pos, total, item)
        })?;
        let file = file.into_inner().map_err(|e| e.into_error())?;
        if let Some(t) = item.date().and_then(local_time) {
            let _ = file.set_modified(t);
        }
        drop(file);
        if let Some(text) = companion_text(src, item) {
            let stem = Path::new(&item.name)
                .file_stem()
                .map_or(item.name.clone(), |s| s.to_string_lossy().into_owned());
            let txt = unique_path(&dir, &sanitize(&format!("{stem} (text).txt")));
            std::fs::write(txt, text)?;
        }
        done += item.size;
        on_progress(done, total, item);
        out.push(Recovered {
            id: item.id,
            path,
            bad_sectors: bad,
        });
    }
    Ok(out)
}

/// Writes a CSV listing every recovered file and where it came from.
pub fn write_report(
    path: &Path,
    dest: &Path,
    items: &[&Item],
    recovered: &[Recovered],
) -> io::Result<()> {
    let by_id: HashMap<u32, &&Item> = items.iter().map(|i| (i.id, i)).collect();
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(
        w,
        "file,original,origin,health,size,taken,first_offset,unreadable_sectors"
    )?;
    for r in recovered {
        let Some(item) = by_id.get(&r.id) else {
            continue;
        };
        let rel = r.path.strip_prefix(dest).unwrap_or(&r.path);
        writeln!(
            w,
            "\"{}\",\"{}\",{:?},{:?},{},{},{},{}",
            rel.display(),
            if item.origin == Origin::Carved {
                String::new()
            } else {
                item.path()
            },
            item.origin,
            item.health,
            item.size,
            item.taken.map(|t| t.to_string()).unwrap_or_default(),
            item.extents.first().map_or(0, |e| e.offset),
            r.bad_sectors
        )?;
    }
    w.flush()
}

fn relative_dir(item: &Item, layout: Layout) -> PathBuf {
    let kind = item.kind.folder();
    match layout {
        // A file in the card's root has an empty folder path; sanitizing
        // that empty name gave a folder called "Unnamed".
        Layout::Original if item.origin != Origin::Carved => item
            .dir
            .split('/')
            .filter(|d| !d.is_empty())
            .map(sanitize)
            .collect(),
        Layout::Original => Path::new("Found by deep scan").join(kind),
        Layout::ByType => PathBuf::from(kind),
        Layout::ByDate => {
            let year = item
                .date()
                .map_or_else(|| "Undated".to_string(), |d| d.year.to_string());
            Path::new(kind).join(year)
        }
    }
}

/// Makes a name safe for macOS, Windows and Linux file systems.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() {
        "Unnamed".to_string()
    } else {
        s
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map_or(name.into(), |s| s.to_string_lossy());
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

/// Camera and filesystem times are local time.
fn local_time(d: DateTime) -> Option<std::time::SystemTime> {
    #[cfg(unix)]
    {
        // SAFETY: mktime reads and normalises a zeroed, filled-in tm.
        let secs = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            tm.tm_year = i32::from(d.year) - 1900;
            tm.tm_mon = i32::from(d.month) - 1;
            tm.tm_mday = i32::from(d.day);
            tm.tm_hour = i32::from(d.hour);
            tm.tm_min = i32::from(d.minute);
            tm.tm_sec = i32::from(d.second);
            tm.tm_isdst = -1;
            libc::mktime(&mut tm)
        };
        u64::try_from(secs)
            .ok()
            .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
    }
    #[cfg(not(unix))]
    {
        let _ = d;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files deleted after a Windows shadow copy was taken come back from
    /// it, once each, in a folder naming the shadow copy, byte for byte;
    /// and recover into that folder.
    #[test]
    fn lists_files_kept_by_a_shadow_copy() {
        let (_, live, expected) = crate::fs::vss::tests::shadowed();
        let result = scan(
            &live,
            "shadow",
            &ScanOptions::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        let kept: Vec<&Item> = result
            .items
            .iter()
            .filter(|i| i.origin == Origin::Snapshot)
            .collect();
        assert_eq!(kept.len(), 2, "{:#?}", result.items);
        let old = kept.iter().find(|i| i.name == "old.txt").unwrap();
        assert_eq!(old.dir, "Snapshots/Shadow copy 2024-03-09 16.00.00");
        assert_eq!(old.health, Health::Good);
        let mut out = Vec::new();
        write_item(&live, old, &mut out, |_| {}).unwrap();
        assert_eq!(out, expected[..10_000]);
        // The deep scan does not list the same data again.
        let starts: Vec<u64> = result
            .items
            .iter()
            .filter_map(|i| i.extents.first().map(|e| e.offset))
            .collect();
        let mut unique = starts.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(starts.len(), unique.len());
        // Recovered under the snapshot's folder.
        let dir = std::env::temp_dir().join(format!("reperio-snap-{}", std::process::id()));
        let chosen: Vec<&Item> = kept.clone();
        recover(
            &live,
            &chosen,
            &dir,
            Layout::Original,
            &AtomicBool::new(false),
            |_, _, _| {},
        )
        .unwrap();
        let back = std::fs::read(
            dir.join("Snapshots/Shadow copy 2024-03-09 16.00.00")
                .join("old.txt"),
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(back, expected[..10_000]);
        assert_eq!(
            serde_json::to_string(&Origin::Snapshot).unwrap(),
            "\"snapshot\""
        );
    }

    #[test]
    fn hides_system_clutter() {
        let file = |dir: &str, name: &str| FsFile {
            dir: dir.into(),
            name: name.into(),
            size: 1,
            modified: None,
            created: None,
            deleted: true,
            extents: vec![],
            overwritten: false,
            alternatives: Vec::new(),
        };
        assert!(is_system_clutter(&file("DCIM/100MEDIA", "._DJI_0001.MP4")));
        assert!(is_system_clutter(&file(
            ".Spotlight-V100/Store-V2",
            "store.db"
        )));
        assert!(!is_system_clutter(&file("DCIM/100MEDIA", "DJI_0001.MP4")));
        // Files emptied from the Trash keep their place in it; they are real.
        assert!(!is_system_clutter(&file(".Trashes/501", "DJI_0001.MP4")));
    }

    /// A scan stopped halfway and continued finds what a full scan finds.
    #[test]
    fn resumed_scan_finds_the_same_files() {
        use crate::formats::{bmff, jpeg};
        let mut card: Vec<u8> = (0..3_000_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let files = [
            jpeg::tests::sample(40_000),
            bmff::tests::sample(70_000),
            jpeg::tests::sample(30_000),
        ];
        for (k, f) in files.iter().enumerate() {
            let at = 409_600 + k * 1_000_448;
            card[at..at + f.len()].copy_from_slice(f);
        }
        let starts = |r: &ScanResult| {
            let mut v: Vec<u64> = r.items.iter().map(|i| i.extents[0].offset).collect();
            v.sort_unstable();
            v
        };
        let cancel = AtomicBool::new(false);
        let full = scan(&card, "card", &ScanOptions::default(), &cancel, |_| {});
        assert_eq!(full.items.len(), 3);

        // Stopped after the first file, as recorded from progress events.
        let deep_total = card.len() as u64;
        let resume = Resume {
            deep_from: 1_000_000,
            deep_total,
            carved: full
                .items
                .iter()
                .filter(|i| i.extents[0].offset < 1_000_000)
                .cloned()
                .collect(),
        };
        let opts = ScanOptions {
            resume: Some(resume),
            ..Default::default()
        };
        let resumed = scan(&card, "card", &opts, &cancel, |_| {});
        assert_eq!(starts(&resumed), starts(&full));

        // A different card (other length) starts over.
        let stale = Resume {
            deep_from: 2_000_000,
            deep_total: deep_total + 1,
            carved: vec![],
        };
        let opts = ScanOptions {
            resume: Some(stale),
            ..Default::default()
        };
        assert_eq!(
            starts(&scan(&card, "card", &opts, &cancel, |_| {})),
            starts(&full)
        );
    }

    /// Finds take a known deleted file's name only when exactly one fits.
    #[test]
    fn names_finds_only_when_unambiguous() {
        let known = |dir: &str, name: &str, size, start| KnownName {
            dir: dir.into(),
            name: name.into(),
            size: Some(size),
            start,
            modified: None,
        };
        let volume = Volume {
            fs: volume_fs::FsType::Ntfs,
            label: None,
            offset: 0,
            len: 1 << 30,
            cluster_size: 4096,
            files: Vec::new(),
            free: Vec::new(),
            apfs: None,
            names: vec![
                // Where it started and its size.
                known("DCIM", "IMG_1.JPG", 5000, Some(40960)),
                // The only zip of its size.
                known("Docs", "report.zip", 7000, None),
                // Two pdfs of one size: neither can be told apart.
                known("A", "one.pdf", 9000, None),
                known("B", "two.pdf", 9000, None),
                // One name, but two finds of its size and type.
                known("C", "clip.mp4", 11_000, None),
            ],
            name_sources: Default::default(),
            snapshots: Vec::new(),
        };
        let find = |id: u32, ext: &str, size: u64, at: u64| Item {
            id,
            name: format!("found {id}.{ext}"),
            dir: String::new(),
            ext: ext.into(),
            kind: Kind::from_ext(ext),
            origin: Origin::Carved,
            health: Health::Good,
            size,
            taken: None,
            modified: None,
            extents: vec![Extent::new(at, size)],
            volume: Some(0),
            fix: None,
            found_by: None,
            nsrl_status: None,
        };
        let mut result = ScanResult {
            volumes: vec![VolumeInfo {
                fs: "NTFS".into(),
                label: None,
                offset: 0,
                len: 1 << 30,
                readable: true,
                files: 0,
                deleted: 0,
                free: 0,
                signature: String::new(),
                names: Default::default(),
                found: String::new(),
            }],
            items: vec![
                find(0, "jpg", 5000, 40960),
                find(1, "zip", 7000, 81920),
                find(2, "pdf", 9000, 122_880),
                find(3, "mp4", 11_000, 163_840),
                find(4, "mp4", 11_000, 204_800),
                // Right size, wrong place: a second jpg of that size, so
                // size and type alone cannot say which file it was.
                find(5, "jpg", 5000, 245_760),
            ],
            ..Default::default()
        };
        name_from_volumes(&mut result, &[(0, volume)], &mut |_| {});
        let paths: Vec<String> = result.items.iter().map(Item::path).collect();
        assert_eq!(
            paths,
            vec![
                "DCIM/IMG_1.JPG",
                "Docs/report.zip",
                "found 2.pdf",
                "found 3.mp4",
                "found 4.mp4",
                "found 5.jpg"
            ]
        );
        assert_eq!(result.items[1].origin, Origin::Deleted);
        assert_eq!(result.volumes[0].names.named_finds, 2);
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("a/b:c?.jpg"), "a_b_c_.jpg");
        assert_eq!(sanitize("  .hidden "), "hidden");
        assert_eq!(sanitize(""), "Unnamed");
    }

    #[test]
    fn root_files_stay_in_the_root() {
        let item = |dir: &str| Item {
            id: 0,
            name: "notes.txt".into(),
            dir: dir.into(),
            ext: "txt".into(),
            kind: Kind::Other,
            origin: Origin::Deleted,
            health: Health::Good,
            size: 1,
            taken: None,
            modified: None,
            extents: Vec::new(),
            volume: None,
            fix: None,
            found_by: None,
            nsrl_status: None,
        };
        assert_eq!(relative_dir(&item(""), Layout::Original), PathBuf::new());
        assert_eq!(
            relative_dir(&item("DCIM/100CANON"), Layout::Original),
            Path::new("DCIM").join("100CANON")
        );
    }
}

#[cfg(test)]
mod trailer_tests {
    use super::plausible_trailer;

    fn after(trailer: &[u8]) -> bool {
        let mut file = vec![0xAB; 100];
        file.extend(trailer);
        plausible_trailer(&file, 100)
    }

    #[test]
    fn accepts_camera_trailers_and_rejects_reused_space() {
        assert!(after(&[]));
        assert!(after(&[0; 3000]));
        assert!(after(&[0xFF, 0xD8, 0xFF, 0xE1, 1, 2, 3]));
        assert!(after(&[
            0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2'
        ]));
        assert!(after(b"\0\0Image_UTC_Data1234567890"));
        // Slack, then another photo's compressed data.
        let mut reused = vec![0u8; 1500];
        reused.extend([
            0x8E, 0x13, 0xF2, 0x07, 0x99, 0xC1, 0x3A, 0x00, 0x55, 0xE0, 0x1F, 0x86, 0x02,
        ]);
        assert!(!after(&reused));
    }
}

#[cfg(test)]
mod free_space_tests {
    use super::*;

    /// What lies in a card's free space.
    #[derive(Clone, Copy, PartialEq)]
    enum Free {
        /// Erased: zeros (as after TRIM on an SSD) or 0xFF (erased flash).
        Erased(u8),
        /// Old data from earlier files.
        OldData,
        /// Old data in every other stretch: half erased.
        Half,
    }

    /// A 32 MB FAT16 card with 4 KB clusters holding `live` text files
    /// and `deleted` deleted ones, and `free` in its free clusters.
    fn fat_card(live: usize, deleted: usize, free: Free) -> Vec<u8> {
        const SECTORS: usize = 65536;
        const DATA: usize = (4 + 2 * 32 + 32) * 512;
        let mut card = vec![0u8; SECTORS * 512];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut junk = |b: &mut [u8]| {
            for x in b {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *x = seed as u8;
            }
        };
        match free {
            Free::Erased(byte) => card[DATA..].fill(byte),
            Free::OldData => junk(&mut card[DATA..]),
            Free::Half => {
                for (n, stretch) in card[DATA..].chunks_mut(1 << 20).enumerate() {
                    if n % 2 == 0 {
                        junk(stretch);
                    }
                }
            }
        }
        let put16 =
            |b: &mut [u8], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let b = &mut card[..512];
        b[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        b[3..11].copy_from_slice(b"MSDOS5.0");
        put16(b, 11, 512);
        b[13] = 8; // sectors per cluster
        put16(b, 14, 4); // reserved
        b[16] = 2; // FATs
        put16(b, 17, 512); // root entries
        b[21] = 0xF8;
        put16(b, 22, 32); // sectors per FAT
        b[32..36].copy_from_slice(&(SECTORS as u32).to_le_bytes());
        b[54..62].copy_from_slice(b"FAT16   ");
        b[510] = 0x55;
        b[511] = 0xAA;
        let cluster = |c: usize| DATA + (c - 2) * 4096;
        let mut fat = vec![0u16; 8192];
        fat[0] = 0xFFF8;
        fat[1] = 0xFFFF;
        let mut root = Vec::new();
        for k in 0..live + deleted {
            let gone = k >= live;
            let c = 2 + k * 3;
            let mut name = *format!("FILE{k:04}TXT")
                .as_bytes()
                .first_chunk::<11>()
                .unwrap();
            if gone {
                name[0] = 0xE5;
            } else {
                fat[c] = 0xFFFF;
            }
            let text = format!("Notes from day {k}. ");
            let data: Vec<u8> = text.bytes().cycle().take(4096).collect();
            // A deleted file's data is gone too when the card erases.
            if !(gone && matches!(free, Free::Erased(_))) {
                card[cluster(c)..cluster(c) + 4096].copy_from_slice(&data);
            }
            let mut e = [0u8; 32];
            e[..11].copy_from_slice(&name);
            e[11] = 0x20;
            put16(&mut e, 26, c as u16);
            e[28..32].copy_from_slice(&4096u32.to_le_bytes());
            root.extend(e);
        }
        card[(4 + 64) * 512..][..root.len()].copy_from_slice(&root);
        for copy in 0..2 {
            let at = 4 * 512 + copy * 32 * 512;
            for (i, v) in fat.iter().enumerate() {
                put16(&mut card, at + i * 2, *v);
            }
        }
        card
    }

    fn free_space_of(card: &Vec<u8>) -> Option<FreeSpaceCheck> {
        let mut seen = None;
        let result = scan(
            card,
            "card",
            &ScanOptions {
                deep: false,
                ..Default::default()
            },
            &AtomicBool::new(false),
            |e| {
                if let Event::FreeSpace(c) = e {
                    seen = Some(*c);
                }
            },
        );
        assert_eq!(result.free_space, seen);
        result.free_space
    }

    #[test]
    fn tells_erased_free_space_from_old_data() {
        // Erased after the files were deleted, as an SSD does.
        for byte in [0x00, 0xFF] {
            let check = free_space_of(&fat_card(20, 5, Free::Erased(byte))).unwrap();
            assert!(check.erased, "{byte:#x}: {check:?}");
            assert!(check.sampled >= FREE_MIN_SAMPLES);
            assert_eq!(check.blank, check.sampled);
            assert_eq!((check.deleted_sampled, check.deleted_blank), (5, 5));
        }

        // Old data, as on a card that never erases.
        let check = free_space_of(&fat_card(20, 5, Free::OldData)).unwrap();
        assert!(!check.erased, "{check:?}");
        assert!(check.blank_fraction < 0.05, "{check:?}");
        assert_eq!((check.deleted_sampled, check.deleted_blank), (5, 0));

        // Half erased: not nearly all, so the deep scan is still worth it.
        let check = free_space_of(&fat_card(20, 0, Free::Half)).unwrap();
        assert!(!check.erased, "{check:?}");
        assert!((0.4..0.6).contains(&check.blank_fraction), "{check:?}");

        // Freshly formatted, or with just a couple of files: blank
        // because nothing was ever written, so no verdict at all.
        assert_eq!(free_space_of(&fat_card(0, 0, Free::Erased(0))), None);
        assert_eq!(free_space_of(&fat_card(3, 0, Free::Erased(0))), None);

        // Deleted files that are still there outweigh blank free space.
        let mut card = fat_card(20, 0, Free::Erased(0));
        let kept = fat_card(20, 6, Free::OldData);
        // The kept card's directory and the deleted files' data, on an
        // otherwise blank card.
        card[(4 + 64) * 512..(4 + 64 + 32) * 512]
            .copy_from_slice(&kept[(4 + 64) * 512..(4 + 64 + 32) * 512]);
        let data = (4 + 64 + 32) * 512;
        for k in 20..26 {
            let at = data + k * 3 * 4096;
            card[at..at + 4096].copy_from_slice(&kept[at..at + 4096]);
        }
        let check = free_space_of(&card).unwrap();
        assert!(check.blank_fraction > 0.95, "{check:?}");
        assert_eq!((check.deleted_sampled, check.deleted_blank), (6, 0));
        assert!(!check.erased, "{check:?}");

        // Saved results from before the check still load.
        let old: ScanResult = serde_json::from_str(
            r#"{"source":"x","source_len":1,"volumes":[],"items":[],"bad_sectors":0,"cancelled":false}"#,
        )
        .unwrap();
        assert_eq!(old.free_space, None);
    }
}

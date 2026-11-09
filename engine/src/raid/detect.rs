//! Working out how an array was laid out.
//!
//! With md superblocks on the drives the layout is read from them. Without
//! (hardware RAID cards, enclosures, Windows and Mac striped sets), it is
//! guessed: the drives' contents say whether they are mirrors (identical),
//! RAID 10 (identical in pairs) or RAID 5 (every stripe XORs to zero), and
//! then each plausible chunk size, member order and parity layout is tried
//! and scored by how well the file system it produces holds together:
//!
//! - a partition table or file system must be found at the start;
//! - FAT: the two copies of the FAT match, and every folder's first
//!   cluster starts with "." and ".." entries pointing where they should;
//! - NTFS: MFT records carry their own record numbers, so every record
//!   read through the layout says whether it came from the right place;
//!   the backup boot sector is at the volume's end;
//! - ext2/3/4: backup superblocks name their group, and walking folders
//!   through the inode tables finds "." and ".." entries naming the right
//!   inodes;
//! - then, for the best few, the files the volume lists are read: a photo
//!   or video read through the wrong layout breaks at the first chunk
//!   boundary it crosses.
//!
//! A wrong chunk size or order puts some of these structures in the wrong
//! place, and the place they land in says so.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::{md, Layout, Level, Parity, RaidSource, MAX_MEMBERS};
use crate::extent::MappedSource;
use crate::formats::{self, Kind, HEAD_LEN};
use crate::fs::{self, partition, FsType};
use crate::reader::Reader;
use crate::source::Source;

/// A level to look for, without the details that are worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LevelChoice {
    Jbod,
    Raid0,
    Raid1,
    Raid5,
    Raid10,
}

impl LevelChoice {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().trim_start_matches("raid") {
            "jbod" | "linear" | "span" | "concat" => Some(LevelChoice::Jbod),
            "0" => Some(LevelChoice::Raid0),
            "1" => Some(LevelChoice::Raid1),
            "5" | "4" => Some(LevelChoice::Raid5),
            "10" => Some(LevelChoice::Raid10),
            _ => None,
        }
    }
}

/// What the user already knows. Everything left out is worked out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hints {
    pub level: Option<LevelChoice>,
    pub parity: Option<Parity>,
    /// Bytes.
    pub chunk: Option<u64>,
    /// The array order, as indexes into the drives, `None` for a missing
    /// member.
    pub order: Option<Vec<Option<usize>>>,
    /// How many members are missing, when no order is given.
    pub missing: usize,
    /// Where the data starts on each drive (one value: on all of them).
    pub offsets: Option<Vec<u64>>,
    /// Guess even if the drives carry md superblocks.
    pub ignore_metadata: bool,
}

/// A possible layout, with how sure we are of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub layout: Layout,
    pub description: String,
    /// From 0 to 1.
    pub confidence: f64,
    /// Read from md superblocks rather than guessed.
    pub from_metadata: bool,
    /// What the combined drive holds: "NTFS volume, 2.0 GB".
    pub found: Vec<String>,
    pub notes: Vec<String>,
    /// Size of the combined drive in bytes.
    pub size: u64,
}

/// The result of [`detect`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Detection {
    /// Most likely first.
    pub candidates: Vec<Candidate>,
    /// Things worth knowing that belong to no one candidate.
    pub notes: Vec<String>,
}

/// Chunk sizes tried when none is given: every power of two from 4 KiB
/// to 1 MiB (md's default is 512 KiB, hardware cards favour 64 KiB).
pub const CHUNKS: [u64; 9] = [
    4 << 10,
    8 << 10,
    16 << 10,
    32 << 10,
    64 << 10,
    128 << 10,
    256 << 10,
    512 << 10,
    1 << 20,
];

/// Up to this many array positions, every order is tried; beyond, orders
/// are found by swapping members while the score improves.
const FULL_SEARCH: usize = 6;

/// Works out the layout of the array made of `drives`. Stops early,
/// returning what it has, when `cancel` is set.
pub fn detect(drives: &[Arc<dyn Source>], hints: &Hints, cancel: &AtomicBool) -> Detection {
    let mut out = Detection::default();
    if drives.is_empty() || drives.len() > MAX_MEMBERS {
        out.notes
            .push(format!("Choose between 1 and {MAX_MEMBERS} drives."));
        return out;
    }
    // Detection reads the same places over and over through different
    // layouts; keep what was read.
    let budget = (256usize << 20) / drives.len();
    let cached: Vec<Arc<dyn Source>> = drives
        .iter()
        .map(|d| Arc::new(BlockCache::new(d.clone(), budget)) as Arc<dyn Source>)
        .collect();

    if !hints.ignore_metadata {
        let (arrays, problems) = md::assemble(&cached);
        out.notes.extend(problems);
        if !arrays.is_empty() {
            let used: HashSet<usize> = arrays
                .iter()
                .flat_map(|a| a.layout.order.iter().flatten().copied())
                .collect();
            if used.len() < drives.len() {
                out.notes.push(format!(
                    "{} of the drives chosen carry no part of these arrays.",
                    drives.len() - used.len()
                ));
            }
            for a in arrays {
                let Ok(src) = a.layout.build(&cached) else {
                    out.notes.push(format!(
                        "Array {} cannot be read with the drives that are here.",
                        a.uuid
                    ));
                    continue;
                };
                let (_, found) = probe_quick(&src);
                let mut description = a.layout.describe();
                description.push_str(&format!(" (md {} superblocks", a.version.name()));
                if !a.name.is_empty() {
                    description.push_str(&format!(", \"{}\"", a.name));
                }
                description.push(')');
                out.candidates.push(Candidate {
                    description,
                    confidence: if a.clean { 1.0 } else { 0.8 },
                    from_metadata: true,
                    found,
                    notes: a.notes,
                    size: src.len(),
                    layout: a.layout,
                });
            }
            // The data array is the big one; system and swap arrays on a
            // NAS are small.
            out.candidates.sort_by(|a, b| {
                b.confidence
                    .total_cmp(&a.confidence)
                    .then(b.size.cmp(&a.size))
            });
            if !out.candidates.is_empty() {
                return out;
            }
        }
    }
    guess(&cached, hints, cancel, &mut out);
    out
}

/// Evidence for a layout: structures that checked out, and ones that did
/// not.
#[derive(Debug, Clone, Copy, Default)]
struct Evidence {
    pass: f64,
    fail: f64,
}

impl Evidence {
    fn check(&mut self, ok: bool) {
        if ok {
            self.pass += 1.0;
        } else {
            self.fail += 1.0;
        }
    }

    fn add(&mut self, other: Evidence) {
        self.pass += other.pass;
        self.fail += other.fail;
    }

    /// A misplaced structure counts more than a well-placed one: in a
    /// wrong layout most of the first chunk still reads right.
    fn net(&self) -> f64 {
        self.pass - 2.0 * self.fail
    }

    fn quality(&self) -> f64 {
        if self.pass + self.fail == 0.0 {
            0.0
        } else {
            self.pass / (self.pass + self.fail)
        }
    }
}

struct Scored {
    layout: Layout,
    src: RaidSource,
    quick: Evidence,
    deep: Option<Evidence>,
}

impl Scored {
    fn total(&self) -> Evidence {
        let mut e = self.quick;
        if let Some(d) = self.deep {
            e.add(d);
        }
        e
    }
}

fn guess(drives: &[Arc<dyn Source>], hints: &Hints, cancel: &AtomicBool, out: &mut Detection) {
    let n_drives = drives.len();
    let slots = match &hints.order {
        Some(o) => o.len(),
        None => n_drives + hints.missing,
    };
    if slots > MAX_MEMBERS {
        out.notes.push("Too many drives.".into());
        return;
    }
    if let Some(order) = &hints.order {
        if order.iter().flatten().any(|&i| i >= n_drives) {
            out.notes
                .push("The order names a drive that was not given.".into());
            return;
        }
    }

    for offsets in offset_choices(drives, hints) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let missing = slots - n_drives.min(slots);
        let samples = sample(drives, &offsets);
        let levels = match hints.level {
            Some(l) => vec![l],
            None => infer_level(&samples, n_drives, missing, out),
        };
        let mut scored: Vec<Scored> = Vec::new();
        for level in levels {
            search(
                drives,
                hints,
                level,
                slots,
                &offsets,
                &samples,
                cancel,
                &mut scored,
            );
        }
        rank(scored, out);
    }
    out.candidates
        .sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    out.candidates.truncate(5);
    if out.candidates.is_empty() {
        out.notes
            .push("No layout could be put together from these drives.".into());
    }
}

/// Where the data might start on each drive: where the user said, else
/// at the start, and also at the first partition when every drive has
/// one at the same place (Windows dynamic disks and Mac RAID sets keep
/// their members in a partition).
fn offset_choices(drives: &[Arc<dyn Source>], hints: &Hints) -> Vec<Vec<u64>> {
    if let Some(o) = &hints.offsets {
        return vec![match o.len() {
            1 => vec![o[0]; drives.len()],
            _ => {
                let mut v = o.clone();
                v.resize(drives.len(), 0);
                v
            }
        }];
    }
    let mut out = vec![vec![0; drives.len()]];
    let firsts: Vec<Option<u64>> = drives
        .iter()
        .map(|d| {
            let parts = partition::partitions(d.as_ref());
            parts
                .first()
                .filter(|p| p.label != "Whole disk" && p.offset > 0)
                .map(|p| p.offset)
        })
        .collect();
    if let Some(Some(first)) = firsts.first() {
        if firsts.iter().all(|f| *f == Some(*first)) {
            out.push(vec![*first; drives.len()]);
        }
    }
    out
}

/// Blocks read from the same places on every drive, to tell mirrors and
/// parity from plain striping.
struct Samples {
    blocks: Vec<Vec<Option<Vec<u8>>>>,
}

const SAMPLE: usize = 4096;

fn sample(drives: &[Arc<dyn Source>], offsets: &[u64]) -> Samples {
    let room = drives
        .iter()
        .zip(offsets)
        .map(|(d, &o)| d.len().saturating_sub(o))
        .min()
        .unwrap_or(0);
    let count = 192u64;
    let blocks = (0..count)
        .map(|i| {
            let at = room.saturating_sub(SAMPLE as u64) / count * i / SAMPLE as u64 * SAMPLE as u64;
            drives
                .iter()
                .zip(offsets)
                .map(|(d, &o)| {
                    let mut b = vec![0u8; SAMPLE];
                    (d.read_at(o + at, &mut b).ok() == Some(SAMPLE)).then_some(b)
                })
                .collect()
        })
        .collect();
    Samples { blocks }
}

fn zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

/// For each pair of drives, how many sampled blocks with data agree and
/// how many were compared.
fn agreement(s: &Samples, a: usize, b: usize) -> (usize, usize) {
    let mut same = 0;
    let mut seen = 0;
    for row in &s.blocks {
        if let (Some(x), Some(y)) = (&row[a], &row[b]) {
            if zero(x) && zero(y) {
                continue;
            }
            seen += 1;
            same += usize::from(x == y);
        }
    }
    (same, seen)
}

fn mostly((same, seen): (usize, usize)) -> bool {
    seen >= 4 && same * 10 >= seen * 9
}

/// RAID 10's mirror pairs: for each drive, the drive it copies.
fn pairs(s: &Samples, n: usize) -> Option<Vec<usize>> {
    if n < 4 || n % 2 == 1 {
        return None;
    }
    let mut partner = vec![usize::MAX; n];
    for a in 0..n {
        for b in a + 1..n {
            if partner[a] == usize::MAX && partner[b] == usize::MAX && mostly(agreement(s, a, b)) {
                partner[a] = b;
                partner[b] = a;
            }
        }
    }
    partner.iter().all(|&p| p != usize::MAX).then_some(partner)
}

fn infer_level(s: &Samples, n: usize, missing: usize, out: &mut Detection) -> Vec<LevelChoice> {
    if missing > 0 {
        // Only RAID 5 survives a missing member and looks like striping;
        // mirrors would be recognised by their copies.
        return vec![LevelChoice::Raid5];
    }
    if n == 1 {
        return vec![LevelChoice::Raid1];
    }
    if (1..n).all(|b| mostly(agreement(s, 0, b))) {
        return vec![LevelChoice::Raid1];
    }
    if pairs(s, n).is_some() {
        return vec![LevelChoice::Raid10];
    }
    if n >= 3 {
        let mut zeroed = 0;
        let mut seen = 0;
        for row in &s.blocks {
            let present: Vec<&Vec<u8>> = row.iter().flatten().collect();
            if present.len() < n || present.iter().all(|b| zero(b)) {
                continue;
            }
            seen += 1;
            let mut x = vec![0u8; SAMPLE];
            for b in present {
                x.iter_mut().zip(b).for_each(|(a, b)| *a ^= b);
            }
            zeroed += usize::from(zero(&x));
        }
        if mostly((zeroed, seen)) {
            return vec![LevelChoice::Raid5];
        }
        if seen < 4 {
            out.notes.push(
                "The drives look mostly empty, so RAID 5 could not be told from striping.".into(),
            );
            return vec![LevelChoice::Raid0, LevelChoice::Jbod, LevelChoice::Raid5];
        }
    }
    vec![LevelChoice::Raid0, LevelChoice::Jbod]
}

/// The array position that holds the start of the array.
fn first_position(level: Level) -> usize {
    match level {
        Level::Raid5 {
            parity: Parity::RightAsymmetric | Parity::RightSymmetric | Parity::First,
        } => 1,
        _ => 0,
    }
}

/// Whether a drive's data starts with something a disk starts with.
fn starts_like_a_disk(d: &dyn Source, offset: u64) -> bool {
    let mut b = [0u8; 1024];
    if d.read_at(offset, &mut b).ok() != Some(1024) {
        return false;
    }
    if fs::detect(d, offset).is_some() {
        return true;
    }
    if &b[512..520] == b"EFI PART" {
        return true;
    }
    // An MBR with at least one sensible entry.
    b[510..512] == [0x55, 0xAA]
        && (0..4).any(|i| {
            let e = &b[446 + i * 16..462 + i * 16];
            let lba = u32::from_le_bytes(e[8..12].try_into().unwrap());
            let count = u32::from_le_bytes(e[12..16].try_into().unwrap());
            e[4] != 0 && lba > 0 && count > 0 && matches!(e[0], 0 | 0x80)
        })
}

#[allow(clippy::too_many_arguments)]
fn search(
    drives: &[Arc<dyn Source>],
    hints: &Hints,
    choice: LevelChoice,
    slots: usize,
    offsets: &[u64],
    samples: &Samples,
    cancel: &AtomicBool,
    scored: &mut Vec<Scored>,
) {
    let n = drives.len();
    let levels: Vec<Level> = match choice {
        LevelChoice::Jbod => vec![Level::Jbod],
        LevelChoice::Raid0 => vec![Level::Raid0],
        LevelChoice::Raid1 => vec![Level::Raid1],
        LevelChoice::Raid5 => match hints.parity {
            Some(parity) => vec![Level::Raid5 { parity }],
            None => Parity::ROTATING
                .into_iter()
                .map(|parity| Level::Raid5 { parity })
                .collect(),
        },
        LevelChoice::Raid10 => vec![Level::Raid10 {
            near: 2,
            far: 1,
            offset: false,
        }],
    };
    let chunks: Vec<u64> = match hints.chunk {
        Some(c) => vec![c],
        None => CHUNKS.to_vec(),
    };
    let starters: Vec<usize> = (0..n)
        .filter(|&i| starts_like_a_disk(drives[i].as_ref(), offsets[i]))
        .collect();
    let partner = if choice == LevelChoice::Raid10 {
        pairs(samples, n)
    } else {
        None
    };

    let layout_for = |level: Level, chunk: u64, order: &[Option<usize>]| Layout {
        level,
        chunk: if level.striped() { chunk } else { 0 },
        order: order.to_vec(),
        offsets: order.iter().map(|o| o.map_or(0, |i| offsets[i])).collect(),
        sizes: vec![0; order.len()],
    };
    let evaluate = |layout: Layout, scored: &mut Vec<Scored>| -> Option<f64> {
        let src = layout.build(drives).ok()?;
        let (quick, _) = probe_quick(&src);
        let net = quick.net();
        scored.push(Scored {
            layout,
            src,
            quick,
            deep: None,
        });
        Some(net)
    };

    for &level in &levels {
        if level == Level::Raid1 {
            // Mirrors need no search; any order reads the same.
            let order: Vec<Option<usize>> = match &hints.order {
                Some(o) => o.clone(),
                None => (0..n).map(Some).collect(),
            };
            evaluate(layout_for(level, 0, &order), scored);
            continue;
        }
        let first = first_position(level);
        let chunk_list: &[u64] = if level.striped() { &chunks } else { &[0] };
        for &chunk in chunk_list {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(order) = &hints.order {
                evaluate(layout_for(level, chunk, order), scored);
                continue;
            }
            let fits = |o: &[Option<usize>]| {
                // The drive that starts like a disk holds the start.
                if !starters.is_empty() && !o[first].is_some_and(|i| starters.contains(&i)) {
                    return false;
                }
                // RAID 10's neighbours are each other's copies.
                if let Some(p) = &partner {
                    for k in (0..o.len()).step_by(2) {
                        if let (Some(a), Some(b)) = (o[k], o.get(k + 1).copied().flatten()) {
                            if p[a] != b {
                                return false;
                            }
                        }
                    }
                }
                true
            };
            if slots <= FULL_SEARCH {
                for order in permutations(n, slots) {
                    if fits(&order) {
                        evaluate(layout_for(level, chunk, &order), scored);
                    }
                }
            } else {
                // Start from the given order with the starting drive in
                // place, then swap members while that helps.
                let mut order: Vec<Option<usize>> = (0..n).map(Some).collect();
                order.resize(slots, None);
                if let Some(&s) = starters.first() {
                    let at = order.iter().position(|&o| o == Some(s)).unwrap();
                    order.swap(at, first);
                }
                let mut best =
                    evaluate(layout_for(level, chunk, &order), scored).unwrap_or(f64::NEG_INFINITY);
                let mut improved = true;
                let mut rounds = 0;
                while improved && rounds < 2 * slots {
                    improved = false;
                    rounds += 1;
                    for a in 0..slots {
                        for b in a + 1..slots {
                            if cancel.load(Ordering::Relaxed) {
                                return;
                            }
                            let mut o = order.clone();
                            o.swap(a, b);
                            if !fits(&o) {
                                continue;
                            }
                            if let Some(net) = evaluate(layout_for(level, chunk, &o), scored) {
                                if net > best {
                                    best = net;
                                    order = o;
                                    improved = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Every distinct arrangement of `n` drives and `slots - n` missing
/// members.
fn permutations(n: usize, slots: usize) -> Vec<Vec<Option<usize>>> {
    let mut items: Vec<Option<usize>> = (0..n).map(Some).collect();
    items.resize(slots, None);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    permute(&mut items, 0, &mut out, &mut seen);
    out
}

fn permute(
    items: &mut Vec<Option<usize>>,
    k: usize,
    out: &mut Vec<Vec<Option<usize>>>,
    seen: &mut HashSet<Vec<Option<usize>>>,
) {
    if k == items.len() {
        if seen.insert(items.clone()) {
            out.push(items.clone());
        }
        return;
    }
    for i in k..items.len() {
        items.swap(k, i);
        permute(items, k + 1, out, seen);
        items.swap(k, i);
    }
}

/// Orders the scored layouts, checks the leaders more deeply and adds
/// the distinct best ones to `out`.
fn rank(mut scored: Vec<Scored>, out: &mut Detection) {
    if scored.is_empty() {
        return;
    }
    scored.sort_by(|a, b| b.quick.net().total_cmp(&a.quick.net()));
    // Read the files of the leaders: what the quick checks cannot see
    // (chunk sizes larger than the structures they look at, file systems
    // they do not know) shows up there.
    for s in scored.iter_mut().take(6) {
        s.deep = Some(probe_files(&s.src));
    }
    scored.truncate(6);
    scored.sort_by(|a, b| b.total().net().total_cmp(&a.total().net()));

    // Layouts that read the same (RAID 10 with mirrored drives swapped,
    // say) are one layout.
    let mut distinct: Vec<Scored> = Vec::new();
    for s in scored {
        if !distinct.iter().any(|d| same_reads(&d.src, &s.src)) {
            distinct.push(s);
        }
    }
    let best = distinct[0].total();
    for (i, s) in distinct.iter().enumerate() {
        let e = s.total();
        let confidence = if e.pass == 0.0 {
            // Nothing recognisable either way.
            0.05
        } else if i == 0 {
            let margin = match distinct.get(1) {
                Some(r) => ((e.net() - r.total().net()) / e.pass.max(1.0)).clamp(0.0, 1.0),
                None => 1.0,
            };
            e.quality() * (0.5 + 0.5 * margin)
        } else {
            e.quality() * 0.5 * (e.net() / best.net().max(1.0)).clamp(0.0, 1.0)
        };
        // Layouts that explain nothing are not worth listing beside a good one.
        if i > 0 && confidence < 0.02 {
            continue;
        }
        let (_, found) = probe_quick(&s.src);
        let mut notes = Vec::new();
        if s.layout.order.iter().any(|o| o.is_none()) {
            notes.push("A missing drive's data is rebuilt from the others.".into());
        }
        out.candidates.push(Candidate {
            description: s.layout.describe(),
            confidence: (confidence * 100.0).round() / 100.0,
            from_metadata: false,
            found,
            notes,
            size: s.src.len(),
            layout: s.layout.clone(),
        });
    }
}

/// Whether two layouts read the same at sampled places.
fn same_reads(a: &dyn Source, b: &dyn Source) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let len = a.len();
    (0..48u64).all(|i| {
        let at = len.saturating_sub(SAMPLE as u64) / 48 * i / 512 * 512;
        let mut x = vec![0u8; SAMPLE];
        let mut y = vec![0u8; SAMPLE];
        match (a.read_at(at, &mut x), b.read_at(at, &mut y)) {
            (Ok(p), Ok(q)) => p == q && x == y,
            _ => false,
        }
    })
}

/// Checks of the structures at known places in the file systems found.
fn probe_quick(src: &dyn Source) -> (Evidence, Vec<String>) {
    let mut e = Evidence::default();
    let mut found = Vec::new();
    let parts = partition::partitions(src);
    let table = parts.len() > 1 || parts.first().is_some_and(|p| p.label != "Whole disk");
    if table {
        e.pass += 1.0;
    }
    for p in parts.iter().take(4) {
        let Some(kind) = fs::detect_volume(src, p.offset, p.len) else {
            continue;
        };
        e.pass += 2.0;
        found.push(format!(
            "{} volume, {}",
            kind.name(),
            crate_size(p.len.min(src.len().saturating_sub(p.offset)))
        ));
        let checks = match kind {
            FsType::Fat12 | FsType::Fat16 | FsType::Fat32 => fat_checks(src, p.offset),
            FsType::Ntfs => ntfs_checks(src, p.offset),
            FsType::Ext2 | FsType::Ext3 | FsType::Ext4 => ext_checks(src, p.offset),
            _ => Evidence::default(),
        };
        e.add(checks);
    }
    (e, found)
}

fn crate_size(bytes: u64) -> String {
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

fn rd(src: &dyn Source, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; len];
    (src.read_at(at, &mut b).ok()? == len).then_some(b)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// FAT: the FAT copies agree, and folders start with "." and "..".
fn fat_checks(src: &dyn Source, off: u64) -> Evidence {
    let mut e = Evidence::default();
    let Some(boot) = rd(src, off, 512) else {
        return e;
    };
    let bps = u64::from(u16_at(&boot, 11));
    let spc = u64::from(boot[13]);
    if !(512..=4096).contains(&bps) || !bps.is_power_of_two() || spc == 0 {
        return e;
    }
    let reserved = u64::from(u16_at(&boot, 14));
    let nfats = u64::from(boot[16]);
    let root_entries = u64::from(u16_at(&boot, 17));
    let fatsz = match u16_at(&boot, 22) {
        0 => u64::from(u32_at(&boot, 36)),
        n => u64::from(n),
    };
    let fat_bytes = fatsz * bps;
    let fat1 = off + reserved * bps;
    let root_region = fat1 + nfats * fat_bytes;
    let data = root_region + (root_entries * 32).div_ceil(bps) * bps;
    let cluster = spc * bps;
    if fat_bytes == 0 || data >= src.len() {
        return e;
    }
    let clusters = (src.len() - data) / cluster + 2;

    if nfats >= 2 {
        let samples = 64u64;
        for i in 0..samples {
            let at = fat_bytes.saturating_sub(512) * i / samples / 512 * 512;
            let (Some(a), Some(b)) = (rd(src, fat1 + at, 512), rd(src, fat1 + fat_bytes + at, 512))
            else {
                continue;
            };
            if zero(&a) && zero(&b) {
                continue;
            }
            e.check(a == b);
        }
    }

    // Folders: (first cluster, parent's first cluster, 0 for the root).
    let mut queue: Vec<(u64, u64)> = Vec::new();
    let mut seen = HashSet::new();
    let read_cluster = |c: u64| {
        rd(
            src,
            data + (c - 2) * cluster,
            cluster.min(64 << 10) as usize,
        )
    };
    let root = if root_entries == 0 {
        let c = u64::from(u32_at(&boot, 44));
        if c < 2 || c >= clusters {
            return e;
        }
        seen.insert(c);
        read_cluster(c)
    } else {
        rd(src, root_region, (root_entries * 32).min(64 << 10) as usize)
    };
    let subfolders = |dir: &[u8], own: u64, queue: &mut Vec<(u64, u64)>| {
        for ent in dir.chunks_exact(32) {
            if ent[0] == 0 {
                break;
            }
            if ent[0] == 0xE5 || ent[0] == b'.' || ent[11] == 0x0F || ent[11] & 0x10 == 0 {
                continue;
            }
            if ent[11] & 0x08 != 0 {
                continue;
            }
            let c = (u64::from(u16_at(ent, 20)) << 16) | u64::from(u16_at(ent, 26));
            if c >= 2 && c < clusters {
                queue.push((c, own));
            }
        }
    };
    let Some(root) = root else { return e };
    subfolders(&root, 0, &mut queue);
    let mut walked = 0;
    while let Some((c, parent)) = queue.pop() {
        if walked >= 128 || !seen.insert(c) {
            continue;
        }
        walked += 1;
        let Some(dir) = read_cluster(c) else { continue };
        let entry = |i: usize| &dir[i * 32..i * 32 + 32];
        let cl = |ent: &[u8]| (u64::from(u16_at(ent, 20)) << 16) | u64::from(u16_at(ent, 26));
        let dot = &entry(0)[..11] == b".          " && cl(entry(0)) == c;
        let dotdot = &entry(1)[..11] == b"..         " && {
            let p = cl(entry(1));
            // ".." of a folder in the root says 0 (some tools write the
            // root's cluster).
            p == parent || (parent == u64::from(u32_at(&boot, 44)) && p == 0)
        };
        e.check(dot && dotdot);
        if dot && dotdot {
            subfolders(&dir[64..], c, &mut queue);
        }
    }
    e
}

/// NTFS: MFT records carry their numbers; the backup boot sector is at
/// the end; the MFT mirror matches the MFT.
fn ntfs_checks(src: &dyn Source, off: u64) -> Evidence {
    let mut e = Evidence::default();
    let Some(boot) = rd(src, off, 512) else {
        return e;
    };
    let bps = u64::from(u16_at(&boot, 11));
    let spc = match boot[13] {
        x if x > 0x80 => 1u64.checked_shl(256 - u32::from(x)).unwrap_or(0),
        x => u64::from(x),
    };
    let cluster = bps * spc;
    if !(256..=4096).contains(&bps) || cluster == 0 || cluster > 2 << 20 {
        return e;
    }
    let record = match boot[0x40] as i8 {
        x if x > 0 => u64::from(x as u8) * cluster,
        x if x > -31 => 1u64 << (-i32::from(x)),
        _ => return e,
    };
    if !(256..=65536).contains(&record) {
        return e;
    }
    let total = u64_at(&boot, 0x28);
    let Some(mft) = u64_at(&boot, 0x30)
        .checked_mul(cluster)
        .and_then(|m| m.checked_add(off))
    else {
        return e;
    };

    // How many records the MFT has, from its own $DATA attribute.
    let mut count = 1024u64;
    if let Some(r0) = rd(src, mft, record as usize) {
        if &r0[..4] == b"FILE" {
            let mut at = usize::from(u16_at(&r0, 0x14));
            for _ in 0..32 {
                if at + 0x38 > r0.len() {
                    break;
                }
                let typ = u32_at(&r0, at);
                let len = u32_at(&r0, at + 4) as usize;
                if typ == 0xFFFF_FFFF || len < 16 {
                    break;
                }
                if typ == 0x80 && r0[at + 8] == 1 {
                    count = u64_at(&r0, at + 0x30) / record;
                    break;
                }
                at += len;
            }
        }
    }
    let count = count.clamp(16, 8192);
    let batch = 64u64;
    let mut i = 0;
    while i < count {
        let k = batch.min(count - i);
        let Some(block) = rd(src, mft + i * record, (k * record) as usize) else {
            break;
        };
        for (j, r) in block.chunks_exact(record as usize).enumerate() {
            if &r[..4] == b"FILE" {
                e.check(u64::from(u32_at(r, 0x2C)) == i + j as u64);
            }
        }
        i += k;
    }

    // The backup boot sector is the volume's last sector.
    if let Some(at) = total.checked_mul(bps).and_then(|t| t.checked_add(off)) {
        if let Some(b) = rd(src, at, 512) {
            e.check(b == boot);
        }
    }
    let mirr = u64_at(&boot, 0x38)
        .checked_mul(cluster)
        .and_then(|m| m.checked_add(off));
    if let (Some(m), Some(a)) = (mirr, rd(src, mft, record as usize)) {
        if rd(src, m, record as usize).is_some_and(|b| a == b) {
            e.pass += 1.0;
        }
    }
    e
}

/// ext2/3/4: backup superblocks name their group, and folders walked
/// through the inode tables start with "." and "..".
fn ext_checks(src: &dyn Source, off: u64) -> Evidence {
    let mut e = Evidence::default();
    let Some(sb) = rd(src, off + 1024, 1024) else {
        return e;
    };
    let log = u32_at(&sb, 0x18);
    if log > 6 {
        return e;
    }
    let bs = 1024u64 << log;
    let first_data = u64::from(u32_at(&sb, 0x14));
    let bpg = u64::from(u32_at(&sb, 0x20));
    let ipg = u64::from(u32_at(&sb, 0x28));
    let inodes = u64::from(u32_at(&sb, 0x00));
    let incompat = u32_at(&sb, 0x60);
    let wide = incompat & 0x80 != 0;
    let mut blocks = u64::from(u32_at(&sb, 0x04));
    if wide {
        blocks |= u64::from(u32_at(&sb, 0x150)) << 32;
    }
    let isz = if u32_at(&sb, 0x4C) >= 1 {
        u64::from(u16_at(&sb, 0x58))
    } else {
        128
    };
    let ds = if wide {
        u64::from(u16_at(&sb, 0xFE))
    } else {
        32
    };
    if bpg == 0 || ipg == 0 || !(128..=4096).contains(&isz) || !(32..=1024).contains(&ds) {
        return e;
    }
    let groups = blocks.saturating_sub(first_data).div_ceil(bpg);
    let filetype = incompat & 0x2 != 0;

    for g in [1u64, 3, 5, 7, 9, 25, 27, 49, 81] {
        if g >= groups {
            break;
        }
        let at = off + (g * bpg + first_data) * bs + if bs == 1024 { 0 } else { 1024 };
        if let Some(b) = rd(src, at, 1024) {
            e.check(u16_at(&b, 0x38) == 0xEF53 && u64::from(u16_at(&b, 0x5A)) == g);
        }
    }

    let gdt = off + (first_data + 1) * bs;
    let inode = |k: u64| -> Option<Vec<u8>> {
        if k == 0 || k > inodes {
            return None;
        }
        let g = (k - 1) / ipg;
        if g >= groups {
            return None;
        }
        let d = rd(src, gdt + g * ds, ds as usize)?;
        let mut table = u64::from(u32_at(&d, 8));
        if ds >= 64 {
            table |= u64::from(u32_at(&d, 0x28)) << 32;
        }
        rd(
            src,
            off.checked_add(table.checked_mul(bs)?)?
                .checked_add((k - 1) % ipg * isz)?,
            isz.min(256) as usize,
        )
    };
    let first_block = |ino: &[u8]| -> Option<u64> {
        let flags = u32_at(ino, 0x20);
        if flags & 0x1000_0000 != 0 {
            return None; // inline data
        }
        if flags & 0x8_0000 == 0 {
            return Some(u64::from(u32_at(ino, 0x28)));
        }
        let mut node = ino[0x28..0x28 + 60].to_vec();
        for _ in 0..5 {
            if u16_at(&node, 0) != 0xF30A || u16_at(&node, 2) == 0 {
                return None;
            }
            let depth = u16_at(&node, 6);
            let ent = &node[12..24];
            if depth == 0 {
                return Some(u64::from(u32_at(ent, 8)) | (u64::from(u16_at(ent, 6)) << 32));
            }
            let leaf = u64::from(u32_at(ent, 4)) | (u64::from(u16_at(ent, 8)) << 32);
            node = rd(src, off.checked_add(leaf.checked_mul(bs)?)?, bs as usize)?;
        }
        None
    };

    // (inode, parent inode, whether a directory entry said it is a folder)
    let mut queue: Vec<(u64, u64, bool)> = vec![(2, 2, true)];
    let mut seen = HashSet::new();
    let mut walked = 0;
    while let Some((k, parent, said)) = queue.pop() {
        if walked >= 200 || !seen.insert(k) {
            continue;
        }
        let Some(ino) = inode(k) else { continue };
        let mode = u16_at(&ino, 0);
        if mode & 0xF000 != 0x4000 {
            if said {
                e.check(false);
            }
            continue;
        }
        walked += 1;
        let Some(block) = first_block(&ino) else {
            continue;
        };
        let Some(dir) = off
            .checked_add(block.saturating_mul(bs))
            .and_then(|at| rd(src, at, bs as usize))
        else {
            e.check(false);
            continue;
        };
        let rec = usize::from(u16_at(&dir, 4));
        let dot = u64::from(u32_at(&dir, 0)) == k && dir[6] == 1 && dir[8] == b'.';
        let dotdot = rec >= 12
            && rec + 12 <= dir.len()
            && u64::from(u32_at(&dir, rec)) == parent
            && dir[rec + 6] == 2
            && &dir[rec + 8..rec + 10] == b"..";
        e.check(dot && dotdot);
        if !(dot && dotdot) {
            continue;
        }
        // The folders listed in this first block.
        let mut at = rec + usize::from(u16_at(&dir, rec + 4));
        let mut steps = 0;
        while at + 8 <= dir.len() && steps < 4096 {
            steps += 1;
            let child = u64::from(u32_at(&dir, at));
            let len = usize::from(u16_at(&dir, at + 4));
            let name_len = usize::from(dir[at + 6]);
            if len < 8 {
                break;
            }
            let is_dir = dir[at + 7] == 2;
            if child != 0 && name_len > 0 && (!filetype || is_dir) {
                queue.push((child, k, filetype));
            }
            at += len;
        }
    }
    e
}

/// Reads files the volume lists: each photo, video or document must be
/// whole when read through the layout.
fn probe_files(src: &dyn Source) -> Evidence {
    let mut e = Evidence::default();
    let all = formats::all();
    for p in partition::partitions(src).iter().take(4) {
        let Some(vol) = fs::open(src, p.offset, p.len) else {
            continue;
        };
        let live: Vec<&fs::FsFile> = vol
            .files
            .iter()
            .filter(|f| !f.deleted && !f.overwritten && f.size >= 64)
            .collect();
        let step = (live.len() / 300).max(1);
        let mut budget: u64 = 48 << 20;
        for f in live.into_iter().step_by(step) {
            if budget == 0 {
                break;
            }
            let view = MappedSource::new(src, f.extents.clone());
            let mut head = vec![0u8; HEAD_LEN];
            let got = view.read_at(0, &mut head).unwrap_or(0);
            if got == 0 {
                // Nothing mapped (an empty or inline file): no evidence.
                continue;
            }
            // Format checks expect a full head; pad a short file with zeros.
            let ext = f.name.rsplit_once('.').map_or("", |(_, x)| x);
            match formats::identify(&all, &head) {
                Some(fmt) => {
                    let limit = view.len().min(fmt.max_len()).min(budget);
                    budget = budget.saturating_sub(limit);
                    let whole = fmt
                        .carve(&mut Reader::new(&view, 0), 0, limit)
                        .is_some_and(|found| found.len * 10 >= f.size * 9);
                    e.check(whole);
                }
                None => {
                    if matches!(Kind::from_ext(ext), Kind::Photo | Kind::Video | Kind::Raw) {
                        e.check(false);
                    }
                }
            }
        }
    }
    e
}

/// Remembers 64 KiB blocks of a drive, up to a budget.
struct BlockCache {
    src: Arc<dyn Source>,
    blocks: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
    max_blocks: usize,
}

const BLOCK: u64 = 64 << 10;

impl BlockCache {
    fn new(src: Arc<dyn Source>, budget: usize) -> Self {
        Self {
            src,
            blocks: Mutex::default(),
            max_blocks: (budget / BLOCK as usize).max(16),
        }
    }

    fn block(&self, i: u64) -> std::io::Result<Arc<Vec<u8>>> {
        if let Some(b) = self
            .blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&i)
        {
            return Ok(b.clone());
        }
        let mut b = vec![0u8; BLOCK as usize];
        let n = self.src.read_at(i * BLOCK, &mut b)?;
        b.truncate(n);
        let b = Arc::new(b);
        let mut map = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= self.max_blocks {
            map.clear();
        }
        map.insert(i, b.clone());
        Ok(b)
    }
}

impl Source for BlockCache {
    fn len(&self) -> u64 {
        self.src.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.src.len();
        if offset >= len {
            return Ok(0);
        }
        // Big reads (file contents in the deep check) go straight through.
        if buf.len() as u64 > 4 * BLOCK {
            return self.src.read_at(offset, buf);
        }
        let want = buf.len().min((len - offset) as usize);
        let mut done = 0;
        while done < want {
            let at = offset + done as u64;
            let b = self.block(at / BLOCK)?;
            let from = (at % BLOCK) as usize;
            if from >= b.len() {
                break;
            }
            let k = (b.len() - from).min(want - done);
            buf[done..done + k].copy_from_slice(&b[from..from + k]);
            done += k;
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn run(drives: Vec<Vec<u8>>, hints: &Hints) -> Detection {
        let drives: Vec<Arc<dyn Source>> = drives
            .into_iter()
            .map(|d| Arc::new(d) as Arc<dyn Source>)
            .collect();
        detect(&drives, hints, &AtomicBool::new(false))
    }

    #[test]
    fn junk_drives_do_not_panic() {
        // Noise, zeros, tiny and empty drives, in every count; whatever
        // is found, building and reading it must be safe.
        for n in 1..=5usize {
            for (k, len) in [0usize, 1, 511, 4096, 300_000].into_iter().enumerate() {
                let drives: Vec<Vec<u8>> = (0..n)
                    .map(|i| {
                        if k % 2 == 0 {
                            noise(len, (i + k + 1) as u64)
                        } else {
                            vec![0; len]
                        }
                    })
                    .collect();
                let found = run(drives, &Hints::default());
                assert!(found.candidates.len() <= 5);
            }
        }
        let found = run(vec![vec![0; 100]; MAX_MEMBERS + 1], &Hints::default());
        assert!(found.candidates.is_empty());
    }

    #[test]
    fn nonsense_hints_are_refused_not_panicked_on() {
        let drives = || vec![noise(200_000, 1), noise(200_000, 2), noise(200_000, 3)];
        for hints in [
            Hints {
                chunk: Some(0),
                ..Hints::default()
            },
            Hints {
                chunk: Some(u64::MAX),
                level: Some(LevelChoice::Raid0),
                ..Hints::default()
            },
            Hints {
                chunk: Some(1000),
                level: Some(LevelChoice::Raid5),
                ..Hints::default()
            },
            Hints {
                order: Some(vec![Some(9), None]),
                ..Hints::default()
            },
            Hints {
                order: Some(vec![None, None, None]),
                ..Hints::default()
            },
            Hints {
                offsets: Some(vec![u64::MAX, 7]),
                level: Some(LevelChoice::Raid0),
                chunk: Some(4096),
                ..Hints::default()
            },
            Hints {
                missing: 100,
                ..Hints::default()
            },
        ] {
            let found = run(drives(), &hints);
            for c in &found.candidates {
                let d: Vec<Arc<dyn Source>> = drives()
                    .into_iter()
                    .map(|d| Arc::new(d) as Arc<dyn Source>)
                    .collect();
                if let Ok(src) = c.layout.build(&d) {
                    let mut b = [0u8; 4096];
                    let _ = src.read_at(0, &mut b);
                    let _ = src.read_at(src.len().saturating_sub(10), &mut b);
                }
            }
        }
    }
}

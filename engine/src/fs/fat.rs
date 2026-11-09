//! FAT12, FAT16 and FAT32, used by most memory cards up to 32 GB.
//!
//! Deleting a file on FAT marks its directory entry with 0xE5 and zeroes its
//! cluster chain, but leaves the name (apart from its first letter), size,
//! timestamps and first cluster in place. The long file name entries keep
//! the full name.

use std::collections::HashSet;

use super::{
    cluster_extents, deleted_clusters, dos_datetime, free_extents, join_path, DeletedAlloc, FsFile,
    FsType, Volume,
};
use crate::extent::Extent;
use crate::reader::Reader;
use crate::source::Source;

const ATTR_LFN: u8 = 0x0F;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const DELETED: u8 = 0xE5;
const MAX_DEPTH: usize = 32;
/// Clusters read for a deleted directory, whose chain is gone.
const MAX_DELETED_DIR_CLUSTERS: u32 = 64;
/// A FAT directory holds at most 65536 entries of 32 bytes. A corrupt
/// entry pointing a directory at a long file's chain must not read the
/// whole file into memory.
const MAX_DIR_LEN: u64 = 65536 * 32;

struct Bpb {
    bytes_per_sector: u64,
    sectors_per_cluster: u64,
    reserved: u64,
    fats: u64,
    root_entries: u64,
    total_sectors: u64,
    fat_sectors: u64,
    root_cluster: u32,
    /// FAT32 keeps its FAT size in the 32-bit field and has no fixed root
    /// directory. Drivers go by this layout, and so do we, because small
    /// FAT32 volumes below the official minimum cluster count exist.
    fat32_layout: bool,
}

impl Bpb {
    fn parse(b: &[u8]) -> Option<Self> {
        let u16_at = |o: usize| u64::from(u16::from_le_bytes([b[o], b[o + 1]]));
        let u32_at = |o: usize| u64::from(u32::from_le_bytes(b[o..o + 4].try_into().unwrap()));
        let bpb = Bpb {
            bytes_per_sector: u16_at(11),
            sectors_per_cluster: u64::from(b[13]),
            reserved: u16_at(14),
            fats: u64::from(b[16]),
            root_entries: u16_at(17),
            total_sectors: if u16_at(19) != 0 {
                u16_at(19)
            } else {
                u32_at(32)
            },
            fat_sectors: if u16_at(22) != 0 {
                u16_at(22)
            } else {
                u32_at(36)
            },
            root_cluster: u32_at(44) as u32,
            fat32_layout: u16_at(22) == 0 && u16_at(17) == 0,
        };
        let valid = b[510..512] == [0x55, 0xAA]
            && matches!(b[0], 0xEB | 0xE9)
            && matches!(bpb.bytes_per_sector, 512 | 1024 | 2048 | 4096)
            && bpb.sectors_per_cluster.is_power_of_two()
            && bpb.reserved > 0
            && matches!(bpb.fats, 1 | 2)
            && bpb.fat_sectors > 0
            && bpb.total_sectors > 0;
        valid.then_some(bpb)
    }

    fn root_dir_sectors(&self) -> u64 {
        (self.root_entries * 32).div_ceil(self.bytes_per_sector)
    }

    fn first_data_sector(&self) -> u64 {
        self.reserved + self.fats * self.fat_sectors + self.root_dir_sectors()
    }

    fn clusters(&self) -> u64 {
        self.total_sectors.saturating_sub(self.first_data_sector()) / self.sectors_per_cluster
    }

    /// The FAT type is defined by the cluster count alone.
    fn kind(&self) -> FsType {
        if self.fat32_layout {
            return FsType::Fat32;
        }
        match self.clusters() {
            0..4085 => FsType::Fat12,
            4085..65525 => FsType::Fat16,
            _ => FsType::Fat32,
        }
    }
}

pub fn detect(boot: &[u8]) -> Option<FsType> {
    let bpb = Bpb::parse(boot)?;
    (bpb.clusters() > 0).then(|| bpb.kind())
}

struct Fat<'a> {
    src: &'a dyn Source,
    kind: FsType,
    /// Next-cluster value for every cluster.
    table: Vec<u32>,
    cluster_size: u64,
    data_start: u64,
    clusters: u32,
}

impl Fat<'_> {
    fn max_cluster(&self) -> u32 {
        self.clusters + 1
    }

    fn offset_of(&self, c: u32) -> u64 {
        self.data_start + u64::from(c - 2) * self.cluster_size
    }

    fn in_use(&self, c: u32) -> bool {
        self.table.get(c as usize).is_some_and(|&v| v != 0)
    }

    fn is_end(&self, v: u32) -> bool {
        match self.kind {
            FsType::Fat12 => v >= 0xFF8,
            FsType::Fat16 => v >= 0xFFF8,
            _ => v >= 0x0FFF_FFF8,
        }
    }

    /// Follows a live cluster chain.
    fn chain(&self, first: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut c = first;
        while (2..=self.max_cluster()).contains(&c) && seen.insert(c) {
            out.push(c);
            let next = self.table[c as usize];
            if self.is_end(next) || next == 0 {
                break;
            }
            c = next;
        }
        out
    }

    fn read_clusters(&self, clusters: &[u32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(clusters.len() * self.cluster_size as usize);
        let mut r = Reader::new(self.src, 0);
        for &c in clusters {
            r.seek(self.offset_of(c));
            match r.bytes(self.cluster_size as usize) {
                Some(b) => out.extend(b),
                None => break,
            }
        }
        out
    }

    /// Reads a deleted directory: its chain is gone, so take free clusters
    /// from its first cluster for as long as they still hold entries.
    fn read_deleted_dir(&self, first: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, c) in (first..first.saturating_add(MAX_DELETED_DIR_CLUSTERS)).enumerate() {
            if c > self.max_cluster() || (i > 0 && self.in_use(c)) {
                break;
            }
            let data = self.read_clusters(&[c]);
            let looks_like_dir = if i == 0 {
                data.starts_with(b".          ")
            } else {
                data.chunks(32).take(4).all(plausible_entry)
            };
            if !looks_like_dir {
                break;
            }
            let ended = data.chunks(32).any(|e| e[0] == 0);
            out.extend(data);
            if ended {
                break;
            }
        }
        out
    }
}

fn plausible_entry(e: &[u8]) -> bool {
    e[0] == 0 || e[0] == DELETED || e[11] == ATTR_LFN || (e[0] >= 0x20 && e[11] & 0xC0 == 0)
}

/// Where FAT32 keeps a copy of its boot sector, in sectors from the start.
/// Formatters all use 6; the boot sector's own field says so, but that
/// field is what is damaged when the copy is needed.
const BACKUP_SECTOR: u64 = 6;

/// The copy of a FAT32 volume's boot sector, for when the first one is
/// damaged. Checked against the volume's first FAT, whose first entry
/// repeats the media byte, so that a boot sector that merely happens to
/// sit 6 sectors after `offset` (the volume really starts elsewhere) is
/// not taken for this volume's copy.
pub(crate) fn backup_boot(src: &dyn Source, offset: u64) -> Option<Vec<u8>> {
    for bps in [512u64, 4096] {
        let mut r = Reader::new(src, offset.checked_add(BACKUP_SECTOR * bps)?);
        let Some(b) = r.bytes(512) else { continue };
        let Some(bpb) = Bpb::parse(&b) else { continue };
        let backup = u16::from_le_bytes([b[50], b[51]]);
        if bpb.bytes_per_sector != bps || !bpb.fat32_layout || u64::from(backup) != BACKUP_SECTOR {
            continue;
        }
        r.seek(offset.checked_add(bpb.reserved.checked_mul(bps)?)?);
        if r.bytes(3).is_some_and(|f| f == [b[21], 0xFF, 0xFF]) {
            return Some(b);
        }
    }
    None
}

pub fn read(src: &dyn Source, offset: u64, len: u64, kind: FsType) -> Option<Volume> {
    let mut r = Reader::new(src, offset);
    let bpb = Bpb::parse(&r.bytes(512)?).or_else(|| Bpb::parse(&backup_boot(src, offset)?))?;
    let bps = bpb.bytes_per_sector;
    let clusters = u32::try_from(bpb.clusters()).ok()?;

    // Load the first FAT.
    r.seek(offset + bpb.reserved * bps);
    let raw = r.bytes((bpb.fat_sectors * bps) as usize)?;
    let capacity = match kind {
        FsType::Fat12 => raw.len() * 2 / 3,
        FsType::Fat16 => raw.len() / 2,
        _ => raw.len() / 4,
    };
    let entries = (clusters as usize + 2).min(capacity);
    let clusters = u32::try_from(entries.checked_sub(2)?).ok()?;
    let table: Vec<u32> = (0..entries)
        .map(|n| match kind {
            FsType::Fat12 => {
                let o = n + n / 2;
                let v = u16::from_le_bytes([raw[o], *raw.get(o + 1).unwrap_or(&0)]);
                u32::from(if n % 2 == 1 { v >> 4 } else { v & 0x0FFF })
            }
            FsType::Fat16 => u32::from(u16::from_le_bytes([raw[n * 2], raw[n * 2 + 1]])),
            _ => u32::from_le_bytes(raw[n * 4..n * 4 + 4].try_into().unwrap()) & 0x0FFF_FFFF,
        })
        .collect();

    let fat = Fat {
        src,
        kind,
        table,
        cluster_size: bpb.sectors_per_cluster * bps,
        data_start: offset + bpb.first_data_sector() * bps,
        clusters,
    };

    let root = if kind == FsType::Fat32 {
        let mut chain = fat.chain(bpb.root_cluster);
        chain.truncate(MAX_DIR_LEN.div_ceil(fat.cluster_size) as usize);
        fat.read_clusters(&chain)
    } else {
        r.seek(offset + (bpb.reserved + bpb.fats * bpb.fat_sectors) * bps);
        r.bytes((bpb.root_dir_sectors() * bps) as usize)?
    };

    let mut walk = Walk {
        fat: &fat,
        files: Vec::new(),
        allocs: Vec::new(),
        label: None,
        visited: HashSet::new(),
    };
    walk.dir(&root, "", false, 0);
    let Walk {
        mut files,
        allocs,
        label,
        ..
    } = walk;
    super::add_alternatives(
        &mut files,
        &allocs,
        fat.max_cluster(),
        |c| fat.in_use(c),
        fat.cluster_size,
        |c| fat.offset_of(c),
    );

    let free = free_extents(
        clusters,
        |c| fat.in_use(c),
        fat.cluster_size,
        |c| fat.offset_of(c),
    );
    Some(Volume {
        fs: kind,
        label,
        offset,
        len,
        cluster_size: fat.cluster_size,
        files,
        free,
        apfs: None,
        names: Vec::new(),
        name_sources: Default::default(),
        snapshots: Vec::new(),
    })
}

struct Walk<'f, 'a> {
    fat: &'f Fat<'a>,
    files: Vec<FsFile>,
    allocs: Vec<DeletedAlloc>,
    label: Option<String>,
    visited: HashSet<u32>,
}

/// A directory entry with its name resolved.
struct Entry<'d> {
    raw: &'d [u8],
    name: String,
    deleted: bool,
    /// The name is a deleted 8.3 name whose first letter is unknown.
    lost_initial: bool,
}

impl Walk<'_, '_> {
    fn dir(&mut self, data: &[u8], path: &str, parent_deleted: bool, depth: usize) {
        let mut entries = Vec::new();
        let mut lfn: Vec<&[u8]> = Vec::new();
        for e in data.chunks_exact(32) {
            if e[0] == 0 {
                break;
            }
            let attr = e[11];
            if attr == ATTR_LFN {
                lfn.push(e);
                continue;
            }
            let deleted = e[0] == DELETED || parent_deleted;
            let long = std::mem::take(&mut lfn);
            if attr & ATTR_VOLUME != 0 {
                if !deleted && self.label.is_none() {
                    self.label = Some(String::from_utf8_lossy(&e[..11]).trim().to_string());
                }
                continue;
            }
            if e[0] == b'.' {
                continue;
            }
            let (name, lost_initial) = match long_name(&long, e) {
                Some(n) => (n, false),
                None => (short_name(e), e[0] == DELETED),
            };
            entries.push(Entry {
                raw: e,
                name,
                deleted,
                lost_initial,
            });
        }

        let names: Vec<String> = entries
            .iter()
            .filter(|e| !e.lost_initial)
            .map(|e| e.name.clone())
            .collect();
        for e in &mut entries {
            if e.lost_initial {
                if let Some(c) = guess_initial(&e.name, e.raw[11] & ATTR_DIR != 0, &names) {
                    e.name.replace_range(..1, c.encode_utf8(&mut [0; 4]));
                }
            }
        }

        for e in entries {
            self.entry(e, path, depth);
        }
    }

    fn entry(&mut self, entry: Entry, path: &str, depth: usize) {
        let Entry {
            raw: e,
            name,
            deleted,
            ..
        } = entry;
        let fat = self.fat;
        let u16_at = |o: usize| u16::from_le_bytes([e[o], e[o + 1]]);
        let hi = if fat.kind == FsType::Fat32 {
            u32::from(u16_at(20)) << 16
        } else {
            0
        };
        let first = hi | u32::from(u16_at(26));
        let size = u64::from(u32::from_le_bytes(e[28..32].try_into().unwrap()));

        if e[11] & ATTR_DIR != 0 {
            if depth >= MAX_DEPTH || first < 2 || !self.visited.insert(first) {
                return;
            }
            let sub = if deleted {
                fat.read_deleted_dir(first)
            } else {
                let mut chain = fat.chain(first);
                chain.truncate(MAX_DIR_LEN.div_ceil(fat.cluster_size) as usize);
                fat.read_clusters(&chain)
            };
            self.dir(&sub, &join_path(path, &name), deleted, depth + 1);
            return;
        }

        let (clusters, overwritten) = if deleted {
            let needed = size.div_ceil(fat.cluster_size);
            let (clusters, overwritten) =
                deleted_clusters(first, needed, fat.max_cluster(), |c| fat.in_use(c));
            self.allocs.push(DeletedAlloc {
                file: self.files.len(),
                first,
                needed,
                clusters: clusters.clone(),
                certain: false,
            });
            (clusters, overwritten)
        } else {
            (fat.chain(first), false)
        };
        self.files.push(FsFile {
            dir: path.to_string(),
            name,
            size,
            modified: dos_datetime(u16_at(24), u16_at(22)),
            created: dos_datetime(u16_at(16), u16_at(14)),
            deleted,
            extents: cluster_extents(clusters, size, fat.cluster_size, |c| fat.offset_of(c)),
            overwritten,
            alternatives: if deleted && fat.kind == FsType::Fat32 && hi == 0 {
                fat.high_word_alternatives(first, size)
            } else {
                Vec::new()
            },
        });
    }
}

/// Most alternative starting clusters tried for one file.
const MAX_HIGH_WORD_GUESSES: u32 = 64;

impl Fat<'_> {
    /// Some systems (Windows among them) clear the upper 16 bits of a
    /// FAT32 file's first cluster when deleting it, so a file stored beyond
    /// cluster 65535 appears to start somewhere it does not. Each place the
    /// upper bits could have pointed is a candidate layout; validation keeps
    /// the one that holds the file.
    fn high_word_alternatives(&self, low: u32, size: u64) -> Vec<Vec<Extent>> {
        let needed = size.div_ceil(self.cluster_size);
        (1..=MAX_HIGH_WORD_GUESSES)
            .map(|k| low + (k << 16))
            .take_while(|&c| c <= self.max_cluster())
            .filter(|&c| !self.in_use(c))
            .map(|c| {
                let (clusters, _) =
                    deleted_clusters(c, needed, self.max_cluster(), |x| self.in_use(x));
                cluster_extents(clusters, size, self.cluster_size, |x| self.offset_of(x))
            })
            .collect()
    }
}

/// Guesses the first letter of a deleted 8.3 name such as `_MG_0001.JPG`.
///
/// Cameras name files in series, so a sibling with the same shape (same
/// length, same next three characters) gives the answer: `IMG_0004.JPG`
/// still on the card means `_MG_0001.JPG` was `IMG_0001.JPG`. Failing that,
/// the camera-standard (DCF) prefixes are recognised.
fn guess_initial(name: &str, is_dir: bool, siblings: &[String]) -> Option<char> {
    let rest = name.get(1..)?;
    let key = rest.get(..3.min(rest.len()))?;
    if let Some(s) = siblings
        .iter()
        .find(|s| s.len() == name.len() && s.get(1..1 + key.len()) == Some(key))
    {
        return s.chars().next();
    }
    if is_dir {
        // DCF folders are numbered from 100: 100CANON, 101_PANA.
        let b = rest.as_bytes();
        return (b.len() >= 2 && b[0].is_ascii_digit() && b[1].is_ascii_digit()).then_some('1');
    }
    const PREFIXES: &[(&str, char)] = &[
        ("MG_", 'I'),
        ("VI_", 'M'),
        ("SC_", 'D'),
        ("SCF", 'D'),
        ("SCN", 'D'),
        ("SC0", 'D'),
        ("ID_", 'V'),
        ("OPR", 'G'),
        ("JI_", 'D'),
        ("ICT", 'P'),
        ("IMG", 'C'),
        ("OV_", 'M'),
        ("XT_", 'T'),
        ("AM_", 'S'),
    ];
    PREFIXES
        .iter()
        .find(|(p, _)| rest.starts_with(p))
        .map(|&(_, c)| c)
}

/// Assembles a long file name from its entries, which precede the short
/// entry in reverse order. Live names are checked against the short name's
/// checksum; deleted names cannot be (the first byte is gone), so they are
/// accepted when every part is marked deleted too.
fn long_name(parts: &[&[u8]], short: &[u8]) -> Option<String> {
    if parts.is_empty() {
        return None;
    }
    let deleted = short[0] == DELETED;
    if deleted {
        if !parts.iter().all(|p| p[0] == DELETED) {
            return None;
        }
    } else {
        let sum = short[..11]
            .iter()
            .fold(0u8, |s, &b| s.rotate_right(1).wrapping_add(b));
        if !parts.iter().all(|p| p[13] == sum) {
            return None;
        }
    }
    let mut units = Vec::new();
    for p in parts.iter().rev() {
        for range in [1..11, 14..26, 28..32] {
            units.extend(p[range].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
        }
    }
    let end = units
        .iter()
        .position(|&u| u == 0 || u == 0xFFFF)
        .unwrap_or(units.len());
    let name = String::from_utf16_lossy(&units[..end]);
    (!name.is_empty()).then_some(name)
}

/// Formats an 8.3 name. A deleted entry has lost its first letter.
fn short_name(e: &[u8]) -> String {
    let lower_base = e[12] & 0x08 != 0;
    let lower_ext = e[12] & 0x10 != 0;
    let mut base: Vec<u8> = e[..8].to_vec();
    match base[0] {
        DELETED => base[0] = b'_',
        0x05 => base[0] = DELETED,
        _ => {}
    }
    let decode = |b: &[u8], lower: bool| {
        let s: String = b
            .iter()
            .map(|&c| c as char)
            .collect::<String>()
            .trim_end()
            .to_string();
        if lower {
            s.to_lowercase()
        } else {
            s
        }
    };
    let base = decode(&base, lower_base);
    let ext = decode(&e[8..11], lower_ext);
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

//! exFAT, the filesystem on SDXC cards (64 GB and up) and most cameras
//! made since about 2010.
//!
//! Deleting a file clears the "in use" bit of its directory entry set and
//! its bits in the allocation bitmap. The name, size, timestamps and first
//! cluster all survive, as does the flag saying whether the file was stored
//! contiguously, which for camera files it nearly always is.

use std::collections::HashSet;

use super::{
    cluster_extents, deleted_clusters, dos_datetime, free_extents, join_path, DeletedAlloc, FsFile,
    FsType, Volume,
};
use crate::reader::Reader;
use crate::source::Source;

const FILE: u8 = 0x05;
const STREAM: u8 = 0x40;
const NAME: u8 = 0x41;
const BITMAP: u8 = 0x81;
const LABEL: u8 = 0x83;
const IN_USE: u8 = 0x80;
const ATTR_DIR: u16 = 0x10;
const NO_FAT_CHAIN: u8 = 0x02;
const MAX_DEPTH: usize = 32;
/// Most bytes read for one directory. The format allows 256 MiB, but a
/// camera folder of 9999 files is about 1 MiB; a corrupt entry claiming a
/// directory as big as a video must not pull the video into memory.
const MAX_DIR_LEN: u64 = 32 << 20;

struct ExFat<'a> {
    src: &'a dyn Source,
    table: Vec<u32>,
    bitmap: Vec<u8>,
    cluster_size: u64,
    heap: u64,
    clusters: u32,
}

impl ExFat<'_> {
    fn max_cluster(&self) -> u32 {
        self.clusters + 1
    }

    fn offset_of(&self, c: u32) -> u64 {
        self.heap + u64::from(c - 2) * self.cluster_size
    }

    fn in_use(&self, c: u32) -> bool {
        let i = c.wrapping_sub(2) as usize;
        self.bitmap
            .get(i / 8)
            .is_some_and(|b| b & (1 << (i % 8)) != 0)
    }

    fn chain(&self, first: u32, limit: u64) -> Vec<u32> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut c = first;
        while (2..=self.max_cluster()).contains(&c) && (out.len() as u64) < limit && seen.insert(c)
        {
            out.push(c);
            c = self.table.get(c as usize).copied().unwrap_or(0);
        }
        out
    }

    /// The clusters of a file, live or deleted, whether any have been
    /// reused, and whether the layout is certain.
    fn clusters(
        &self,
        first: u32,
        len: u64,
        contiguous: bool,
        deleted: bool,
    ) -> (Vec<u32>, bool, bool) {
        let needed = len.div_ceil(self.cluster_size);
        if needed == 0 || first < 2 || first > self.max_cluster() {
            return (Vec::new(), false, true);
        }
        if contiguous {
            let last = (u64::from(first) + needed - 1).min(u64::from(self.max_cluster())) as u32;
            let list: Vec<u32> = (first..=last).collect();
            let overwritten = deleted && list.iter().any(|&c| self.in_use(c));
            return (list, overwritten, true);
        }
        let chain = self.chain(first, needed);
        if !deleted {
            return (chain, false, true);
        }
        // Some systems leave the FAT chain of a deleted file intact. Trust it
        // only if it is complete and none of it has been reallocated.
        if chain.len() as u64 == needed && chain.iter().all(|&c| !self.in_use(c)) {
            return (chain, false, true);
        }
        let (list, overwritten) =
            deleted_clusters(first, needed, self.max_cluster(), |c| self.in_use(c));
        (list, overwritten, false)
    }

    fn read(&self, clusters: &[u32], len: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut r = Reader::new(self.src, 0);
        for &c in clusters {
            r.seek(self.offset_of(c));
            match r.bytes(self.cluster_size as usize) {
                Some(b) => out.extend(b),
                None => break,
            }
        }
        out.truncate(len as usize);
        out
    }
}

/// Whether `b` is an exFAT boot sector this reader can use.
fn valid_boot(b: &[u8]) -> bool {
    &b[3..11] == b"EXFAT   " && (9..=12).contains(&b[108]) && b[109] <= 25 - b[108]
}

/// The copy of an exFAT volume's boot sector, 12 sectors in (the backup
/// boot region), for when the first one is damaged. It must agree on its
/// own sector size, and the FAT it points to must start as every exFAT
/// FAT does, so that a boot sector belonging to a volume that really
/// starts elsewhere is not taken for this volume's copy.
pub(crate) fn backup_boot(src: &dyn Source, offset: u64) -> Option<Vec<u8>> {
    for shift in [9u8, 12] {
        let mut r = Reader::new(src, offset.checked_add(12 << shift)?);
        let Some(b) = r.bytes(512) else { continue };
        if !valid_boot(&b) || b[108] != shift {
            continue;
        }
        let fat = u64::from(u32::from_le_bytes(b[80..84].try_into().unwrap())) << shift;
        r.seek(offset.checked_add(fat)?);
        if r.bytes(4).is_some_and(|f| f == [0xF8, 0xFF, 0xFF, 0xFF]) {
            return Some(b);
        }
    }
    None
}

pub fn read(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    let mut r = Reader::new(src, offset);
    let first = r.bytes(512)?;
    let boot = if valid_boot(&first) {
        first
    } else {
        backup_boot(src, offset)?
    };
    let u32_at = |o: usize| u32::from_le_bytes(boot[o..o + 4].try_into().unwrap());
    let (bps_shift, spc_shift) = (boot[108], boot[109]);
    if !(9..=12).contains(&bps_shift) || spc_shift > 25 - bps_shift {
        return None;
    }
    let bps = 1u64 << bps_shift;
    let cluster_size = bps << spc_shift;
    // The FAT is read whole and free space walked cluster by cluster, so
    // the count is limited to what the volume can hold (and kept clear of
    // u32::MAX, where the cluster numbering would overflow).
    let fits = len.min(src.len().saturating_sub(offset)) / cluster_size;
    let clusters = u32_at(92).min(fits.min(u64::from(u32::MAX - 2)) as u32);

    r.seek(offset + u64::from(u32_at(80)) * bps);
    let raw = r.bytes((clusters as usize + 2) * 4)?;
    let table = raw
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();

    let mut fs = ExFat {
        src,
        table,
        bitmap: Vec::new(),
        cluster_size,
        heap: offset + u64::from(u32_at(88)) * bps,
        clusters,
    };

    let root_clusters = fs.chain(u32_at(96), MAX_DIR_LEN.div_ceil(cluster_size));
    let root = fs.read(&root_clusters, u64::MAX);

    // The root directory holds the allocation bitmap and the volume label.
    let mut label = None;
    for e in root.chunks_exact(32) {
        match e[0] {
            0 => break,
            BITMAP if e[1] & 1 == 0 => {
                let first = u32::from_le_bytes(e[20..24].try_into().unwrap());
                let size = u64::from_le_bytes(e[24..32].try_into().unwrap());
                let list = fs.chain(first, size.div_ceil(cluster_size));
                fs.bitmap = fs.read(&list, size);
            }
            LABEL => {
                let n = (e[1] as usize).min(11);
                let units: Vec<u16> = e[2..2 + n * 2]
                    .chunks(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                label = Some(String::from_utf16_lossy(&units));
            }
            _ => {}
        }
    }
    if fs.bitmap.is_empty() {
        return None;
    }

    let mut walk = Walk {
        fs: &fs,
        files: Vec::new(),
        allocs: Vec::new(),
        visited: HashSet::new(),
    };
    walk.dir(&root, "", false, 0);
    let Walk {
        mut files, allocs, ..
    } = walk;
    super::add_alternatives(
        &mut files,
        &allocs,
        fs.max_cluster(),
        |c| fs.in_use(c),
        cluster_size,
        |c| fs.offset_of(c),
    );

    let free = free_extents(
        clusters,
        |c| fs.in_use(c),
        cluster_size,
        |c| fs.offset_of(c),
    );
    Some(Volume {
        fs: FsType::ExFat,
        label,
        offset,
        len,
        cluster_size,
        files,
        free,
        apfs: None,
        names: Vec::new(),
        name_sources: Default::default(),
        snapshots: Vec::new(),
    })
}

struct Walk<'f, 'a> {
    fs: &'f ExFat<'a>,
    files: Vec<FsFile>,
    allocs: Vec<DeletedAlloc>,
    visited: HashSet<u32>,
}

impl Walk<'_, '_> {
    fn dir(&mut self, data: &[u8], path: &str, parent_deleted: bool, depth: usize) {
        let entries: Vec<&[u8]> = data.chunks_exact(32).collect();
        let mut i = 0;
        while i < entries.len() {
            let e = entries[i];
            if e[0] == 0 {
                break;
            }
            if e[0] & 0x7F != FILE {
                i += 1;
                continue;
            }
            let count = e[1] as usize;
            let set_deleted = e[0] & IN_USE == 0;
            let Some(set) = entries.get(i + 1..i + 1 + count) else {
                break;
            };
            // A deleted set may have been partly reused by a newer file.
            let consistent = count >= 2
                && set[0][0] & 0x7F == STREAM
                && set[1..].iter().all(|s| s[0] & 0x7F == NAME)
                && set.iter().all(|s| (s[0] & IN_USE == 0) == set_deleted);
            if !consistent {
                i += 1;
                continue;
            }
            i += 1 + count;
            self.entry(e, set, path, parent_deleted || set_deleted, depth);
        }
    }

    fn entry(&mut self, file: &[u8], set: &[&[u8]], path: &str, deleted: bool, depth: usize) {
        let stream = set[0];
        let name_len = stream[3] as usize;
        let units: Vec<u16> = set[1..]
            .iter()
            .flat_map(|n| n[2..32].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])))
            .take(name_len)
            .collect();
        let name = String::from_utf16_lossy(&units);
        if name.is_empty() {
            return;
        }
        let attrs = u16::from_le_bytes([file[4], file[5]]);
        let ts = |o: usize| {
            let v = u32::from_le_bytes(file[o..o + 4].try_into().unwrap());
            dos_datetime((v >> 16) as u16, v as u16)
        };
        let contiguous = stream[1] & NO_FAT_CHAIN != 0;
        let valid_len = u64::from_le_bytes(stream[8..16].try_into().unwrap());
        let first = u32::from_le_bytes(stream[20..24].try_into().unwrap());
        let data_len = u64::from_le_bytes(stream[24..32].try_into().unwrap());

        let fs = self.fs;
        let (clusters, overwritten, certain) = fs.clusters(first, data_len, contiguous, deleted);

        if attrs & ATTR_DIR != 0 {
            if depth < MAX_DEPTH && first >= 2 && self.visited.insert(first) {
                let len = data_len.min(MAX_DIR_LEN);
                let wanted = len.div_ceil(fs.cluster_size) as usize;
                let sub = fs.read(&clusters[..clusters.len().min(wanted)], len);
                self.dir(&sub, &join_path(path, &name), deleted, depth + 1);
            }
            return;
        }

        let size = valid_len.min(data_len);
        if deleted {
            self.allocs.push(DeletedAlloc {
                file: self.files.len(),
                first,
                needed: data_len.div_ceil(fs.cluster_size),
                clusters: clusters.clone(),
                certain,
            });
        }
        self.files.push(FsFile {
            dir: path.to_string(),
            name,
            size,
            modified: ts(12),
            created: ts(8),
            deleted,
            extents: cluster_extents(clusters, size, fs.cluster_size, |c| fs.offset_of(c)),
            overwritten,
            alternatives: Vec::new(),
        });
    }
}

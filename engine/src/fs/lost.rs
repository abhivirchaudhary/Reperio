//! Finding volumes that no partition table lists, as TestDisk does.
//!
//! When a partition table is wiped, a partition deleted, or a disk
//! re-partitioned, the file systems themselves are usually still there.
//! Each one starts with a structure that says what it is and how big it
//! is (a boot sector, a superblock, a volume header), and most keep a copy
//! of it elsewhere in case the first is damaged:
//!
//! - NTFS: a copy of the boot sector in the volume's last sector.
//! - FAT32: a copy of the boot sector 6 sectors in.
//! - exFAT: a copy of the boot region 12 sectors in.
//! - HFS+: a copy of the volume header 1024 bytes before the end.
//! - ext2/3/4: copies of the superblock at the start of groups 1, 3, 5,
//!   7, 9, 25, 27..., each saying which group it is in, and so how far it
//!   is from the volume's start.
//! - APFS: the container superblock in block 0, with its block size and
//!   count.
//!
//! The search reads the drive, looks for these at every sector it reads,
//! and works out from each where its volume starts and how long it is.
//! Every such volume is then opened like any other, and kept only if it
//! holds files: a structure that merely looks right is not enough.
//!
//! A drive read over USB pays a round trip for every read, so reads are
//! large and checked in memory. Two ways of searching:
//!
//! - **Likely starts** (always done where no readable volume is): only the
//!   places partitioning tools put volumes are read, with a few KiB either
//!   side: every 1 MiB (2048 sectors, modern alignment), and every
//!   cylinder (255 heads of 63 sectors) and 63 sectors past it (older
//!   tools, and logical partitions). Copies at a volume's end or a few
//!   sectors in, and ext's superblock copies (whole groups apart), fall in
//!   the same windows when the volume itself was aligned.
//! - **Every sector** (an option): the whole area is read, so volumes at
//!   any sector are found, including ones only a copy at an odd place
//!   still identifies.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{ext4, fat, FsType, Volume};
use crate::source::{read_skipping, Source};

const SECTOR: u64 = 512;
const MIB: u64 = 1 << 20;
/// 63 sectors: one track of the old cylinder-head-sector geometry.
const TRACK: u64 = 63 * SECTOR;
/// 255 heads of 63 sectors: the cylinder older tools aligned to.
const CYLINDER: u64 = 255 * TRACK;
/// Read before each likely start: the last sectors of a volume that ends
/// there, where NTFS and HFS+ keep their copies.
const BEFORE: u64 = 4096;
/// Read after each likely start: boot sector, superblock and volume header
/// (all in the first 2 KiB), FAT32's copy at 6 sectors and exFAT's at 12.
const AFTER: u64 = 8192;
/// Bytes past the last sector checked in a read, so that a superblock
/// (1024 bytes) found there is complete.
const TAIL: u64 = 1024;
/// Windows this close together are read as one: a round trip costs more
/// than the bytes in between.
const JOIN: u64 = 64 << 10;
/// The most read at once.
const CHUNK: u64 = 4 << 20;
/// Smaller "volumes" are not worth listing.
const MIN_LEN: u64 = 64 << 10;
/// The most possible volume starts kept, and opened. A drive full of disk
/// images could have many; a corrupt one must not make the search endless.
const MAX_CANDIDATES: usize = 65_536;
const MAX_OPEN: usize = 512;

/// A volume found by searching the drive.
#[derive(Debug, Clone)]
pub struct Found {
    /// Found only by a copy of its first structure: the first one did not
    /// identify it (damaged, or overwritten).
    pub backup: bool,
    pub volume: Volume,
}

/// A place a volume may start, worked out from a structure found.
#[derive(Debug, Clone)]
struct Candidate {
    start: u64,
    len: u64,
    fs: FsType,
    backup: bool,
    /// Bytes the volume must have at these offsets, any one of them, for
    /// it to be worth opening (a FAT's first entry, NTFS's file table).
    /// Empty when the structure found was check enough.
    probes: Vec<(u64, Vec<u8>)>,
}

/// Searches `areas` (start and end offsets) of `src` for volumes.
///
/// With `every_sector`, every sector of the areas is read and checked;
/// otherwise only around likely volume starts (see the module docs).
/// Volumes starting at one of `known` (those already read from the
/// partition table) and volumes starting outside `areas` are left out.
/// `progress` gets bytes of the areas passed and their total. Returns the
/// volumes found, in order on the drive, none overlapping another.
pub fn find(
    src: &dyn Source,
    areas: &[(u64, u64)],
    every_sector: bool,
    known: &[u64],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Vec<Found> {
    let disk = src.len();
    let mut areas: Vec<(u64, u64)> = areas
        .iter()
        .map(|&(a, e)| (a, e.min(disk)))
        .filter(|&(a, e)| a < e)
        .collect();
    areas.sort_unstable();
    let total: u64 = areas.iter().map(|&(a, e)| e - a).sum();

    let mut found: BTreeMap<(u64, u8), Candidate> = BTreeMap::new();
    let mut add = |c: Candidate| {
        let key = (c.start, c.fs as u8);
        match found.get(&key) {
            // The volume's own structure is better evidence than a copy.
            Some(old) if !old.backup || c.backup => {}
            _ if found.len() >= MAX_CANDIDATES && !found.contains_key(&key) => {}
            _ => {
                found.insert(key, c);
            }
        }
    };

    let mut buf = Vec::new();
    let mut passed = 0u64;
    for &(a, e) in &areas {
        let mut pos = a;
        while pos < e {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let (from, to, next) = if every_sector {
                let to = pos.saturating_add(CHUNK).min(e);
                (pos, to, to)
            } else {
                let Some((from, to, next)) = windows(pos, e) else {
                    break;
                };
                (from, to, next)
            };
            let from = from - from % SECTOR;
            let end = to.saturating_add(TAIL).min(disk);
            buf.resize((end - from) as usize, 0);
            let (n, _) = read_skipping(src, from, &mut buf);
            let mut q = from;
            while q < to && q + SECTOR <= from + n as u64 {
                let at = (q - from) as usize;
                structures(q, &buf[at..n], &mut add);
                q += SECTOR;
            }
            pos = next;
            progress(passed + pos.min(e) - a, total);
        }
        passed += e - a;
        progress(passed, total);
    }

    let inside = |s: u64| areas.iter().any(|&(a, e)| (a..e).contains(&s));
    let mut candidates: Vec<Candidate> = found
        .into_values()
        .filter(|c| inside(c.start) && !known.contains(&c.start))
        .filter(|c| c.len >= MIN_LEN && c.start.checked_add(c.len).is_some_and(|e| e <= disk))
        .filter(|c| plausible(src, c))
        .collect();
    // The volumes' own structures first: when there are more than can be
    // opened, copies are the ones to go without.
    candidates.sort_by_key(|c| (c.backup, c.start));
    candidates.truncate(MAX_OPEN);

    let mut opened: Vec<Found> = Vec::new();
    for c in candidates {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        // Real content, not just a structure that parses: a volume must
        // list at least one file, live or deleted.
        if let Some(volume) = super::open(src, c.start, c.len).filter(|v| !v.files.is_empty()) {
            opened.push(Found {
                backup: c.backup,
                volume,
            });
        }
    }
    best_without_overlaps(opened)
}

/// Keeps the best of volumes that overlap: the one listing more files,
/// then one found by its own structure over one found by a copy. The same
/// volume found twice (by its boot sector and by a copy) is one of them.
fn best_without_overlaps(mut all: Vec<Found>) -> Vec<Found> {
    all.sort_by_key(|f| {
        (
            std::cmp::Reverse(f.volume.files.len()),
            f.backup,
            f.volume.offset,
        )
    });
    let mut kept: Vec<Found> = Vec::new();
    for f in all {
        let (s, e) = (f.volume.offset, f.volume.offset + f.volume.len);
        let overlaps = kept.iter().any(|k| {
            let (ks, ke) = (k.volume.offset, k.volume.offset + k.volume.len);
            s < ke && ks < e
        });
        if !overlaps {
            kept.push(f);
        }
    }
    kept.sort_by_key(|f| f.volume.offset);
    kept
}

/// The next likely volume start at or after `x`: a 1 MiB boundary, a
/// cylinder boundary, or a track past one.
fn next_start(x: u64) -> u64 {
    let up = |x: u64, m: u64| x.div_ceil(m).saturating_mul(m);
    let track = up(x.saturating_sub(TRACK), CYLINDER).saturating_add(TRACK);
    up(x, MIB).min(up(x, CYLINDER)).min(track)
}

/// The likely start after `b`, if there is one further on.
fn start_after(b: u64) -> Option<u64> {
    b.checked_add(1).map(next_start).filter(|&n| n > b)
}

/// The next stretch to read for likely starts from `pos` up to `end`
/// (inclusive: a volume ending at `end` keeps its copies just before it).
/// Joins windows close together, up to [`CHUNK`]. Returns where to read
/// from and to, and where to go on from, or `None` when no start is left.
fn windows(pos: u64, end: u64) -> Option<(u64, u64, u64)> {
    let first = next_start(pos);
    if first > end {
        return None;
    }
    let from = first.saturating_sub(BEFORE);
    let mut to = first.saturating_add(AFTER);
    let mut next = start_after(first);
    while let Some(n) = next.filter(|&n| {
        n <= end && n.saturating_sub(BEFORE) <= to.saturating_add(JOIN) && to - from < CHUNK
    }) {
        to = n.saturating_add(AFTER);
        next = start_after(n);
    }
    Some((from, to, next.unwrap_or(u64::MAX)))
}

fn le16(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn be32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_be_bytes(b[at..at + 4].try_into().unwrap()))
}

/// Looks for volume structures in the sector at `at`, whose bytes (and
/// whatever follows, up to a superblock's 1024) start `b`. Every value is
/// untrusted: anything that overflows is no candidate.
fn structures(at: u64, b: &[u8], add: &mut impl FnMut(Candidate)) {
    let s = &b[..SECTOR as usize];
    if s[510..512] == [0x55, 0xAA] {
        match &s[3..11] {
            b"EXFAT   " => exfat(at, s, add),
            b"NTFS    " => ntfs(at, s, add),
            _ => {
                if let Some(kind) = fat::detect(s) {
                    fat_boot(at, s, kind, add);
                }
            }
        }
    }
    if matches!(&s[..2], b"H+" | b"HX") {
        hfsplus(at, s, add);
    }
    if &s[32..36] == b"NXSB" {
        apfs(at, s, add);
    }
    if s[0x38..0x3A] == [0x53, 0xEF] && b.len() >= 1024 {
        if let Some((fs, len, sb_at, group)) = ext4::locate(&b[..1024]) {
            if let Some(start) = at.checked_sub(sb_at) {
                add(Candidate {
                    start,
                    len,
                    fs,
                    backup: group > 0,
                    probes: Vec::new(),
                });
            }
        }
    }
}

/// A FAT boot sector: the volume's own, or FAT32's copy.
fn fat_boot(at: u64, s: &[u8], kind: FsType, add: &mut impl FnMut(Candidate)) {
    let bps = le16(s, 11);
    let total = match le16(s, 19) {
        0 => le32(s, 32),
        n => n,
    };
    let reserved = le16(s, 14);
    let Some(len) = total.checked_mul(bps) else {
        return;
    };
    // The first FAT entry repeats the media byte, then all ones.
    let probe = |start: u64| {
        let fat = start.checked_add(reserved * bps)?;
        Some(vec![(fat, vec![s[21], 0xFF, 0xFF])])
    };
    let mut push = |start: u64, backup: bool| {
        if let Some(probes) = probe(start) {
            add(Candidate {
                start,
                len,
                fs: kind,
                backup,
                probes,
            });
        }
    };
    push(at, false);
    let copy = le16(s, 50);
    if kind == FsType::Fat32 && copy > 0 && copy < reserved {
        if let Some(start) = at.checked_sub(copy * bps) {
            push(start, true);
        }
    }
}

/// An exFAT boot sector: the volume's own, or the copy 12 sectors in.
fn exfat(at: u64, s: &[u8], add: &mut impl FnMut(Candidate)) {
    let (shift, spc) = (u32::from(s[108]), u32::from(s[109]));
    if !(9..=12).contains(&shift) || spc > 25 - shift {
        return;
    }
    let Some(len) = le64(s, 72).checked_mul(1 << shift) else {
        return;
    };
    let fat = le32(s, 80) << shift;
    let mut push = |start: u64, backup: bool| {
        if let Some(f) = start.checked_add(fat) {
            add(Candidate {
                start,
                len,
                fs: FsType::ExFat,
                backup,
                probes: vec![(f, vec![0xF8, 0xFF, 0xFF, 0xFF])],
            });
        }
    };
    push(at, false);
    if let Some(start) = at.checked_sub(12 << shift) {
        push(start, true);
    }
}

/// An NTFS boot sector: the volume's own, or the copy in its last sector.
/// The boot sector counts the volume's sectors less that last one.
fn ntfs(at: u64, s: &[u8], add: &mut impl FnMut(Candidate)) {
    let bps = le16(s, 11);
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return;
    }
    let spc = u32::from(s[13]);
    let cluster = if spc > 0x80 {
        1u64.checked_shl(256 - spc)
    } else {
        Some(u64::from(spc) * bps)
    };
    let Some(cluster) = cluster.filter(|&c| c > 0 && c <= 2 << 20) else {
        return;
    };
    let total = le64(s, 40);
    let Some(len) = total.checked_add(1).and_then(|n| n.checked_mul(bps)) else {
        return;
    };
    let (mft, mirror) = (le64(s, 48), le64(s, 56));
    let mut push = |start: u64, backup: bool| {
        // The file table, or its mirror, starts with a file record.
        let probes = [mft, mirror]
            .iter()
            .filter_map(|&lcn| {
                Some((
                    start.checked_add(lcn.checked_mul(cluster)?)?,
                    b"FILE".to_vec(),
                ))
            })
            .collect::<Vec<_>>();
        if !probes.is_empty() {
            add(Candidate {
                start,
                len,
                fs: FsType::Ntfs,
                backup,
                probes,
            });
        }
    };
    push(at, false);
    if let Some(start) = total.checked_mul(bps).and_then(|n| at.checked_sub(n)) {
        push(start, true);
    }
}

/// An HFS+ volume header: the volume's own, 1024 bytes in, or the copy
/// 1024 bytes before its end.
fn hfsplus(at: u64, s: &[u8], add: &mut impl FnMut(Candidate)) {
    let version = u16::from_be_bytes([s[2], s[3]]);
    let block = be32(s, 40);
    if !matches!(version, 4 | 5) || !(512..=1 << 20).contains(&block) || !block.is_power_of_two() {
        return;
    }
    let len = be32(s, 44) * block;
    if let Some(start) = at.checked_sub(1024) {
        add(Candidate {
            start,
            len,
            fs: FsType::HfsPlus,
            backup: false,
            probes: Vec::new(),
        });
    }
    if let Some(start) = (at + 1024).checked_sub(len) {
        add(Candidate {
            start,
            len,
            fs: FsType::HfsPlus,
            backup: true,
            probes: Vec::new(),
        });
    }
}

/// An APFS container superblock, in the container's first block. (The
/// checkpoint area keeps more copies; taken for block 0, they fail to
/// open.)
fn apfs(at: u64, s: &[u8], add: &mut impl FnMut(Candidate)) {
    let block = le32(s, 36);
    if !(4096..=65536).contains(&block) || !block.is_power_of_two() {
        return;
    }
    if let Some(len) = le64(s, 40).checked_mul(block) {
        add(Candidate {
            start: at,
            len,
            fs: FsType::Apfs,
            backup: false,
            probes: Vec::new(),
        });
    }
}

/// Whether a candidate has what its kind of volume must have, checked
/// with a read or two before opening it (which can read megabytes).
fn plausible(src: &dyn Source, c: &Candidate) -> bool {
    c.probes.is_empty()
        || c.probes.iter().any(|(at, want)| {
            let mut got = vec![0u8; want.len()];
            src.read_at(*at, &mut got).ok() == Some(want.len()) && got == *want
        })
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
                (x >> 24) as u8
            })
            .collect()
    }

    fn search(disk: &Vec<u8>, every_sector: bool) -> Vec<Found> {
        find(
            disk,
            &[(0, disk.len() as u64)],
            every_sector,
            &[],
            &AtomicBool::new(false),
            &mut |_, _| {},
        )
    }

    #[test]
    fn likely_starts_cover_alignments() {
        assert_eq!(next_start(0), 0);
        assert_eq!(next_start(1), TRACK);
        assert_eq!(next_start(TRACK + 1), MIB);
        assert_eq!(next_start(7 * MIB + 1), CYLINDER);
        assert_eq!(next_start(CYLINDER + 1), CYLINDER + TRACK);
        assert_eq!(next_start(u64::MAX - 5), u64::MAX);
        // Windows end, and never loop, at the end of the area and of u64.
        assert!(windows(u64::MAX - 5, u64::MAX).is_some_and(|(_, _, n)| n == u64::MAX));
        let (from, to, next) = windows(0, 64 * MIB).unwrap();
        assert_eq!(from, 0);
        assert!(to >= TRACK + AFTER && next > 0);
    }

    #[test]
    fn random_data_has_no_volumes() {
        for seed in 1..4 {
            let disk = noise(24 << 20, seed);
            assert!(search(&disk, true).is_empty());
            assert!(search(&disk, false).is_empty());
        }
    }

    /// Structures that parse, with nonsense behind them, at every
    /// alignment and with boundary values in the fields read: nothing may
    /// be found, and nothing may panic, overflow or take long.
    #[test]
    fn corrupted_structures_are_rejected_safely() {
        let started = std::time::Instant::now();
        let values: [u64; 8] = [0, 1, 0x7F, 0x80, 0xFF, 0xFFFF, u32::MAX as u64, u64::MAX];
        for (i, &v) in values.iter().enumerate() {
            let mut disk = noise(8 << 20, 77 + i as u64);
            for k in 0..64u64 {
                let at = (k * 63 * 512 + k * 4096) as usize % (disk.len() - 4096);
                let at = at - at % 512;
                let b = &mut disk[at..at + 2048];
                match k % 6 {
                    0 => {
                        b[0] = 0xEB;
                        b[11..13].copy_from_slice(&512u16.to_le_bytes());
                        b[13] = 8;
                        b[14..16].copy_from_slice(&(v as u16).to_le_bytes());
                        b[16] = 2;
                        b[19..21].copy_from_slice(&0u16.to_le_bytes());
                        b[22..24].copy_from_slice(&0u16.to_le_bytes());
                        b[32..36].copy_from_slice(&(v as u32 | 1).to_le_bytes());
                        b[36..40].copy_from_slice(&(v as u32 | 1).to_le_bytes());
                        b[50..52].copy_from_slice(&(v as u16).to_le_bytes());
                    }
                    1 => {
                        b[3..11].copy_from_slice(b"EXFAT   ");
                        b[72..80].copy_from_slice(&v.to_le_bytes());
                        b[80..84].copy_from_slice(&(v as u32).to_le_bytes());
                        b[108] = 9 + (k % 4) as u8;
                        b[109] = v as u8;
                    }
                    2 => {
                        b[3..11].copy_from_slice(b"NTFS    ");
                        b[11..13].copy_from_slice(&512u16.to_le_bytes());
                        b[13] = v as u8;
                        b[40..48].copy_from_slice(&v.to_le_bytes());
                        b[48..56].copy_from_slice(&v.to_le_bytes());
                        b[56..64].copy_from_slice(&v.wrapping_add(1).to_le_bytes());
                    }
                    3 => {
                        b[..2].copy_from_slice(b"H+");
                        b[2..4].copy_from_slice(&4u16.to_be_bytes());
                        b[40..44].copy_from_slice(&4096u32.to_be_bytes());
                        b[44..48].copy_from_slice(&(v as u32).to_be_bytes());
                    }
                    4 => {
                        b[32..36].copy_from_slice(b"NXSB");
                        b[36..40].copy_from_slice(&4096u32.to_le_bytes());
                        b[40..48].copy_from_slice(&v.to_le_bytes());
                    }
                    _ => {
                        b[0x38..0x3A].copy_from_slice(&[0x53, 0xEF]);
                        b[0x18..0x1C].copy_from_slice(&((v % 3) as u32).to_le_bytes());
                        b[0x04..0x08].copy_from_slice(&(v as u32).to_le_bytes());
                        b[0x20..0x24].copy_from_slice(&8192u32.to_le_bytes());
                        b[0x28..0x2C].copy_from_slice(&2048u32.to_le_bytes());
                        b[0x5A..0x5C].copy_from_slice(&(v as u16).to_le_bytes());
                    }
                }
                if k % 6 < 3 {
                    b[510..512].copy_from_slice(&[0x55, 0xAA]);
                }
            }
            assert!(search(&disk, true).is_empty(), "value {v:#x}");
            assert!(search(&disk, false).is_empty(), "value {v:#x}");
        }
        assert!(started.elapsed().as_secs() < 120);
    }

    /// A cancelled search stops at once and reports nothing.
    #[test]
    fn cancel_stops_the_search() {
        let disk = noise(16 << 20, 9);
        let cancel = AtomicBool::new(true);
        let mut calls = 0;
        let got = find(
            &disk,
            &[(0, disk.len() as u64)],
            true,
            &[],
            &cancel,
            &mut |_, _| calls += 1,
        );
        assert!(got.is_empty());
        assert_eq!(calls, 0);
    }
}

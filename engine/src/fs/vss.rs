//! Windows Volume Shadow Copies: earlier states of an NTFS volume, kept by
//! System Restore, Windows Backup and "Previous Versions".
//!
//! A shadow copy is not a full copy. When a 16 KB block of the volume is
//! about to be overwritten for the first time after a shadow copy was
//! taken, Windows first copies the old block into a store (files in
//! `System Volume Information`) and notes where. So the volume as it was
//! is the current volume with those blocks swapped back in. A file deleted
//! since keeps its data either way: untouched blocks are still where they
//! were, and overwritten ones are in the store.
//!
//! Layout (as documented by libvshadow): a volume header at 0x1E00 inside
//! NTFS's boot area points to a catalog, a chain of 16 KB blocks listing
//! each shadow copy (its store) with its date and where its block list
//! is. A store's block list is a chain of 16 KB blocks of 32-byte
//! descriptors: original block, where its old content is, and flags.
//!
//! Everything here comes from the disk, so every offset is checked
//! against the volume and every chain and count is bounded.

use std::collections::{HashMap, HashSet};
use std::io;

use super::{ntfs, FsFile, SnapshotFile, Volume};
use crate::extent::{push_merged, Extent, MappedSource};
use crate::formats::DateTime;
use crate::source::{Cached, Source};

/// Where the volume header is, from the start of the volume.
const HEADER_AT: u64 = 0x1E00;
/// `{3808876b-c176-4e48-b7ae-04046e6cc752}` as stored on disk.
const VSS_ID: [u8; 16] = [
    0x6b, 0x87, 0x08, 0x38, 0x76, 0xc1, 0x48, 0x4e, 0xb7, 0xae, 0x04, 0x04, 0x6e, 0x6c, 0xc7, 0x52,
];
/// Shadow copies work in blocks of this size; catalog and block lists are
/// stored in blocks of the same size.
const BLOCK: u64 = 0x4000;
const SECTOR: u64 = 512;
const SECTORS: usize = (BLOCK / SECTOR) as usize;
const REC_HEADER: u32 = 1;
const REC_CATALOG: u32 = 2;
const REC_BLOCK_LIST: u32 = 3;
/// The block's old content is that of another block (it was moved).
const FORWARDER: u32 = 1;
/// Only the sectors flagged in the descriptor's bitmap are in the store.
const OVERLAY: u32 = 2;
const NOT_USED: u32 = 4;
/// Windows keeps at most 64 shadow copies of a volume.
const MAX_STORES: usize = 64;
const MAX_CATALOG_BLOCKS: usize = 64;
/// Bounds memory on a corrupt or huge store: 4 million descriptors cover
/// 64 GB of changed blocks, at about 24 bytes each.
const MAX_DESCRIPTORS: usize = 4_000_000;
/// Seconds between 1601-01-01 (Windows time) and 1970-01-01.
const WINDOWS_EPOCH_OFFSET: i64 = 11_644_473_600;

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// One block descriptor, with offsets relative to the volume.
#[derive(Debug, Clone, Copy)]
struct Desc {
    original: u64,
    /// Where the old content is; for a forwarder, the block whose content
    /// it is.
    target: u64,
    flags: u32,
    bitmap: u32,
}

/// One shadow copy's store: the old blocks copied out after it was taken.
#[derive(Debug, Default)]
struct Store {
    /// Windows time the shadow copy was taken.
    created: u64,
    /// The volume's size then.
    size: u64,
    /// Sorted by original block, in list order within a block.
    descs: Vec<Desc>,
}

/// All of a volume's shadow copies, oldest first.
#[derive(Debug, Default)]
struct Stores {
    stores: Vec<Store>,
}

impl Stores {
    /// Where each sector of the 16 KB block at `block` was in shadow copy
    /// `from`, as volume offsets.
    ///
    /// A block changed after shadow copy N was taken, but before the next
    /// one, is in store N; one changed later is in the newer store that
    /// was current then, and one never changed is still in place. So the
    /// stores are looked through from N onwards, then the volume.
    fn sectors(&self, block: u64, from: usize) -> [u64; SECTORS] {
        let mut out = [0u64; SECTORS];
        for (i, s) in out.iter_mut().enumerate() {
            *s = block + i as u64 * SECTOR;
        }
        let Some(store) = self.stores.get(from) else {
            return out;
        };
        let at = store.descs.partition_point(|d| d.original < block);
        let descs = store.descs[at..]
            .iter()
            .take_while(|d| d.original == block)
            .copied()
            .collect::<Vec<_>>();
        let base = descs.iter().find(|d| d.flags & OVERLAY == 0);
        match base {
            // A forwarder's target is looked up from the next store on,
            // so the recursion ends after at most `MAX_STORES` steps.
            Some(d) if d.flags & FORWARDER != 0 => out = self.sectors(d.target, from + 1),
            Some(d) => {
                for (i, s) in out.iter_mut().enumerate() {
                    *s = d.target + i as u64 * SECTOR;
                }
            }
            None => out = self.sectors(block, from + 1),
        }
        for d in descs.iter().filter(|d| d.flags & OVERLAY != 0) {
            for (i, s) in out.iter_mut().enumerate() {
                if d.bitmap & (1 << i) != 0 {
                    *s = d.target + i as u64 * SECTOR;
                }
            }
        }
        out
    }
}

/// A shadow copy as a volume of its own, offsets from its start.
struct Shadow<'a> {
    src: &'a dyn Source,
    /// Where the live volume starts on the source.
    base: u64,
    len: u64,
    stores: &'a Stores,
    index: usize,
}

impl Shadow<'_> {
    /// The source extents holding the shadow copy's bytes `[offset,
    /// offset + len)`.
    fn map(&self, offset: u64, len: u64) -> Vec<Extent> {
        let mut out = Vec::new();
        let end = offset.saturating_add(len).min(self.len);
        let mut pos = offset;
        while pos < end {
            let block = pos - pos % BLOCK;
            let sectors = self.stores.sectors(block, self.index);
            let mut i = ((pos - block) / SECTOR) as usize;
            while i < SECTORS && pos < end {
                let sector_end = (block + (i as u64 + 1) * SECTOR).min(end);
                let within = pos - (block + i as u64 * SECTOR);
                let at = self.base.saturating_add(sectors[i]).saturating_add(within);
                push_merged(&mut out, Extent::new(at, sector_end - pos));
                pos = sector_end;
                i += 1;
            }
        }
        out
    }
}

impl Source for Shadow<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        for e in self.map(offset, buf.len() as u64) {
            let n = e.len as usize;
            let got = self.src.read_at(e.offset, &mut buf[done..done + n])?;
            done += got;
            if got < n {
                break;
            }
        }
        Ok(done)
    }
}

/// Reads one 16 KB catalog or block-list block at volume offset `at`,
/// checking it is what it says.
fn read_block(src: &dyn Source, base: u64, vol_len: u64, at: u64, kind: u32) -> Option<Vec<u8>> {
    if at == 0 || !at.is_multiple_of(SECTOR) || at.checked_add(BLOCK)? > vol_len {
        return None;
    }
    let mut b = vec![0u8; BLOCK as usize];
    if src.read_at(base.checked_add(at)?, &mut b).ok()? < b.len() {
        return None;
    }
    (b[..16] == VSS_ID && le32(&b, 20) == kind).then_some(b)
}

/// Reads the shadow copies of the NTFS volume at `base`, oldest first.
/// None if it has none.
fn read_stores(src: &dyn Source, base: u64, vol_len: u64) -> Stores {
    let mut stores = Stores::default();
    let mut h = [0u8; 512];
    let Some(at) = base.checked_add(HEADER_AT) else {
        return stores;
    };
    if src.read_at(at, &mut h).ok() != Some(512)
        || h[..16] != VSS_ID
        || !(1..=2).contains(&le32(&h, 16))
        || le32(&h, 20) != REC_HEADER
    {
        return stores;
    }

    // The catalog: an entry of type 2 (size, date) and one of type 3
    // (where the block list is) per store, matched by the store's ID.
    let mut info: HashMap<[u8; 16], (u64, u64)> = HashMap::new();
    let mut lists: Vec<([u8; 16], u64)> = Vec::new();
    let mut next = le64(&h, 48);
    let mut seen = HashSet::new();
    while seen.len() < MAX_CATALOG_BLOCKS && seen.insert(next) {
        let Some(b) = read_block(src, base, vol_len, next, REC_CATALOG) else {
            break;
        };
        for e in b[128..].chunks_exact(128) {
            let id: [u8; 16] = e[16..32].try_into().unwrap();
            match le64(e, 0) {
                2 => {
                    info.insert(id, (le64(e, 8), le64(e, 48)));
                }
                3 if lists.len() < MAX_STORES && !lists.iter().any(|l| l.0 == id) => {
                    lists.push((id, le64(e, 8)));
                }
                _ => {}
            }
        }
        next = le64(&b, 40);
    }

    let mut total = 0;
    for (id, list) in lists {
        let Some(&(size, created)) = info.get(&id) else {
            continue;
        };
        let mut store = Store {
            created,
            size,
            descs: Vec::new(),
        };
        let mut next = list;
        let mut seen = HashSet::new();
        // Each list block holds 508 descriptors, so the descriptor limit
        // also bounds the blocks followed.
        while total < MAX_DESCRIPTORS && seen.insert(next) {
            let Some(b) = read_block(src, base, vol_len, next, REC_BLOCK_LIST) else {
                break;
            };
            for e in b[128..].chunks_exact(32) {
                let d = Desc {
                    original: le64(e, 0),
                    target: if le32(e, 24) & FORWARDER != 0 {
                        le64(e, 8)
                    } else {
                        le64(e, 16)
                    },
                    flags: le32(e, 24),
                    bitmap: le32(e, 28),
                };
                if e.iter().all(|&x| x == 0) || d.flags & NOT_USED != 0 {
                    continue;
                }
                // Blocks are whole and inside the volume; anything else is
                // corrupt and ignored (the block reads as it is now).
                let block_ok = |o: u64| o.is_multiple_of(BLOCK) && o < vol_len;
                let data_ok = |o: u64| {
                    o.is_multiple_of(SECTOR) && o.checked_add(BLOCK).is_some_and(|e| e <= vol_len)
                };
                let ok = block_ok(d.original)
                    && if d.flags & FORWARDER != 0 {
                        block_ok(d.target)
                    } else {
                        data_ok(d.target)
                    };
                if ok && total < MAX_DESCRIPTORS {
                    store.descs.push(d);
                    total += 1;
                }
            }
            next = le64(&b, 40);
        }
        // Stable: several descriptors for one block keep their order.
        store.descs.sort_by_key(|d| d.original);
        stores.stores.push(store);
    }
    stores.stores.sort_by_key(|s| s.created);
    stores
}

/// Whether two lists of extents hold the same bytes. Unreadable means
/// not the same, so the shadow copy's version is listed.
fn same_content(src: &dyn Source, a: &[Extent], b: &[Extent]) -> bool {
    let (a, b) = (
        MappedSource::new(src, a.to_vec()),
        MappedSource::new(src, b.to_vec()),
    );
    if a.len() != b.len() {
        return false;
    }
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    let mut pos = 0;
    while pos < a.len() {
        let n = (a.len() - pos).min(x.len() as u64) as usize;
        match (a.read_at(pos, &mut x[..n]), b.read_at(pos, &mut y[..n])) {
            (Ok(i), Ok(j)) if i == n && j == n && x[..n] == y[..n] => pos += n as u64,
            _ => return false,
        }
    }
    true
}

fn filetime(ticks: u64) -> Option<DateTime> {
    DateTime::from_unix((ticks / 10_000_000) as i64 - WINDOWS_EPOCH_OFFSET)
}

/// Files in the NTFS volume's shadow copies whose data the live volume
/// no longer has, newest shadow copy first.
pub(super) fn snapshot_files(src: &dyn Source, vol: &Volume) -> Vec<SnapshotFile> {
    let stores = read_stores(src, vol.offset, vol.len);
    if stores.stores.is_empty() {
        return Vec::new();
    }
    // Files (deleted ones too) whose content is still where the live
    // volume says: a shadow copy's version with the same layout is the
    // same data, since any block changed since would be in the store.
    let live: HashSet<(u64, &[Extent])> = vol
        .files
        .iter()
        .map(|f| (f.size, f.extents.as_slice()))
        .collect();
    let mut by_path: HashMap<(&str, &str), Vec<&FsFile>> = HashMap::new();
    for f in &vol.files {
        by_path.entry((&f.dir, &f.name)).or_default().push(f);
    }
    let mut out = Vec::new();
    for index in (0..stores.stores.len()).rev() {
        let store = &stores.stores[index];
        let len = match store.size {
            s if s > 0 && s <= vol.len => s,
            _ => vol.len,
        };
        let shadow = Shadow {
            src,
            base: vol.offset,
            len,
            stores: &stores,
            index,
        };
        let cached = Cached::new(&shadow);
        let Some(snap) = ntfs::read(&cached, 0, len) else {
            continue;
        };
        let taken = filetime(store.created);
        let label = match taken {
            Some(t) => format!("Shadow copy {t}"),
            None => format!("Shadow copy {}", index + 1),
        };
        // Files already deleted when the copy was taken are what the live
        // volume's own records give; only its files are its own.
        for f in snap.files.into_iter().filter(|f| !f.deleted) {
            let mut extents = Vec::new();
            for e in &f.extents {
                for p in shadow.map(e.offset, e.len) {
                    push_merged(&mut extents, p);
                }
            }
            if extents.is_empty() || live.contains(&(f.size, extents.as_slice())) {
                continue;
            }
            // Blocks are 16 KB and clusters smaller, so a file sharing a
            // block with one overwritten since reads partly from the store
            // though its content has not changed. Compare the content of
            // the live file at the same place.
            let same = by_path
                .get(&(f.dir.as_str(), f.name.as_str()))
                .is_some_and(|l| {
                    l.iter().any(|live| {
                        live.size == f.size && same_content(src, &live.extents, &extents)
                    })
                });
            if same {
                continue;
            }
            out.push(SnapshotFile {
                snapshot: label.clone(),
                taken,
                file: FsFile {
                    extents,
                    overwritten: false,
                    alternatives: Vec::new(),
                    ..f
                },
            });
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const CLUSTER: usize = 4096;
    const RECORD: usize = 1024;
    const MFT_LCN: usize = 4;
    const VOL: usize = 1 << 20;

    fn put16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A $FILE_NAME attribute naming `name` in the root folder.
    fn file_name(name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        put64(&mut v, 0, 5 | (5 << 48)); // parent: the root, record 5
        let t = (1_700_000_000 + WINDOWS_EPOCH_OFFSET as u64) * 10_000_000;
        put64(&mut v, 8, t);
        put64(&mut v, 16, t);
        v[64] = units.len() as u8;
        v[65] = 1; // Win32 name
        for (i, u) in units.iter().enumerate() {
            put16(&mut v, 66 + i * 2, *u);
        }
        let mut a = vec![0u8; (24 + v.len()).div_ceil(8) * 8];
        put32(&mut a, 0, 0x30);
        let n = a.len() as u32;
        put32(&mut a, 4, n);
        put32(&mut a, 16, v.len() as u32);
        put16(&mut a, 20, 24);
        a[24..24 + v.len()].copy_from_slice(&v);
        a
    }

    /// A non-resident $DATA attribute: one run of `clusters` at `lcn`.
    fn data_runs(lcn: u64, clusters: u64, size: u64) -> Vec<u8> {
        let mut a = vec![0u8; 72];
        put32(&mut a, 0, 0x80);
        put32(&mut a, 4, 72);
        a[8] = 1;
        put64(&mut a, 24, clusters - 1);
        put16(&mut a, 32, 64);
        put64(&mut a, 40, clusters * CLUSTER as u64);
        put64(&mut a, 48, size);
        put64(&mut a, 56, size);
        // Header 0x22: two-byte length, two-byte offset.
        a[64..69].copy_from_slice(&[
            0x22,
            clusters as u8,
            (clusters >> 8) as u8,
            lcn as u8,
            (lcn >> 8) as u8,
        ]);
        a
    }

    /// A resident $DATA attribute holding `data`.
    fn data_resident(data: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; (24 + data.len()).div_ceil(8) * 8];
        put32(&mut a, 0, 0x80);
        let n = a.len() as u32;
        put32(&mut a, 4, n);
        put32(&mut a, 16, data.len() as u32);
        put16(&mut a, 20, 24);
        a[24..24 + data.len()].copy_from_slice(data);
        a
    }

    /// An MFT record with the given attributes, its fixups applied as
    /// NTFS writes them.
    fn record(attrs: &[Vec<u8>], in_use: bool) -> Vec<u8> {
        let mut r = vec![0u8; RECORD];
        r[..4].copy_from_slice(b"FILE");
        put16(&mut r, 4, 48);
        put16(&mut r, 6, 3);
        put16(&mut r, 20, 56);
        put16(&mut r, 22, u16::from(in_use));
        let mut pos = 56;
        for a in attrs {
            r[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        put32(&mut r, pos, 0xFFFF_FFFF);
        put32(&mut r, 24, (pos + 8) as u32);
        put32(&mut r, 28, RECORD as u32);
        put16(&mut r, 48, 7);
        for i in 1..=2 {
            let end = i * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[48 + i * 2] = a;
            r[49 + i * 2] = b;
            put16(&mut r, end - 2, 7);
        }
        r
    }

    fn put_record(vol: &mut [u8], n: usize, rec: &[u8]) {
        let at = MFT_LCN * CLUSTER + n * RECORD;
        vol[at..at + RECORD].copy_from_slice(rec);
    }

    pub(crate) fn content(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32 * 31 + u32::from(seed) * 7) as u8)
            .collect()
    }

    /// A small NTFS volume: an MFT of 32 records at cluster 4, files from
    /// record 16 on.
    fn ntfs(files: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut vol = vec![0u8; VOL];
        vol[3..11].copy_from_slice(b"NTFS    ");
        put16(&mut vol, 11, 512);
        vol[13] = 8;
        put64(&mut vol, 40, (VOL / 512) as u64);
        put64(&mut vol, 48, MFT_LCN as u64);
        put64(&mut vol, 56, 200);
        vol[64] = 0xF6; // 2^10-byte records
        let mft = record(&[file_name("$MFT"), data_runs(4, 8, 32 * 1024)], true);
        put_record(&mut vol, 0, &mft);
        for (n, rec) in files {
            put_record(&mut vol, *n, rec);
        }
        vol
    }

    /// The volume as it was (old.txt, note.txt and keep.txt), the live
    /// volume after old.txt and note.txt were deleted and their record
    /// and clusters reused, and a shadow copy store holding the blocks
    /// that changed.
    pub(crate) fn shadowed() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let old = content(10_000, 1);
        let keep = content(4_000, 2);
        let note = b"A note small enough to live in its MFT record.".to_vec();
        let mut before = ntfs(&[
            (
                16,
                record(&[file_name("old.txt"), data_runs(32, 3, 10_000)], true),
            ),
            (
                17,
                record(&[file_name("keep.txt"), data_runs(35, 1, 4_000)], true),
            ),
            (
                18,
                record(&[file_name("note.txt"), data_resident(&note)], true),
            ),
        ]);
        before[32 * CLUSTER..32 * CLUSTER + old.len()].copy_from_slice(&old);
        // keep.txt shares old.txt's 16 KB block, which the store holds.
        before[35 * CLUSTER..35 * CLUSTER + keep.len()].copy_from_slice(&keep);

        let mut live = before.clone();
        put_record(
            &mut live,
            16,
            &record(&[file_name("new.txt"), data_runs(32, 1, 100)], true),
        );
        put_record(&mut live, 18, &record(&[file_name("note.txt")], false));
        live[32 * CLUSTER..35 * CLUSTER].fill(0xEE);

        // Volume header, catalog at 0x28000, block list at 0x2C000, store
        // data at 0x50000: the MFT block holding records 16 to 31
        // (0x8000) and the block holding old.txt's data (0x20000).
        let h = HEADER_AT as usize;
        live[h..h + 16].copy_from_slice(&VSS_ID);
        put32(&mut live, h + 16, 1);
        put32(&mut live, h + 20, REC_HEADER);
        put64(&mut live, h + 24, HEADER_AT);
        put64(&mut live, h + 48, 0x28000);
        let store_id = [9u8; 16];
        let c = 0x28000;
        live[c..c + 16].copy_from_slice(&VSS_ID);
        put32(&mut live, c + 16, 1);
        put32(&mut live, c + 20, REC_CATALOG);
        put64(&mut live, c + 32, c as u64);
        let e = c + 128;
        put64(&mut live, e, 2);
        put64(&mut live, e + 8, VOL as u64);
        live[e + 16..e + 32].copy_from_slice(&store_id);
        put64(
            &mut live,
            e + 48,
            (1_710_000_000 + WINDOWS_EPOCH_OFFSET as u64) * 10_000_000,
        );
        let e = c + 256;
        put64(&mut live, e, 3);
        put64(&mut live, e + 8, 0x2C000);
        live[e + 16..e + 32].copy_from_slice(&store_id);
        let l = 0x2C000;
        live[l..l + 16].copy_from_slice(&VSS_ID);
        put32(&mut live, l + 16, 1);
        put32(&mut live, l + 20, REC_BLOCK_LIST);
        for (i, (orig, store)) in [(0x8000usize, 0x50000usize), (0x20000, 0x54000)]
            .into_iter()
            .enumerate()
        {
            let d = l + 128 + i * 32;
            put64(&mut live, d, orig as u64);
            put64(&mut live, d + 16, store as u64);
            let block = before[orig..orig + BLOCK as usize].to_vec();
            live[store..store + BLOCK as usize].copy_from_slice(&block);
        }
        (before, live, [old, note].concat())
    }

    fn read_all(src: &dyn Source, extents: &[Extent]) -> Vec<u8> {
        let view = MappedSource::new(src, extents.to_vec());
        let mut buf = vec![0u8; view.len() as usize];
        view.read_at(0, &mut buf).unwrap();
        buf
    }

    #[test]
    fn shadow_copy_reads_as_the_volume_was() {
        let (before, live, _) = shadowed();
        let stores = read_stores(&live, 0, live.len() as u64);
        assert_eq!(stores.stores.len(), 1);
        let shadow = Shadow {
            src: &live,
            base: 0,
            len: live.len() as u64,
            stores: &stores,
            index: 0,
        };
        let mut got = vec![0u8; before.len()];
        assert_eq!(shadow.read_at(0, &mut got).unwrap(), before.len());
        // The changed blocks come back as they were; the rest (including
        // the shadow copy's own structures) reads as the volume is now.
        for (from, to) in [(0x8000, 0xC000), (0x20000, 0x24000)] {
            assert!(got[from..to] == before[from..to]);
            assert!(got[from..to] != live[from..to]);
        }
        assert!(got[0x24000..] == live[0x24000..]);
        assert!(got[..0x8000] == live[..0x8000]);
    }

    #[test]
    fn finds_files_deleted_since_the_shadow_copy() {
        let (_, live, expected) = shadowed();
        let vol = ntfs::read(&live, 0, live.len() as u64).unwrap();
        let found = snapshot_files(&live, &vol);
        let names: Vec<&str> = found.iter().map(|f| f.file.name.as_str()).collect();
        assert_eq!(names, ["old.txt", "note.txt"], "{found:?}");
        let old = &found[0].file;
        assert_eq!(read_all(&live, &old.extents), expected[..10_000]);
        let note = &found[1].file;
        assert_eq!(read_all(&live, &note.extents), expected[10_000..]);
        assert!(found[0].snapshot.starts_with("Shadow copy 2024-03-09"));
        // The data comes from the store, not from where it was.
        assert!(old.extents.iter().all(|e| e.offset >= 0x50000));
    }

    /// The same volume at a partition offset, through the file system
    /// entry point.
    #[test]
    fn found_through_open_at_an_offset() {
        let (_, live, expected) = shadowed();
        let mut disk = vec![0u8; 1 << 16];
        disk.extend(&live);
        let vol = super::super::open(&disk, 1 << 16, live.len() as u64).unwrap();
        assert_eq!(vol.snapshots.len(), 2);
        let old = &vol.snapshots[0].file;
        assert_eq!(read_all(&disk, &old.extents), expected[..10_000]);
    }

    #[test]
    fn overlays_and_forwarders() {
        let stores = Stores {
            stores: vec![
                Store {
                    descs: vec![
                        Desc {
                            original: 0,
                            target: 0x10000,
                            flags: FORWARDER,
                            bitmap: 0,
                        },
                        Desc {
                            original: 0,
                            target: 0x80000,
                            flags: OVERLAY,
                            bitmap: 0b10,
                        },
                    ],
                    ..Default::default()
                },
                Store {
                    descs: vec![Desc {
                        original: 0x10000,
                        target: 0x90000,
                        flags: 0,
                        bitmap: 0,
                    }],
                    ..Default::default()
                },
            ],
        };
        let s = stores.sectors(0, 0);
        assert_eq!(s[0], 0x90000);
        assert_eq!(s[1], 0x80000 + 512);
        assert_eq!(s[2], 0x90000 + 1024);
        // The newer copy only has the block moved, not the overlay.
        assert_eq!(stores.sectors(0, 1)[1], 512);
        assert_eq!(stores.sectors(0x10000, 1)[0], 0x90000);
    }

    /// Corrupt headers, catalogs and lists: nothing panics or loops, and
    /// whatever is found reads inside the volume.
    #[test]
    fn corrupt_stores_do_not_panic() {
        let (_, live, _) = shadowed();
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let spots = [HEADER_AT as usize, 0x28000, 0x2C000];
        for round in 0..300 {
            let mut d = live.clone();
            for _ in 0..1 + round % 8 {
                let base = spots[(next() % 3) as usize];
                let at = base + (next() % 1024) as usize;
                let v = next();
                match next() % 3 {
                    0 => d[at] = v as u8,
                    1 => put64(&mut d, at, v),
                    _ => put64(&mut d, at, v % (1 << 21)),
                }
            }
            // Loops: a list or catalog pointing at itself.
            if round % 10 == 0 {
                put64(&mut d, 0x2C000 + 40, 0x2C000);
                put64(&mut d, 0x28000 + 40, 0x28000);
            }
            let len = d.len() as u64;
            if let Some(vol) = ntfs::read(&d, 0, len) {
                for f in snapshot_files(&d, &vol) {
                    assert!(f.file.extents.iter().all(|e| e.end() <= len));
                }
            }
        }
    }
}

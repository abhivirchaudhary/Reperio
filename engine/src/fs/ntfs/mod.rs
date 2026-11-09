//! NTFS, used by Windows and by many external hard drives and large USB
//! sticks.
//!
//! Every file has a record in the master file table (MFT). Deleting a file
//! clears the record's "in use" flag and frees its clusters in `$Bitmap`,
//! but leaves the record itself, with the file's names, timestamps, parent
//! folder and the exact list of clusters holding its data, until the record
//! is reused. That makes deleted NTFS files very recoverable: names,
//! folders and fragmented layouts all come back exactly.
//!
//! Once a deleted file's record has been reused, three other places still
//! know about it (see [`deep`]): the journal ($LogFile, [`logfile`]), the
//! change journal ($UsnJrnl, [`usn`]) and old entries in folder indexes
//! ([`index`]).

mod deep;
mod index;
mod logfile;
mod usn;

use std::collections::HashMap;

use super::{FsFile, FsType, Volume};
use crate::extent::{push_merged, Extent};
use crate::formats::DateTime;
use crate::source::{read_tolerant, Source};

const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_VOLUME_NAME: u32 = 0x60;
const ATTR_INDEX_ROOT: u32 = 0x90;
const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_END: u32 = 0xFFFF_FFFF;
const IN_USE: u16 = 0x01;
const DIRECTORY: u16 = 0x02;
const ROOT: u64 = 5;
/// A file reference is a record number (low 48 bits) and the record's
/// sequence count (high 16 bits), which goes up each time it is reused.
const REF_MASK: u64 = 0xFFFF_FFFF_FFFF;
/// Records 0 to 15 are the filesystem's own metadata files.
const FIRST_USER_RECORD: u64 = 16;
/// Most records read; bounds the work on a corrupt or huge MFT.
const MAX_RECORDS: u64 = 50_000_000;
/// Bytes of the MFT read at a time.
const MFT_BATCH: u64 = 4 << 20;
/// Seconds between 1601-01-01 (Windows time) and 1970-01-01.
const WINDOWS_EPOCH_OFFSET: i64 = 11_644_473_600;

struct Boot {
    cluster: u64,
    record_size: u64,
    mft_lcn: u64,
    /// $MFTMirr: copies of the MFT's first records, for when they are
    /// unreadable in the MFT itself.
    mirror_lcn: u64,
    total_clusters: u64,
}

impl Boot {
    fn parse(b: &[u8]) -> Option<Self> {
        if &b[3..11] != b"NTFS    " {
            return None;
        }
        let sector = u64::from(u16::from_le_bytes([b[11], b[12]]));
        let spc = b[13];
        // Both sizes may be a negative power of two; a corrupt one could
        // shift or multiply past 64 bits, which is just invalid.
        let spc = if spc > 0x80 {
            1u64.checked_shl(256 - u32::from(spc))?
        } else {
            u64::from(spc)
        };
        let cluster = sector.checked_mul(spc)?;
        let per_record = b[64] as i8;
        let record_size = if per_record < 0 {
            1u64.checked_shl((-i32::from(per_record)) as u32)?
        } else {
            (per_record as u64).checked_mul(cluster)?
        };
        let total_sectors = u64::from_le_bytes(b[40..48].try_into().unwrap());
        let mft_lcn = u64::from_le_bytes(b[48..56].try_into().unwrap());
        let mirror_lcn = u64::from_le_bytes(b[56..64].try_into().unwrap());
        let valid = matches!(sector, 512 | 1024 | 2048 | 4096)
            && cluster.is_power_of_two()
            && cluster <= 2 << 20
            && (256..=65536).contains(&record_size)
            && record_size % sector == 0;
        // `then`, not `then_some`: the division must not run for an
        // invalid boot sector, whose cluster size may be zero.
        valid.then(|| Boot {
            cluster,
            record_size,
            mft_lcn,
            mirror_lcn,
            total_clusters: total_sectors.saturating_mul(sector) / cluster,
        })
    }
}

/// A data attribute's runs: (first cluster, or `None` if sparse; length).
type Runs = Vec<(Option<u64>, u64)>;

/// One MFT record's useful contents.
#[derive(Default)]
struct Record {
    in_use: bool,
    directory: bool,
    /// For an extension record, the record it extends.
    base: Option<u64>,
    /// The record's sequence count; references to it carry the same one.
    seq: u16,
    /// (name, parent reference, namespace), the best name chosen later.
    names: Vec<(String, u64, u8)>,
    modified: Option<DateTime>,
    created: Option<DateTime>,
    size: Option<u64>,
    /// Unnamed data attribute pieces: (first VCN, runs of (LCN, clusters)).
    data: Vec<(u64, Runs)>,
    /// Small files are stored inside the record itself.
    /// Where a small file's data sits inside its record, as pieces of the
    /// raw record (see [`resident_extents`]).
    resident: Option<Vec<Extent>>,
    /// The same, as (offset in the record, length).
    resident_at: Option<(u64, u64)>,
    /// Compressed, encrypted or sparse data cannot be copied out raw.
    unrecoverable: bool,
    /// The `$J` stream, for the change journal's record.
    journal: Vec<(u64, Runs)>,
    journal_size: Option<u64>,
    /// A folder's index: its root, and where its INDX blocks are.
    index_root: Option<Vec<u8>>,
    index_blocks: Vec<(u64, Runs)>,
}

/// Reads an NTFS volume. Only a boot sector is required (the first, or the
/// backup at the volume's end): everything else is read as far as it can
/// be, and what is missing makes the result smaller, never absent.
pub fn read(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    let boot_at = |at: u64| {
        let mut b = [0u8; 512];
        (src.read_at(at, &mut b).ok()? == 512).then_some(())?;
        Boot::parse(&b)
    };
    let boot = boot_at(offset).or_else(|| boot_at(offset + len.checked_sub(512)?))?;
    let cluster = boot.cluster;
    // Cluster numbers come from the boot sector and run lists. One past the
    // end of the source just fails to read; it must not overflow.
    let lcn_offset = |lcn: u64| offset.saturating_add(lcn.saturating_mul(cluster));
    // The boot sector's size can be anything; free space ends with the
    // volume, and with the source if the image is cut short.
    let total_clusters = boot
        .total_clusters
        .min(len.min(src.len().saturating_sub(offset)).div_ceil(cluster));

    let (records, label) = read_mft(src, &boot, &lcn_offset);
    let mut records = records;

    // Fold extension records into their base records: a file with many
    // fragments or names has its attributes spread over several records.
    let extensions: Vec<(u64, u64)> = records
        .iter()
        .filter_map(|(&n, r)| r.base.map(|b| (n, b)))
        .collect();
    for (n, base) in extensions {
        if let Some(ext) = records.remove(&n) {
            if let Some(b) = records.get_mut(&base) {
                b.data.extend(ext.data);
                b.size = b.size.or(ext.size);
                if b.resident.is_none() {
                    b.resident = ext.resident;
                    b.resident_at = ext.resident_at;
                }
                b.names.extend(ext.names);
                b.unrecoverable |= ext.unrecoverable;
                b.created = b.created.or(ext.created);
                b.modified = b.modified.or(ext.modified);
                b.journal.extend(ext.journal);
                b.journal_size = b.journal_size.or(ext.journal_size);
                b.index_root = b.index_root.take().or(ext.index_root);
                b.index_blocks.extend(ext.index_blocks);
            }
        }
    }

    // Free space, from $Bitmap (record 6). Without it (damaged, or parts
    // unreadable), clusters in use are those that files still on the
    // volume say they hold.
    let bitmap = records
        .get(&6)
        .and_then(|b| read_stream(src, b, cluster, &lcn_offset))
        .filter(|bm| bm.len() as u64 * 8 >= total_clusters)
        .unwrap_or_else(|| bitmap_from_records(&records, total_clusters, cluster, &lcn_offset));
    let in_use = |lcn: u64| {
        bitmap
            .get((lcn / 8) as usize)
            .is_some_and(|b| b & (1 << (lcn % 8)) != 0)
    };

    // What the journals and folder indexes still say, and the names of
    // deleted folders they hold, for paths.
    let ctx = deep::Context {
        src,
        cluster,
        record_size: boot.record_size,
        total_clusters,
        lcn_offset: &lcn_offset,
        in_use: &in_use,
    };
    let found = deep::gather(&ctx, &records);
    let folders = deep::folder_names(&records, &found);
    let mut paths = Paths::new(&records, &folders);

    let mut numbers: Vec<u64> = records
        .keys()
        .copied()
        .filter(|&n| n >= FIRST_USER_RECORD)
        .collect();
    numbers.sort_unstable();
    let mut files = Vec::new();
    for n in numbers {
        let rec = &records[&n];
        if rec.directory || rec.unrecoverable {
            continue;
        }
        let Some((name, parent)) = name_of(rec) else {
            continue;
        };
        let Some(size) = rec.size else { continue };
        let extents = if let Some(pieces) = &rec.resident {
            pieces.clone()
        } else {
            mapped_extents(&rec.data, size, cluster, &lcn_offset)
        };
        let size = if rec.resident.is_some() {
            size
        } else {
            extents.iter().map(|e| e.len).sum::<u64>().min(size)
        };
        if size == 0 && rec.size != Some(0) {
            continue;
        }
        let deleted = !rec.in_use;
        // A deleted file's clusters back in use belong to something newer.
        let overwritten = deleted
            && rec.resident.is_none()
            && collect_runs(&rec.data).iter().any(|&(lcn, n)| {
                // Only clusters the bitmap covers can be in use; a corrupt
                // run length would otherwise mean counting to 2^64.
                let bits = bitmap.len() as u64 * 8;
                lcn.is_some_and(|l| (l..l.saturating_add(n).min(bits)).any(&in_use))
            });
        let dir = paths.dir(parent);
        files.push(FsFile {
            dir,
            name,
            size,
            modified: rec.modified,
            created: rec.created,
            deleted,
            extents,
            overwritten,
            alternatives: Vec::new(),
        });
    }

    let deep = deep::recover(&ctx, &records, &found, &mut paths, &files);
    files.extend(deep.files);

    let free = {
        let mut out = Vec::new();
        for lcn in 0..total_clusters {
            if !in_use(lcn) {
                push_merged(&mut out, Extent::new(lcn_offset(lcn), cluster));
            }
        }
        out
    };
    Some(Volume {
        fs: FsType::Ntfs,
        label,
        offset,
        len,
        cluster_size: cluster,
        files,
        free,
        apfs: None,
        names: deep.names,
        name_sources: deep.sources,
        snapshots: Vec::new(),
    })
}

/// A record's best name: Win32 first, then POSIX, DOS 8.3 short names
/// last. With its parent folder's reference.
fn name_of(rec: &Record) -> Option<(String, u64)> {
    rec.names
        .iter()
        .min_by_key(|(_, _, ns)| match ns {
            1 | 3 => 0,
            0 => 1,
            _ => 2,
        })
        .map(|(n, p, _)| (n.clone(), *p))
}

/// As much of a non-resident stream as its records still map, cut to
/// `size`. A file whose last pieces were in a lost extension record comes
/// back short rather than not at all.
fn mapped_extents(
    data: &[(u64, Runs)],
    size: u64,
    cluster: u64,
    lcn_offset: &dyn Fn(u64) -> u64,
) -> Vec<Extent> {
    let mut out = Vec::new();
    let mut at = 0;
    for (vpos, disk, len) in stream_map(data, cluster, lcn_offset) {
        if vpos != at || at >= size {
            break;
        }
        let n = len.min(size - at);
        push_merged(&mut out, Extent::new(disk, n));
        at += n;
    }
    out
}

/// Names of folders that are no longer in the file table, by record
/// number: (sequence count, name, parent reference).
type FolderNames = HashMap<u64, Vec<(u16, String, u64)>>;

/// Folder paths, following parent references up to the root.
struct Paths<'a> {
    records: &'a HashMap<u64, Record>,
    /// Deleted folders whose records were reused, named by the journals
    /// and old index entries.
    folders: &'a FolderNames,
    known: HashMap<u64, Option<String>>,
}

impl<'a> Paths<'a> {
    fn new(records: &'a HashMap<u64, Record>, folders: &'a FolderNames) -> Self {
        Self {
            records,
            folders,
            known: HashMap::new(),
        }
    }

    /// The folder a reference points at, and its parent's reference. The
    /// file table's record is used if it is still that folder (same
    /// sequence count, or one more once deleted); failing that, a name
    /// learned elsewhere for that exact reference; failing that, the
    /// record anyway, as the best guess there is.
    fn step(&self, r: u64) -> Option<(String, u64)> {
        let (num, seq) = (r & REF_MASK, (r >> 48) as u16);
        let current = self
            .records
            .get(&num)
            .filter(|rec| rec.directory)
            .and_then(|rec| Some((rec, name_of(rec)?)));
        if let Some((rec, named)) = &current {
            let same =
                seq == 0 || rec.seq == seq || (!rec.in_use && rec.seq == seq.wrapping_add(1));
            if same {
                return Some(named.clone());
            }
        }
        if let Some((_, name, parent)) = self
            .folders
            .get(&num)
            .and_then(|v| v.iter().find(|(s, _, _)| *s == seq))
        {
            return Some((name.clone(), *parent));
        }
        current.map(|(_, named)| named)
    }

    fn dir(&mut self, dir: u64) -> String {
        let mut chain = Vec::new();
        let mut cur = dir;
        let mut ok = false;
        for _ in 0..64 {
            if cur & REF_MASK == ROOT {
                ok = true;
                break;
            }
            if let Some(known) = self.known.get(&cur) {
                if let Some(p) = known {
                    chain.push(p.clone());
                    ok = true;
                }
                break;
            }
            match self.step(cur) {
                Some((name, parent)) => {
                    chain.push(name);
                    cur = parent;
                }
                None => break,
            }
        }
        chain.reverse();
        let path = if ok {
            chain.join("/")
        } else {
            format!("Lost folders/{}", chain.join("/"))
        };
        self.known
            .insert(dir, if ok { Some(path.clone()) } else { None });
        path.trim_end_matches('/').to_string()
    }
}

/// Reads every MFT record that can be found, by number, and the volume's
/// name. The MFT's own layout is in record 0 and, once it is fragmented
/// enough, in extension records of record 0 elsewhere in the MFT; those are
/// followed until no new piece turns up. If record 0 is unreadable, its
/// copy in $MFTMirr is used; if neither describes the MFT, it is read on
/// from where the boot sector says it starts for as long as records are
/// found there.
fn read_mft(
    src: &dyn Source,
    boot: &Boot,
    lcn_offset: &dyn Fn(u64) -> u64,
) -> (HashMap<u64, Record>, Option<String>) {
    let size = boot.record_size;
    let mut records: HashMap<u64, Record> = HashMap::new();
    let mut label = None;
    let record0 = [boot.mft_lcn, boot.mirror_lcn].into_iter().find_map(|lcn| {
        let at = lcn_offset(lcn);
        parse_record(&read_record(src, at, size)?, 0, at)
    });
    let mut pieces: Vec<(u64, Runs)> = record0.map(|r| r.data).unwrap_or_default();
    // Stretches of the MFT read so far, by position in it.
    let mut done: Vec<(u64, u64)> = Vec::new();
    let read_part = |vpos: u64,
                     disk: u64,
                     len: u64,
                     records: &mut HashMap<u64, Record>,
                     label: &mut Option<String>|
     -> bool {
        // Records are read many at a time: one read per record means
        // hundreds of thousands of reads on a Windows drive, and minutes
        // through a raw USB device.
        let mut found_any = false;
        let mut at = 0;
        while at + size <= len {
            let first = (vpos + at) / size;
            if first >= MAX_RECORDS {
                break;
            }
            let n = ((len - at) / size).min(MFT_BATCH / size).max(1);
            let mut batch = vec![0u8; (n * size) as usize];
            let (got, _) = read_tolerant(src, disk + at, &mut batch);
            for (i, raw) in batch[..got].chunks_exact(size as usize).enumerate() {
                let number = first + i as u64;
                let rec_at = disk + at + i as u64 * size;
                if let Some(raw) = fix_record(raw.to_vec()) {
                    if number == 3 {
                        *label = volume_name(&raw);
                    }
                    if let Some(rec) = parse_record(&raw, number, rec_at) {
                        found_any = true;
                        records.insert(number, rec);
                    }
                }
            }
            if got < batch.len() {
                break;
            }
            at += n * size;
        }
        found_any
    };
    for _ in 0..64 {
        let mut new = false;
        for (vpos, disk, len) in stream_map(&pieces, boot.cluster, lcn_offset) {
            if done.iter().any(|&(s, e)| s <= vpos && vpos + len <= e) {
                continue;
            }
            read_part(vpos, disk, len, &mut records, &mut label);
            done.push((vpos, vpos + len));
            new = true;
        }
        if !new {
            break;
        }
        // Pieces of the MFT's data in record 0's extension records.
        for rec in records.values().filter(|r| r.base == Some(0)) {
            for piece in &rec.data {
                if !pieces.iter().any(|p| p.0 == piece.0) {
                    pieces.push(piece.clone());
                }
            }
        }
    }
    if records.len() <= 1 {
        // No usable description of the MFT: read on from its start in
        // stretches for as long as they hold records.
        let stretch = MFT_BATCH;
        let mut vpos = 0;
        while vpos / size < MAX_RECORDS
            && read_part(
                vpos,
                lcn_offset(boot.mft_lcn) + vpos,
                stretch,
                &mut records,
                &mut label,
            )
        {
            vpos += stretch;
        }
    }
    (records, label)
}

/// Where a non-resident attribute's bytes are: (position in the stream,
/// position on disk, length), for the parts its runs map. Sparse runs and
/// pieces that were in records now lost leave gaps.
fn stream_map(
    pieces: &[(u64, Runs)],
    cluster: u64,
    lcn_offset: &dyn Fn(u64) -> u64,
) -> Vec<(u64, u64, u64)> {
    let mut sorted: Vec<&(u64, Runs)> = pieces.iter().collect();
    sorted.sort_by_key(|(vcn, _)| *vcn);
    let mut out: Vec<(u64, u64, u64)> = Vec::new();
    for (vcn, runs) in sorted {
        let mut vpos = vcn.saturating_mul(cluster);
        for &(lcn, clusters) in runs {
            let len = clusters.saturating_mul(cluster);
            if let Some(lcn) = lcn {
                let disk = lcn_offset(lcn);
                match out.last_mut() {
                    Some(last) if last.0 + last.2 == vpos && last.1 + last.2 == disk => {
                        last.2 += len
                    }
                    _ => out.push((vpos, disk, len)),
                }
            }
            vpos = vpos.saturating_add(len);
        }
    }
    out
}

/// A small system file's contents ($Bitmap), resident or not, as far as it
/// can be read. `None` if any part is missing.
fn read_stream(
    src: &dyn Source,
    rec: &Record,
    cluster: u64,
    lcn_offset: &dyn Fn(u64) -> u64,
) -> Option<Vec<u8>> {
    let size = rec.size?;
    // $Bitmap is an eighth of a cluster per cluster: 32 MB for a 1 TB
    // volume at 4 KB clusters. Anything far bigger is a corrupt size.
    if size > 1 << 31 {
        return None;
    }
    let mut out = vec![0u8; size as usize];
    let parts: Vec<(u64, u64, u64)> = match &rec.resident {
        Some(pieces) => {
            let mut at = 0;
            pieces
                .iter()
                .map(|e| {
                    let p = (at, e.offset, e.len);
                    at += e.len;
                    p
                })
                .collect()
        }
        None => stream_map(&rec.data, cluster, lcn_offset),
    };
    let mut covered = 0;
    for (vpos, disk, len) in parts {
        if vpos >= size {
            continue;
        }
        let n = len.min(size - vpos) as usize;
        let (got, bad) = read_tolerant(src, disk, &mut out[vpos as usize..vpos as usize + n]);
        if got < n || bad > 0 {
            return None;
        }
        covered += n as u64;
    }
    (covered >= size).then_some(out)
}

/// A cluster bitmap from what files still on the volume hold, for when
/// $Bitmap cannot be read. System files ($MFT, $LogFile and the rest) are
/// files too, so the volume's own structures count as in use.
fn bitmap_from_records(
    records: &HashMap<u64, Record>,
    total_clusters: u64,
    cluster: u64,
    lcn_offset: &dyn Fn(u64) -> u64,
) -> Vec<u8> {
    let mut bm = vec![0u8; total_clusters.div_ceil(8) as usize];
    let base = lcn_offset(0);
    for rec in records.values().filter(|r| r.in_use) {
        for (_, disk, len) in stream_map(&rec.data, cluster, lcn_offset) {
            let first = (disk - base) / cluster;
            let last = (first + len.div_ceil(cluster)).min(total_clusters);
            for lcn in first..last {
                bm[(lcn / 8) as usize] |= 1 << (lcn % 8);
            }
        }
    }
    // Directories' index blocks are not followed, so their clusters count
    // as free: the deep scan searches them too, which costs only time.
    bm
}

/// Reads one record and applies its update sequence fixups: the last two
/// bytes of every sector were swapped out on write to detect torn writes.
fn read_record(src: &dyn Source, at: u64, size: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; size as usize];
    if src.read_at(at, &mut buf).ok()? < buf.len() {
        return None;
    }
    fix_record(buf)
}

/// Applies a record's update sequence fixups (see [`read_record`]).
fn fix_record(buf: Vec<u8>) -> Option<Vec<u8>> {
    fix_block(buf, b"FILE")
}

/// Applies the update sequence fixups of any NTFS block that has them:
/// file records, index blocks and the journal's pages.
fn fix_block(mut buf: Vec<u8>, magic: &[u8; 4]) -> Option<Vec<u8>> {
    fix_in_place(&mut buf, magic).then_some(buf)
}

/// [`fix_block`] on a borrowed block. On failure the block may be left
/// partly fixed.
fn fix_in_place(buf: &mut [u8], magic: &[u8; 4]) -> bool {
    if buf.len() < 8 || &buf[..4] != magic {
        return false;
    }
    let usa = usize::from(u16::from_le_bytes([buf[4], buf[5]]));
    let count = usize::from(u16::from_le_bytes([buf[6], buf[7]]));
    if count < 2 || usa + count * 2 > buf.len() {
        return false;
    }
    let sector = buf.len() / (count - 1);
    if sector < 2 {
        return false;
    }
    let usn = [buf[usa], buf[usa + 1]];
    for i in 1..count {
        let end = i * sector;
        if end > buf.len() || buf[end - 2..end] != usn {
            return false;
        }
        buf[end - 2] = buf[usa + i * 2];
        buf[end - 1] = buf[usa + i * 2 + 1];
    }
    true
}

fn parse_record(b: &[u8], number: u64, at: u64) -> Option<Record> {
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let flags = u16_at(22);
    // The base record's reference: its number and a sequence count. A
    // reference of all zeros means none; an extension of record 0 (the
    // MFT's own) has number 0 and a non-zero sequence count.
    let base_ref = u64::from_le_bytes(b[32..40].try_into().unwrap());
    let base = base_ref & 0xFFFF_FFFF_FFFF;
    let mut rec = Record {
        in_use: flags & IN_USE != 0,
        directory: flags & DIRECTORY != 0,
        seq: u16_at(16),
        base: (base_ref != 0 && base != number).then_some(base),
        ..Default::default()
    };
    let mut pos = usize::from(u16_at(20));
    let used = (u32::from_le_bytes(b[24..28].try_into().unwrap()) as usize).min(b.len());
    for _ in 0..64 {
        if pos + 16 > used {
            break;
        }
        let typ = u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap());
        if typ == ATTR_END {
            break;
        }
        let alen = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if alen < 16 || pos + alen > used {
            break;
        }
        let a = &b[pos..pos + alen];
        let non_resident = a[8] != 0;
        // Resident and non-resident headers are 24 and 64 bytes long; a
        // shorter attribute is corrupt and its fields cannot be read.
        if alen < if non_resident { 64 } else { 24 } {
            pos += alen;
            continue;
        }
        let named = a[9] != 0;
        let aflags = u16::from_le_bytes([a[12], a[13]]);
        match typ {
            ATTR_FILE_NAME if !non_resident => {
                let off = usize::from(u16::from_le_bytes([a[20], a[21]]));
                if let Some(v) = a.get(off..) {
                    if v.len() >= 66 {
                        let parent = u64::from_le_bytes(v[..8].try_into().unwrap());
                        let n = usize::from(v[64]);
                        let units: Vec<u16> = v[66..]
                            .chunks(2)
                            .take(n)
                            .map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                            .collect();
                        rec.names
                            .push((String::from_utf16_lossy(&units), parent, v[65]));
                        rec.created = rec.created.or_else(|| filetime(&v[8..16]));
                        rec.modified = rec.modified.or_else(|| filetime(&v[16..24]));
                    }
                }
            }
            ATTR_DATA if !named => {
                if aflags & 0x4001 != 0 {
                    rec.unrecoverable = true; // compressed or encrypted
                }
                if non_resident {
                    let start_vcn = u64::from_le_bytes(a[16..24].try_into().unwrap());
                    if start_vcn == 0 {
                        rec.size = Some(u64::from_le_bytes(a[48..56].try_into().unwrap()));
                    }
                    let runs_at = usize::from(u16::from_le_bytes([a[32], a[33]]));
                    let Some(runs) = a.get(runs_at..).and_then(parse_runs) else {
                        rec.unrecoverable = true;
                        pos += alen;
                        continue;
                    };
                    if runs.iter().any(|(lcn, _)| lcn.is_none()) {
                        rec.unrecoverable = true; // sparse
                    }
                    rec.data.push((start_vcn, runs));
                } else {
                    let vlen = u32::from_le_bytes(a[16..20].try_into().unwrap()) as u64;
                    let voff = u64::from(u16::from_le_bytes([a[20], a[21]]));
                    let start = pos as u64 + voff;
                    rec.size = Some(vlen);
                    rec.resident = Some(resident_extents(b, at, start, vlen));
                    rec.resident_at = Some((start, vlen));
                }
            }
            // The change journal's stream, and folder indexes: kept for
            // finding deleted files once their records are reused.
            ATTR_DATA if non_resident && attr_name(a).as_deref() == Some("$J") => {
                let start_vcn = u64::from_le_bytes(a[16..24].try_into().unwrap());
                if start_vcn == 0 {
                    rec.journal_size = Some(u64::from_le_bytes(a[48..56].try_into().unwrap()));
                }
                let runs_at = usize::from(u16::from_le_bytes([a[32], a[33]]));
                if let Some(runs) = a.get(runs_at..).and_then(parse_runs) {
                    rec.journal.push((start_vcn, runs));
                }
            }
            ATTR_INDEX_ROOT if !non_resident && attr_name(a).as_deref() == Some("$I30") => {
                let vlen = u32::from_le_bytes(a[16..20].try_into().unwrap()) as usize;
                let voff = usize::from(u16::from_le_bytes([a[20], a[21]]));
                rec.index_root = a.get(voff..voff.saturating_add(vlen)).map(<[u8]>::to_vec);
            }
            ATTR_INDEX_ALLOCATION if non_resident && attr_name(a).as_deref() == Some("$I30") => {
                let start_vcn = u64::from_le_bytes(a[16..24].try_into().unwrap());
                let runs_at = usize::from(u16::from_le_bytes([a[32], a[33]]));
                if let Some(runs) = a.get(runs_at..).and_then(parse_runs) {
                    rec.index_blocks.push((start_vcn, runs));
                }
            }
            _ => {}
        }
        pos += alen;
    }
    Some(rec)
}

/// An attribute's name, if it has one.
fn attr_name(a: &[u8]) -> Option<String> {
    let n = usize::from(a[9]);
    let at = usize::from(u16::from_le_bytes([a[10], a[11]]));
    let units: Vec<u16> = a
        .get(at..at + n * 2)?
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    (n > 0).then(|| String::from_utf16_lossy(&units))
}

/// Where on disk the `len` bytes at `start` of the record at `at` really
/// are. The last two bytes of each 512-byte sector of a record were
/// swapped on write for a check value, and the real ones kept in the
/// update sequence array near the record's start: data running over one
/// of those spots takes those two bytes from the array.
fn resident_extents(record: &[u8], at: u64, start: u64, len: u64) -> Vec<Extent> {
    let usa = u64::from(u16::from_le_bytes([record[4], record[5]]));
    // Fixups are every sector: 512 bytes, or 4096 on 4K-sector drives.
    let count = u64::from(u16::from_le_bytes([record[6], record[7]]));
    let stride = (record.len() as u64 / count.saturating_sub(1).max(1)).max(512);
    let mut out = Vec::new();
    let (mut pos, end) = (start, start + len);
    while pos < end {
        // The next swapped pair at or after `pos`, if within the data.
        let sector = pos / stride + 1;
        let fix = sector * stride - 2;
        if fix >= end {
            push_merged(&mut out, Extent::new(at + pos, end - pos));
            break;
        }
        if fix > pos {
            push_merged(&mut out, Extent::new(at + pos, fix - pos));
            pos = fix;
        }
        // `pos` is within the pair: take the rest of it from the array.
        let skip = pos - fix;
        let n = (2 - skip).min(end - pos);
        push_merged(&mut out, Extent::new(at + usa + sector * 2 + skip, n));
        pos += n;
    }
    out
}

/// Decodes a data run list into (LCN, length) pairs; `None` LCN is sparse.
fn parse_runs(b: &[u8]) -> Option<Runs> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut lcn: i64 = 0;
    while let Some(&header) = b.get(pos) {
        if header == 0 {
            return Some(out);
        }
        let (len_size, off_size) = (usize::from(header & 0x0F), usize::from(header >> 4));
        if len_size == 0 || len_size > 8 || off_size > 8 || pos + 1 + len_size + off_size > b.len()
        {
            return None;
        }
        let mut length = 0u64;
        for i in 0..len_size {
            length |= u64::from(b[pos + 1 + i]) << (8 * i);
        }
        if off_size == 0 {
            out.push((None, length));
        } else {
            let mut delta = 0i64;
            for i in 0..off_size {
                delta |= i64::from(b[pos + 1 + len_size + i]) << (8 * i);
            }
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift; // sign-extend
            lcn = lcn.checked_add(delta).filter(|&l| l >= 0)?;
            out.push((Some(lcn as u64), length));
        }
        pos += 1 + len_size + off_size;
    }
    Some(out)
}

/// Joins data attribute pieces in VCN order.
fn collect_runs(pieces: &[(u64, Runs)]) -> Runs {
    let mut sorted: Vec<_> = pieces.iter().collect();
    sorted.sort_by_key(|(vcn, _)| *vcn);
    sorted
        .into_iter()
        .flat_map(|(_, runs)| runs.iter().copied())
        .collect()
}

fn filetime(b: &[u8]) -> Option<DateTime> {
    let ticks = u64::from_le_bytes(b.try_into().ok()?);
    DateTime::from_unix((ticks / 10_000_000) as i64 - WINDOWS_EPOCH_OFFSET)
}

fn volume_name(b: &[u8]) -> Option<String> {
    let mut pos = usize::from(u16::from_le_bytes([b[20], b[21]]));
    while pos + 24 <= b.len() {
        let typ = u32::from_le_bytes(b[pos..pos + 4].try_into().ok()?);
        let alen = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().ok()?) as usize;
        if typ == ATTR_END || alen < 16 {
            return None;
        }
        if typ == ATTR_VOLUME_NAME {
            let vlen = u32::from_le_bytes(b[pos + 16..pos + 20].try_into().ok()?) as usize;
            let voff = usize::from(u16::from_le_bytes([b[pos + 20], b[pos + 21]]));
            let v = b.get(pos + voff..pos + voff + vlen)?;
            // chunks_exact: an odd length must not index past the end.
            let units: Vec<u16> = v
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            return Some(String::from_utf16_lossy(&units)).filter(|s| !s.is_empty());
        }
        pos += alen;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_run_lists() {
        // 0x21: 1-byte length, 2-byte offset. Second run goes backwards.
        let runs = parse_runs(&[0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0x00, 0xFF, 0x00]).unwrap();
        assert_eq!(runs, vec![(Some(0x100), 0x10), (Some(0x100 - 0x100), 0x08)]);
        let sparse = parse_runs(&[0x01, 0x05, 0x00]).unwrap();
        assert_eq!(sparse, vec![(None, 5)]);
    }

    /// A small file's data runs over the spot at the end of a sector that
    /// NTFS swaps for a check value: the real two bytes come back from the
    /// update sequence array.
    #[test]
    fn resident_data_across_a_sector_end() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 1) as u8).collect();
        let (at, start) = (4096u64, 400usize);
        // The record as it is on disk: sector ends hold the check value,
        // the array (at 48) holds the real bytes.
        let mut disk = vec![0u8; 8192];
        let rec = &mut disk[at as usize..at as usize + 1024];
        rec[..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&48u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[48..50].copy_from_slice(&[0xAB, 0xCD]);
        rec[start..start + data.len()].copy_from_slice(&data);
        for i in 1..=2 {
            let end = i * 512;
            let real = [rec[end - 2], rec[end - 1]];
            rec[48 + i * 2..50 + i * 2].copy_from_slice(&real);
            rec[end - 2..end].copy_from_slice(&[0xAB, 0xCD]);
        }
        for (from, len) in [(0, 300), (110, 1), (111, 1), (100, 50), (0, 10)] {
            let extents = resident_extents(
                &disk[at as usize..at as usize + 1024],
                at,
                (start + from) as u64,
                len,
            );
            let mut got: Vec<u8> = Vec::new();
            for e in &extents {
                got.extend(&disk[e.offset as usize..e.end() as usize]);
            }
            assert_eq!(got[..], data[from..from + len as usize], "{from}+{len}");
        }
    }
}

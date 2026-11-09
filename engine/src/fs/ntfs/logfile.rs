//! The NTFS journal, `$LogFile`.
//!
//! Before NTFS changes a file record it writes what it is about to do to
//! the journal, with enough to redo or undo it: whole record images when a
//! record is set up or freed, and the new bytes of each attribute change.
//! Deleting a file logs the record's last contents as the undo half of the
//! "free this record" step, names, folder and cluster list included. Those
//! images outlive the record itself, which Windows reuses quickly, and give
//! back deleted files whole.
//!
//! The journal is a ring of 4 KB pages ("RCRD", each with its own update
//! sequence fixups) after two restart pages ("RSTR"). Records are packed
//! into the pages' data areas and may run on into the next page. Each
//! record's header holds its log sequence number (LSN), which encodes
//! where in the file it was written: a record is believed only at the
//! very position its LSN names, which tells live records apart from stale
//! leftovers and from bytes that merely look like a header.

use std::collections::HashMap;
use std::ops::Range;

use super::{fix_in_place, resident_extents, MAX_RECORDS};
use crate::extent::{Extent, MappedSource};
use crate::source::{read_tolerant, Source};

/// A file record as the journal shows it at one moment.
#[derive(Debug, Clone)]
pub(super) struct Image {
    pub number: u64,
    pub lsn: u64,
    /// The record, padded to the record size.
    pub bytes: Vec<u8>,
    /// For an image taken whole from one journal record: where on the
    /// source each of its bytes is (fixups accounted for), so a small
    /// file's data stored inside it can be copied straight off the disk.
    pub map: Option<Vec<Extent>>,
}

/// Most of the journal read. Windows makes it 64 MB on big volumes.
const MAX_LOG: u64 = 128 << 20;
/// Size of a journal record's header, before its client data.
const RECORD_HEAD: usize = 0x30;
/// Size of an NTFS operation's header inside the client data.
const OP_HEAD: usize = 0x20;
/// Longest client data believed; real ones are a few KB at most.
const MAX_CLIENT_DATA: usize = 64 << 10;

const INIT_FILE_RECORD: u16 = 0x02;
const DEALLOC_FILE_RECORD: u16 = 0x03;
const WRITE_END_OF_RECORD: u16 = 0x04;
const CREATE_ATTRIBUTE: u16 = 0x05;
const DELETE_ATTRIBUTE: u16 = 0x06;
const UPDATE_RESIDENT: u16 = 0x07;
const UPDATE_MAPPING_PAIRS: u16 = 0x09;
const SET_NEW_SIZES: u16 = 0x0B;

/// One NTFS operation on a file record.
struct Op {
    lsn: u64,
    redo_op: u16,
    undo_op: u16,
    number: u64,
    record_off: usize,
    attr_off: usize,
    data: Vec<u8>,
    redo: Range<usize>,
    undo: Range<usize>,
    /// Where each byte of `data` is, in journal offsets.
    pieces: Vec<Extent>,
}

/// How the journal's pages are laid out.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    page: usize,
    /// Where records start in each page, after its header and fixups.
    data: usize,
    /// High bits of an LSN that count passes around the ring; the rest is
    /// the file offset in 8-byte units.
    seq_bits: u32,
}

impl Layout {
    /// File offset of the record with this LSN.
    fn offset(&self, lsn: u64) -> u64 {
        (lsn << self.seq_bits) >> (self.seq_bits - 3)
    }
}

fn valid_page_size(n: usize) -> bool {
    n.is_power_of_two() && (512..=65536).contains(&n)
}

/// Default data offset for a page: header (0x28) and update sequence
/// array, rounded to 8 bytes.
fn default_data(page: usize) -> usize {
    (0x28 + 2 * (page / 512 + 1)).next_multiple_of(8)
}

/// Reads a restart page at `at`, for the page layout.
fn restart(log: &dyn Source, at: u64) -> Option<Layout> {
    let mut head = [0u8; 512];
    if log.read_at(at, &mut head).ok()? < 512 || &head[..4] != b"RSTR" {
        return None;
    }
    let system = u32::from_le_bytes(head[0x10..0x14].try_into().unwrap()) as usize;
    let page = u32::from_le_bytes(head[0x14..0x18].try_into().unwrap()) as usize;
    if !valid_page_size(system) || !valid_page_size(page) {
        return None;
    }
    let mut b = vec![0u8; system];
    if log.read_at(at, &mut b).ok()? < system || !fix_in_place(&mut b, b"RSTR") {
        return None;
    }
    let ra = usize::from(u16::from_le_bytes([b[0x18], b[0x19]]));
    let area = b.get(ra..ra + 0x30)?;
    let seq_bits = u32::from_le_bytes(area[0x10..0x14].try_into().unwrap());
    let data = usize::from(u16::from_le_bytes([area[0x26], area[0x27]]));
    let data = if (0x28..page - RECORD_HEAD).contains(&data) && data % 8 == 0 {
        data
    } else {
        default_data(page)
    };
    (4..=60).contains(&seq_bits).then_some(Layout {
        page,
        data,
        seq_bits,
    })
}

/// The LSN layout from the pages themselves, when both restart pages are
/// gone: the number of sequence bits under which the most pages' "last
/// record ends here" LSNs point into the page that holds them.
fn infer_seq_bits(buf: &[u8], page: usize, valid: &[bool]) -> Option<u32> {
    let mut best = (0, None);
    for bits in 4..=60u32 {
        let l = Layout {
            page,
            data: 0,
            seq_bits: bits,
        };
        let hits = valid
            .iter()
            .enumerate()
            .filter(|(_, v)| **v)
            .filter(|(k, _)| {
                let p = &buf[k * page..];
                [0x08, 0x20].iter().any(|&o| {
                    let lsn = u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
                    lsn != 0 && l.offset(lsn) / page as u64 == *k as u64
                })
            })
            .count();
        if hits > best.0 {
            best = (hits, Some(bits));
        }
    }
    best.1
}

/// Every record image the journal holds, with the record number each
/// belongs to: the undo images of freed records, the images records were
/// set up with, and those images with the later logged changes replayed
/// onto them. `extents` is where the journal's data is on the source.
pub(super) fn images(
    src: &dyn Source,
    extents: Vec<Extent>,
    cluster: u64,
    record_size: u64,
) -> Vec<Image> {
    let log = MappedSource::new(src, extents);
    let len = log.len().min(MAX_LOG) as usize;
    let mut layout = restart(&log, 0);
    if layout.is_none() {
        // The second restart page follows the first, a system page on.
        layout = [4096u64, 8192, 16384, 65536]
            .into_iter()
            .find_map(|at| restart(&log, at));
    }
    let page = layout.map_or(4096, |l| l.page);
    if len < page * 3 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len - len % page];
    read_tolerant(&log, 0, &mut buf);
    let valid: Vec<bool> = buf
        .chunks_exact_mut(page)
        .enumerate()
        .map(|(k, p)| k >= 2 && fix_in_place(p, b"RCRD"))
        .collect();
    let layout = match layout {
        Some(l) => l,
        None => match infer_seq_bits(&buf, page, &valid) {
            Some(seq_bits) => Layout {
                page,
                data: default_data(page),
                seq_bits,
            },
            None => return Vec::new(),
        },
    };
    let mut ops = find_ops(&buf, &valid, layout, cluster, record_size);
    ops.sort_by_key(|o| o.lsn);
    replay(&ops, &log, record_size as usize)
}

/// Finds the journal's records about file records in its pages.
fn find_ops(buf: &[u8], valid: &[bool], l: Layout, cluster: u64, record_size: u64) -> Vec<Op> {
    let page = l.page;
    let mut out = Vec::new();
    for k in (0..valid.len()).filter(|&k| valid[k]) {
        let p0 = k * page;
        let mut at = l.data;
        while at + RECORD_HEAD <= page {
            let h = &buf[p0 + at..p0 + at + RECORD_HEAD];
            at += 8;
            let lsn = u64::from_le_bytes(h[..8].try_into().unwrap());
            if lsn == 0 || l.offset(lsn) != (p0 + at - 8) as u64 {
                continue;
            }
            // Client records only; restart records hold no file changes.
            let kind = u32::from_le_bytes(h[0x20..0x24].try_into().unwrap());
            let n = u32::from_le_bytes(h[0x18..0x1C].try_into().unwrap()) as usize;
            if kind != 1 || !(OP_HEAD..=MAX_CLIENT_DATA).contains(&n) {
                continue;
            }
            // Gather the client data, on into following pages if needed.
            let (mut data, mut pieces) = (Vec::with_capacity(n), Vec::new());
            let (mut kk, mut start) = (k, at - 8 + RECORD_HEAD);
            while data.len() < n {
                if start >= page {
                    kk += 1;
                    start = l.data;
                    if !valid.get(kk).copied().unwrap_or(false) {
                        break;
                    }
                }
                let take = (n - data.len()).min(page - start);
                let pg = &buf[kk * page..(kk + 1) * page];
                data.extend_from_slice(&pg[start..start + take]);
                pieces.extend(resident_extents(
                    pg,
                    (kk * page) as u64,
                    start as u64,
                    take as u64,
                ));
                start += take;
            }
            if data.len() < n {
                continue;
            }
            if let Some(op) = op(lsn, data, pieces, cluster, record_size) {
                out.push(op);
            }
        }
    }
    out
}

/// Decodes an NTFS operation, keeping only those on file records.
fn op(lsn: u64, data: Vec<u8>, pieces: Vec<Extent>, cluster: u64, record_size: u64) -> Option<Op> {
    let u16_at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    let (redo_op, undo_op) = (u16_at(0), u16_at(2));
    let interesting = matches!(
        redo_op,
        INIT_FILE_RECORD
            | DEALLOC_FILE_RECORD
            | WRITE_END_OF_RECORD
            | CREATE_ATTRIBUTE
            | DELETE_ATTRIBUTE
            | UPDATE_RESIDENT
            | UPDATE_MAPPING_PAIRS
            | SET_NEW_SIZES
    ) || undo_op == INIT_FILE_RECORD;
    if !interesting {
        return None;
    }
    let range = |off: u16, len: u16| {
        let (o, n) = (usize::from(off), usize::from(len));
        if o >= OP_HEAD && o + n <= data.len() {
            o..o + n
        } else {
            0..0
        }
    };
    let redo = range(u16_at(4), u16_at(6));
    let undo = range(u16_at(8), u16_at(10));
    // The record's place in the MFT: a cluster of it, then 512-byte
    // blocks into that cluster.
    let vcn = u64::from_le_bytes(data[0x18..0x20].try_into().unwrap());
    let number = vcn
        .checked_mul(cluster)?
        .checked_add(u64::from(u16_at(0x14)) * 512)?
        / record_size;
    if number >= MAX_RECORDS {
        return None;
    }
    Some(Op {
        lsn,
        redo_op,
        undo_op,
        number,
        record_off: usize::from(u16_at(0x10)),
        attr_off: usize::from(u16_at(0x12)),
        redo,
        undo,
        data,
        pieces,
    })
}

/// A record image from one operation's redo or undo data, if it is one.
fn image(op: &Op, part: &Range<usize>, log: &MappedSource, size: usize) -> Option<Image> {
    let d = op.data.get(part.clone())?;
    if op.record_off != 0 || d.len() < 0x30 || &d[..4] != b"FILE" {
        return None;
    }
    // Records of NTFS 3.1 carry their own number; it must agree.
    let usa = u16::from_le_bytes([d[4], d[5]]);
    let own = u32::from_le_bytes(d[0x2C..0x30].try_into().unwrap());
    if usa >= 0x30 && u64::from(own) != op.number {
        return None;
    }
    let n = d.len().min(size);
    let mut bytes = d[..n].to_vec();
    bytes.resize(size, 0);
    let mut map = Vec::new();
    for e in sub_extents(&op.pieces, part.start as u64, n as u64) {
        for p in log.physical(e.offset, e.len) {
            crate::extent::push_merged(&mut map, p);
        }
    }
    let mapped = crate::extent::total_len(&map) == n as u64;
    Some(Image {
        number: op.number,
        lsn: op.lsn,
        bytes,
        map: mapped.then_some(map),
    })
}

/// The part `[from, from + len)` of the stream that `pieces` lays out.
fn sub_extents(pieces: &[Extent], from: u64, len: u64) -> Vec<Extent> {
    let mut out = Vec::new();
    let (mut pos, end) = (0u64, from.saturating_add(len));
    for e in pieces {
        let (s, t) = (pos.max(from), (pos + e.len).min(end));
        if s < t {
            crate::extent::push_merged(&mut out, Extent::new(e.offset + (s - pos), t - s));
        }
        pos += e.len;
    }
    out
}

/// A record followed through the journal from the image it was set up
/// with.
struct Life {
    image: Vec<u8>,
    lsn: u64,
    /// The setting-up image's map, while nothing has changed the image.
    map: Option<Vec<Extent>>,
}

impl Life {
    fn into_image(self, number: u64) -> Image {
        Image {
            number,
            lsn: self.lsn,
            bytes: self.image,
            map: self.map,
        }
    }
}

/// Replays the operations, in LSN order, onto the records they change.
fn replay(ops: &[Op], log: &MappedSource, size: usize) -> Vec<Image> {
    let mut out = Vec::new();
    let mut lives: HashMap<u64, Life> = HashMap::new();
    for op in ops {
        if op.undo_op == INIT_FILE_RECORD {
            out.extend(image(op, &op.undo, log, size));
        }
        match op.redo_op {
            INIT_FILE_RECORD => {
                if let Some(prev) = lives.remove(&op.number) {
                    out.push(prev.into_image(op.number));
                }
                if let Some(img) = image(op, &op.redo, log, size) {
                    lives.insert(
                        op.number,
                        Life {
                            image: img.bytes,
                            lsn: op.lsn,
                            map: img.map,
                        },
                    );
                }
            }
            DEALLOC_FILE_RECORD => {
                if let Some(mut life) = lives.remove(&op.number) {
                    life.image[0x16] &= !1;
                    out.push(life.into_image(op.number));
                }
            }
            _ => {
                let Some(life) = lives.get_mut(&op.number) else {
                    continue;
                };
                // A change that does not fit the image means a step was
                // missed (a page lost to the ring): stop following it.
                if apply(&mut life.image, op) {
                    life.map = None;
                    life.lsn = op.lsn;
                } else if let Some(life) = lives.remove(&op.number) {
                    out.push(life.into_image(op.number));
                }
            }
        }
    }
    let mut rest: Vec<(u64, Life)> = lives.into_iter().collect();
    rest.sort_by_key(|(n, _)| *n);
    out.extend(rest.into_iter().map(|(n, l)| l.into_image(n)));
    out
}

fn get32(rec: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_le_bytes(rec.get(o..o + 4)?.try_into().unwrap()) as usize)
}

fn set32(rec: &mut [u8], o: usize, v: usize) {
    rec[o..o + 4].copy_from_slice(&(v as u32).to_le_bytes());
}

/// The attribute at `roff`, which must lie inside the record's used part:
/// its length and whether it is non-resident.
fn attr_at(rec: &[u8], roff: usize, used: usize) -> Option<(usize, bool)> {
    let asize = get32(rec, roff + 4)?;
    (roff >= 0x18 && asize >= 24 && roff + asize <= used).then(|| (asize, rec[roff + 8] != 0))
}

/// Resizes the attribute at `roff` from `asize` to `nsize` bytes, moving
/// the attributes after it, if the record has room.
fn resize(rec: &mut [u8], roff: usize, used: usize, asize: usize, nsize: usize) -> bool {
    // `attr_at` checked that the attribute ends within `used`.
    let new_used = used - asize + nsize;
    if new_used > rec.len() {
        return false;
    }
    rec.copy_within(roff + asize..used, roff + nsize);
    if new_used < used {
        rec[new_used..used].fill(0);
    }
    set32(rec, 0x18, new_used);
    set32(rec, roff + 4, nsize);
    true
}

/// Applies one redo step to a record image, as NTFS would on mounting.
/// Returns false, leaving the image as it was or half-changed, if the step
/// does not fit it.
fn apply(rec: &mut [u8], op: &Op) -> bool {
    let size = rec.len();
    let d = &op.data[op.redo.clone()];
    let Some(used) = get32(rec, 0x18).filter(|u| (0x30..=size).contains(u)) else {
        return false;
    };
    let (roff, aoff) = (op.record_off, op.attr_off);
    match op.redo_op {
        WRITE_END_OF_RECORD => {
            let at = roff + aoff;
            if roff < 0x18 || at + d.len() > size {
                return false;
            }
            rec[at..at + d.len()].copy_from_slice(d);
            set32(rec, 0x18, at + d.len());
            true
        }
        CREATE_ATTRIBUTE => {
            let Some(esize) = get32(d, 4).filter(|&n| n >= 24 && n <= d.len()) else {
                return false;
            };
            if roff < 0x18 || roff > used || used + esize > size {
                return false;
            }
            rec.copy_within(roff..used, roff + esize);
            rec[roff..roff + esize].copy_from_slice(&d[..esize]);
            set32(rec, 0x18, used + esize);
            true
        }
        DELETE_ATTRIBUTE => {
            let Some((asize, _)) = attr_at(rec, roff, used) else {
                return false;
            };
            rec.copy_within(roff + asize..used, roff);
            rec[used - asize..used].fill(0);
            set32(rec, 0x18, used - asize);
            true
        }
        UPDATE_RESIDENT => {
            let Some((asize, false)) = attr_at(rec, roff, used) else {
                return false;
            };
            let end = aoff + d.len();
            if op.redo.len() == op.undo.len() {
                // Same length: overwritten in place.
                if end > asize {
                    return false;
                }
                rec[roff + aoff..roff + end].copy_from_slice(d);
                return true;
            }
            let value_at = usize::from(u16::from_le_bytes([rec[roff + 0x14], rec[roff + 0x15]]));
            if aoff < value_at || !resize(rec, roff, used, asize, end.next_multiple_of(8)) {
                return false;
            }
            set32(rec, roff + 0x10, end - value_at);
            rec[roff + aoff..roff + end].copy_from_slice(d);
            true
        }
        UPDATE_MAPPING_PAIRS => {
            let Some((asize, true)) = attr_at(rec, roff, used) else {
                return false;
            };
            if asize < 0x40 {
                return false;
            }
            let runs_at = usize::from(u16::from_le_bytes([rec[roff + 0x20], rec[roff + 0x21]]));
            let end = aoff + d.len();
            if aoff < runs_at
                || aoff > asize
                || !resize(rec, roff, used, asize, end.next_multiple_of(8))
            {
                return false;
            }
            rec[roff + aoff..roff + end].copy_from_slice(d);
            true
        }
        SET_NEW_SIZES => {
            let Some((asize, true)) = attr_at(rec, roff, used) else {
                return false;
            };
            if d.len() < 24 || asize < 0x40 {
                return false;
            }
            // Allocated, valid (initialised) and data sizes, in that order.
            rec[roff + 0x28..roff + 0x30].copy_from_slice(&d[0..8]);
            rec[roff + 0x38..roff + 0x40].copy_from_slice(&d[8..16]);
            rec[roff + 0x30..roff + 0x38].copy_from_slice(&d[16..24]);
            true
        }
        _ => true,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub const PAGE: usize = 4096;
    pub const SEQ_BITS: u32 = 30;

    /// One operation as the journal's client data holds it.
    pub struct TestOp<'a> {
        pub redo_op: u16,
        pub undo_op: u16,
        pub redo: &'a [u8],
        pub undo: &'a [u8],
        pub record_off: u16,
        pub attr_off: u16,
        /// Target: the MFT cluster and the 512-byte block in it.
        pub vcn: u64,
        pub block: u16,
    }

    pub fn client_data(o: &TestOp) -> Vec<u8> {
        let mut d = vec![0u8; OP_HEAD];
        d[0..2].copy_from_slice(&o.redo_op.to_le_bytes());
        d[2..4].copy_from_slice(&o.undo_op.to_le_bytes());
        let redo_at = OP_HEAD;
        let undo_at = (redo_at + o.redo.len()).next_multiple_of(8);
        d[4..6].copy_from_slice(&(redo_at as u16).to_le_bytes());
        d[6..8].copy_from_slice(&(o.redo.len() as u16).to_le_bytes());
        d[8..10].copy_from_slice(&(undo_at as u16).to_le_bytes());
        d[10..12].copy_from_slice(&(o.undo.len() as u16).to_le_bytes());
        d[0x10..0x12].copy_from_slice(&o.record_off.to_le_bytes());
        d[0x12..0x14].copy_from_slice(&o.attr_off.to_le_bytes());
        d[0x14..0x16].copy_from_slice(&o.block.to_le_bytes());
        d[0x18..0x20].copy_from_slice(&o.vcn.to_le_bytes());
        d.extend_from_slice(o.redo);
        d.resize(undo_at, 0);
        d.extend_from_slice(o.undo);
        d
    }

    /// A journal of `pages` pages holding `ops` in order from page 2 (the
    /// first log page), as on disk: restart page (if `restart`), records
    /// packed and running across pages, fixups applied.
    pub fn journal(ops: &[TestOp], pages: usize, restart: bool) -> Vec<u8> {
        let data_at = default_data(PAGE);
        let lsn_of = |off: usize| (off as u64 >> 3) | (1u64 << (64 - SEQ_BITS));
        let mut log = vec![0u8; pages * PAGE];
        // Logical stream of page data areas, then cut into pages.
        let area = PAGE - data_at;
        let mut stream: Vec<u8> = Vec::new();
        let file_off = |s: usize| (2 + s / area) * PAGE + data_at + s % area;
        let mut last_end = vec![0u64; pages];
        for o in ops {
            // A header must fit in the page; otherwise skip to the next.
            if area - stream.len() % area < RECORD_HEAD {
                stream.resize(stream.len().next_multiple_of(area), 0);
            }
            let at = file_off(stream.len());
            let d = client_data(o);
            let mut h = vec![0u8; RECORD_HEAD];
            h[..8].copy_from_slice(&lsn_of(at).to_le_bytes());
            h[0x18..0x1C].copy_from_slice(&(d.len() as u32).to_le_bytes());
            h[0x20..0x24].copy_from_slice(&1u32.to_le_bytes());
            let spans = (stream.len() % area) + RECORD_HEAD + d.len() > area;
            h[0x28] = u8::from(spans);
            stream.extend(h);
            stream.extend(d);
            stream.resize(stream.len().next_multiple_of(8), 0);
            let end_page = (stream.len() - 1) / area + 2;
            if end_page < pages {
                last_end[end_page] = lsn_of(at);
            }
        }
        for (k, chunk) in stream.chunks(area).enumerate() {
            let p = &mut log[(2 + k) * PAGE..(3 + k) * PAGE];
            p[data_at..data_at + chunk.len()].copy_from_slice(chunk);
        }
        for k in 2..pages {
            let p = &mut log[k * PAGE..(k + 1) * PAGE];
            if p[data_at..].iter().all(|&b| b == 0) {
                continue;
            }
            p[..4].copy_from_slice(b"RCRD");
            p[0x20..0x28].copy_from_slice(&last_end[k].to_le_bytes());
            seal(p);
        }
        if restart {
            let p = &mut log[..PAGE];
            p[..4].copy_from_slice(b"RSTR");
            p[0x10..0x14].copy_from_slice(&(PAGE as u32).to_le_bytes());
            p[0x14..0x18].copy_from_slice(&(PAGE as u32).to_le_bytes());
            p[0x18..0x1A].copy_from_slice(&0x30u16.to_le_bytes());
            p[0x30 + 0x10..0x30 + 0x14].copy_from_slice(&SEQ_BITS.to_le_bytes());
            p[0x30 + 0x26..0x30 + 0x28].copy_from_slice(&(data_at as u16).to_le_bytes());
            seal(p);
        }
        log
    }

    /// Applies update sequence fixups to a 4 KB page for writing.
    pub fn seal(p: &mut [u8]) {
        p[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        p[6..8].copy_from_slice(&9u16.to_le_bytes());
        p[0x28..0x2A].copy_from_slice(&[0x5A, 0x00]);
        for i in 1..=8 {
            let end = i * 512;
            p[0x28 + i * 2] = p[end - 2];
            p[0x29 + i * 2] = p[end - 1];
            p[end - 2..end].copy_from_slice(&[0x5A, 0x00]);
        }
    }

    /// A 1 KB file record image as NTFS keeps it in memory: no fixups.
    pub fn record(number: u32, seq: u16, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 0x38];
        r[..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
        r[0x2C..0x30].copy_from_slice(&number.to_le_bytes());
        for a in attrs {
            r.extend(a);
        }
        r.extend(0xFFFF_FFFFu32.to_le_bytes());
        r.extend([0u8; 4]);
        let used = r.len() as u32;
        r[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
        r
    }

    pub fn resident(typ: u32, value: &[u8]) -> Vec<u8> {
        let len = (24 + value.len()).next_multiple_of(8);
        let mut a = vec![0u8; len];
        a[..4].copy_from_slice(&typ.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    pub fn non_resident(runs: &[u8], size: u64, last_vcn: u64) -> Vec<u8> {
        let len = (0x40 + runs.len()).next_multiple_of(8);
        let mut a = vec![0u8; len];
        a[..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[0x18..0x20].copy_from_slice(&last_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x28..0x30].copy_from_slice(&size.next_multiple_of(4096).to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&size.to_le_bytes());
        a[0x40..0x40 + runs.len()].copy_from_slice(runs);
        a
    }

    /// A file name attribute's value.
    pub fn file_name(parent: u64, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        v[..8].copy_from_slice(&parent.to_le_bytes());
        let t = super::super::index::tests::TICKS;
        v[8..16].copy_from_slice(&t.to_le_bytes());
        v[16..24].copy_from_slice(&t.to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = 1;
        for (i, u) in units.iter().enumerate() {
            v[66 + i * 2..68 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        v
    }

    fn one_extent(log: &[u8]) -> Vec<Extent> {
        vec![Extent::new(0, log.len() as u64)]
    }

    /// The undo image of a freed record comes back, mapped byte for byte
    /// onto the journal's pages even where it crosses sector ends and runs
    /// into the next page.
    #[test]
    fn freed_record_image_and_its_map() {
        let text: Vec<u8> = (0..600u32).map(|i| b'a' + (i % 26) as u8).collect();
        let img = record(
            40,
            3,
            1,
            &[
                resident(0x30, &file_name(5 | (5 << 48), "notes.txt")),
                resident(0x80, &text),
            ],
        );
        let filler = vec![0x11u8; 3000];
        let ops = [
            TestOp {
                redo_op: 0,
                undo_op: 0,
                redo: &filler,
                undo: &[],
                record_off: 0,
                attr_off: 0,
                vcn: 0,
                block: 0,
            },
            // Record 40 at 1 KB records, 4 KB clusters: cluster 10, block 0.
            TestOp {
                redo_op: DEALLOC_FILE_RECORD,
                undo_op: INIT_FILE_RECORD,
                redo: &[],
                undo: &img,
                record_off: 0,
                attr_off: 0,
                vcn: 10,
                block: 0,
            },
        ];
        for restart in [true, false] {
            let log = journal(&ops, 8, restart);
            let got = images(&log, one_extent(&log), 4096, 1024);
            assert_eq!(got.len(), 1, "restart page: {restart}");
            let i = &got[0];
            assert_eq!(i.number, 40);
            assert_eq!(&i.bytes[..img.len()], &img[..]);
            let view = MappedSource::new(&log, i.map.clone().unwrap());
            let mut back = vec![0u8; img.len()];
            assert_eq!(view.read_at(0, &mut back).unwrap(), img.len());
            assert_eq!(back, img);
        }
    }

    /// A record set up empty, then given clusters and a size by later
    /// steps, comes back as it ended up.
    #[test]
    fn replays_changes_onto_a_record() {
        let name = resident(0x30, &file_name(5, "movie.mp4"));
        let start = record(41, 7, 1, &[name.clone(), non_resident(&[0], 0, 0)]);
        let data_at = (0x38 + name.len()) as u16;
        let runs = [0x21u8, 0x30, 0x00, 0x02, 0x00]; // 0x30 clusters at 0x200
        let sizes: Vec<u8> = [0x30u64 * 4096, 190_000, 190_000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let step = |redo_op, redo, attr_off| TestOp {
            redo_op,
            undo_op: 0,
            redo,
            undo: &[],
            record_off: data_at,
            attr_off,
            vcn: 10,
            block: 2,
        };
        let ops = [
            TestOp {
                redo_op: INIT_FILE_RECORD,
                undo_op: 0,
                redo: &start,
                undo: &[],
                record_off: 0,
                attr_off: 0,
                vcn: 10,
                block: 2,
            },
            step(UPDATE_MAPPING_PAIRS, &runs[..], 0x40),
            step(SET_NEW_SIZES, &sizes[..], 0),
        ];
        let log = journal(&ops, 6, true);
        let got = images(&log, one_extent(&log), 4096, 1024);
        assert_eq!(got.len(), 1);
        let rec = super::super::parse_record(&got[0].bytes, 41, 0).unwrap();
        assert_eq!(rec.size, Some(190_000));
        assert_eq!(rec.data, vec![(0, vec![(Some(0x200), 0x30)])]);
        assert_eq!(rec.names[0].0, "movie.mp4");
        assert!(got[0].map.is_none());
    }

    #[test]
    fn survives_corrupt_journals() {
        let img = record(
            40,
            3,
            1,
            &[
                resident(0x30, &file_name(5, "a.txt")),
                non_resident(&[0x11, 4, 9, 0], 9000, 3),
            ],
        );
        let ops: Vec<TestOp> = (0..30)
            .map(|i| TestOp {
                redo_op: [INIT_FILE_RECORD, UPDATE_MAPPING_PAIRS, SET_NEW_SIZES][i % 3],
                undo_op: INIT_FILE_RECORD,
                redo: &img[..img.len() - (i % 3) * 50],
                undo: &img,
                record_off: [0, 0x60, 0x60, 0x400][i % 4],
                attr_off: (i * 8) as u16,
                vcn: 10,
                block: (i % 2) as u16,
            })
            .collect();
        let good = journal(&ops, 24, true);
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..300 {
            let mut b = good.clone();
            for _ in 0..1 + rng % 64 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let at = (rng % b.len() as u64) as usize;
                b[at] = (rng >> 40) as u8;
            }
            let ext = one_extent(&b);
            for i in images(&b, ext, 4096, 1024) {
                let _ = super::super::parse_record(&i.bytes, i.number, 0);
            }
        }
    }
}

//! Repairing photos that won't open.
//!
//! - **Lost header.** A JPEG's header holds the tables its compressed data
//!   is decoded with: quantisation, Huffman and the frame size. Cameras
//!   write the same tables into every photo of a given size and quality,
//!   so a good photo from the same camera supplies them. When the start of
//!   the scan survives (only the tables were overwritten), the good
//!   photo's header is put in front of it. When the whole start is gone,
//!   the data can still be picked up at the next restart marker, where
//!   the decoder starts afresh: the missing stretch before it is filled
//!   with grey blocks encoded with the same tables, so the rest of the
//!   picture lands where it belongs.
//! - **Cut short.** The part that survived is kept and given an end
//!   marker; the decoder shows it and greys the rest.
//! - **Beyond that.** Cameras store smaller copies inside each photo: the
//!   EXIF thumbnail and, in most current cameras, a large preview in the
//!   Multi-Picture (MPF) segment. The largest that is whole is rescued.
//! - **HEIC.** An iPhone photo is a grid of tiles listed in an index at the
//!   start of the file. One cut short keeps its complete rows of tiles: a
//!   new index describes a shorter picture made of them. When no row is
//!   whole, the thumbnail stored beside the picture is rescued instead.
//!
//! Untrusted data never sizes an allocation beyond the photo itself.

use std::io::Cursor;

use serde::{Deserialize, Serialize};

use crate::formats::jpeg;
use crate::reader::Reader;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotoRepair {
    /// A JPEG given the header of a good photo from the same camera.
    Header,
    /// A JPEG cut short: the part that survived.
    Partial,
    /// The photo's own embedded preview, smaller than the photo.
    Preview,
    /// A HEIC cut short: its complete rows of tiles.
    HeicPartial,
    /// A HEIC's own thumbnail.
    HeicThumbnail,
}

impl PhotoRepair {
    /// Extension of the repaired file, when it differs from the damaged one's.
    pub fn ext(self) -> Option<&'static str> {
        match self {
            PhotoRepair::Preview => Some("jpg"),
            _ => None,
        }
    }

    /// Added to the name of what was rescued, when it is less than the photo.
    pub fn suffix(self) -> &'static str {
        match self {
            PhotoRepair::Header => "",
            PhotoRepair::Partial | PhotoRepair::HeicPartial => "(partial)",
            PhotoRepair::Preview | PhotoRepair::HeicThumbnail => "(preview)",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            PhotoRepair::Header => {
                "Repaired: its lost header was rebuilt from a photo by the same camera."
            }
            PhotoRepair::Partial => {
                "Only part of the photo survived; that part was kept and the rest shows grey."
            }
            PhotoRepair::Preview => {
                "The photo could not be repaired; its embedded preview (a smaller copy) was rescued."
            }
            PhotoRepair::HeicPartial => {
                "Only the top of the photo survived; it was kept as a shorter photo."
            }
            PhotoRepair::HeicThumbnail => {
                "The photo could not be repaired; its thumbnail was rescued."
            }
        }
    }
}

/// Repairs a damaged photo. `refs` are good photos that may come from the
/// same camera (used for JPEGs whose header is lost). Returns `None` when
/// the photo is intact or nothing could be rescued.
pub fn repair(data: &[u8], refs: &[&[u8]]) -> Option<(Vec<u8>, PhotoRepair)> {
    repair_with(data, refs).map(|(d, how, _)| (d, how))
}

/// Picture width and height, make and model.
pub type ReferenceKey = (u16, u16, Option<(String, String)>);

/// What tells one camera's JPEG header from another's: picture size and
/// make and model. `None` if `head` is not a JPEG whose header can be lent.
pub fn reference_key(head: &[u8]) -> Option<ReferenceKey> {
    let h = reference_header(head)?;
    let (w, ht) = h.size()?;
    Some((w, ht, h.make_model))
}

/// Like [`repair`], and says which of `refs` was used, if any.
pub fn repair_with(data: &[u8], refs: &[&[u8]]) -> Option<(Vec<u8>, PhotoRepair, Option<usize>)> {
    if is_heif(data) {
        return repair_heic(data).map(|(d, how)| (d, how, None));
    }
    repair_jpeg_with(data, refs)
}

fn is_heif(data: &[u8]) -> bool {
    data.get(4..8) == Some(b"ftyp")
        && matches!(
            data.get(8..12),
            Some(b"heic" | b"heix" | b"mif1" | b"msf1" | b"heim" | b"heis")
        )
}

// ----------------------------------------------------------------- JPEG

/// Whether a JPEG is structurally whole (the scan reaches its end marker).
fn jpeg_whole(data: &[u8]) -> bool {
    let v = data.to_vec();
    jpeg::walk(&mut Reader::new(&v, 0), 0, v.len() as u64).is_ok()
}

/// Decodes a JPEG fully. Returns its size.
pub fn decodes(data: &[u8]) -> Option<(u16, u16)> {
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(data));
    // Real photos compress at least 1:1000 or so; more is a corrupt header.
    d.set_max_decoding_buffer_size((data.len().saturating_mul(1024)).clamp(64 << 20, 512 << 20));
    d.decode().ok()?;
    let info = d.info()?;
    Some((info.width, info.height))
}

pub fn repair_jpeg(data: &[u8], refs: &[&[u8]]) -> Option<(Vec<u8>, PhotoRepair)> {
    repair_jpeg_with(data, refs).map(|(d, how, _)| (d, how))
}

fn repair_jpeg_with(data: &[u8], refs: &[&[u8]]) -> Option<(Vec<u8>, PhotoRepair, Option<usize>)> {
    if data.starts_with(&[0xFF, 0xD8]) && jpeg_whole(data) && decodes(data).is_some() {
        return None;
    }
    let own = Header::parse(data);
    // The header is intact: keep what of the picture survived.
    if own.as_ref().is_some_and(|h| h.complete()) {
        if let Some(p) = partial(data) {
            return Some((p, PhotoRepair::Partial, None));
        }
    }
    if let Some((fixed, used)) = transplant(data, own.as_ref(), refs) {
        return Some((fixed, PhotoRepair::Header, Some(used)));
    }
    if let Some(p) = best_preview(data) {
        return Some((p, PhotoRepair::Preview, None));
    }
    None
}

/// The surviving part of a JPEG cut short, ended properly, if it decodes.
fn partial(data: &[u8]) -> Option<Vec<u8>> {
    let v = data.to_vec();
    // Where the data stops making sense: the end of a cut-off photo, or
    // where overwritten data begins.
    let stop = match jpeg::walk(&mut Reader::new(&v, 0), 0, v.len() as u64) {
        Ok(f) => f.len as usize,
        Err(p) => p as usize,
    }
    .min(data.len());
    // Trailing zeros are unused space after the data, not image.
    let end = data[..stop]
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i + 1);
    let mut out = data[..end].to_vec();
    if !out.ends_with(&[0xFF, 0xD9]) {
        // A marker cut in half would swallow the end marker.
        if out.last() == Some(&0xFF) {
            out.pop();
        }
        out.extend([0xFF, 0xD9]);
    }
    let h = Header::parse(&out)?;
    // Some picture data must be there.
    if out.len() <= h.scan_start + 64 {
        return None;
    }
    decodes(&out).map(|_| out)
}

/// A JPEG's header segments, as far as they could be read.
#[derive(Debug, Default, Clone)]
struct Header {
    /// Whole segments, marker included.
    dqt: Vec<Vec<u8>>,
    dht: Vec<Vec<u8>>,
    sof: Option<Vec<u8>>,
    dri: Option<Vec<u8>>,
    sos: Option<Vec<u8>>,
    /// Where the compressed data starts (just after the SOS segment).
    scan_start: usize,
    make_model: Option<(String, String)>,
    /// Which reference this came from.
    index: usize,
}

impl Header {
    fn parse(data: &[u8]) -> Option<Header> {
        if !data.starts_with(&[0xFF, 0xD8]) {
            return None;
        }
        let mut h = Header::default();
        let mut i = 2;
        while i + 4 <= data.len() {
            if data[i] != 0xFF {
                break;
            }
            let marker = data[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
            if len < 2 || i + 2 + len > data.len() {
                break;
            }
            let seg = data[i..i + 2 + len].to_vec();
            match marker {
                0xDB => h.dqt.push(seg),
                0xC4 => h.dht.push(seg),
                0xC0 | 0xC1 => h.sof = Some(seg),
                // Progressive and other frame types are not rebuilt.
                0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
                0xDD => h.dri = Some(seg),
                0xE1 if h.make_model.is_none() => {
                    h.make_model = seg.get(4..).and_then(exif_make_model);
                }
                0xDA => {
                    h.sos = Some(seg);
                    h.scan_start = i + 2 + len;
                    return Some(h);
                }
                _ => {}
            }
            i += 2 + len;
        }
        Some(h)
    }

    fn complete(&self) -> bool {
        self.sos.is_some() && self.sof.is_some() && !self.dqt.is_empty() && !self.dht.is_empty()
    }

    fn size(&self) -> Option<(u16, u16)> {
        let s = self.sof.as_ref()?;
        Some((
            u16::from_be_bytes([*s.get(7)?, *s.get(8)?]),
            u16::from_be_bytes([*s.get(5)?, *s.get(6)?]),
        ))
    }

    /// The tables and frame header, ready to be followed by compressed data.
    fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        for s in &self.dqt {
            out.extend(s);
        }
        out.extend(self.sof.iter().flatten());
        for s in &self.dht {
            out.extend(s);
        }
        out.extend(self.dri.iter().flatten());
        out.extend(self.sos.iter().flatten());
        out
    }

    fn restart_interval(&self) -> Option<u16> {
        let d = self.dri.as_ref()?;
        Some(u16::from_be_bytes([*d.get(4)?, *d.get(5)?])).filter(|&r| r > 0)
    }
}

/// Make and model from an APP1 EXIF segment body.
fn exif_make_model(app1: &[u8]) -> Option<(String, String)> {
    let t = app1.strip_prefix(b"Exif\0\0")?;
    let le = match t.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = t.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b: [u8; 4] = t.get(o..o + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let ifd = u32_at(4)? as usize;
    let mut make = String::new();
    let mut model = String::new();
    for i in 0..usize::from(u16_at(ifd)?).min(256) {
        let e = ifd + 2 + 12 * i;
        let tag = u16_at(e)?;
        if tag != 0x010F && tag != 0x0110 {
            continue;
        }
        let n = u32_at(e + 4)? as usize;
        let at = if n <= 4 {
            e + 8
        } else {
            u32_at(e + 8)? as usize
        };
        let s = t.get(at..at.checked_add(n.min(64))?)?;
        let s = String::from_utf8_lossy(s)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        if tag == 0x010F {
            make = s;
        } else {
            model = s;
        }
    }
    (!make.is_empty() || !model.is_empty()).then_some((make, model))
}

/// A reference photo's header, if it is a baseline JPEG.
fn reference_header(r: &[u8]) -> Option<Header> {
    Header::parse(r).filter(Header::complete)
}

/// Rebuilds a JPEG whose tables were lost, with a reference's header.
fn transplant(data: &[u8], own: Option<&Header>, refs: &[&[u8]]) -> Option<(Vec<u8>, usize)> {
    let mut candidates: Vec<Header> = refs
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let mut h = reference_header(r)?;
            h.index = i;
            Some(h)
        })
        .collect();
    // The same camera first: a matching make and model, then size.
    let own_size = own.and_then(Header::size);
    let own_camera = own.and_then(|h| h.make_model.clone());
    candidates.sort_by_key(|c| {
        let camera = own_camera.is_some() && c.make_model == own_camera;
        let size = own_size.is_some() && c.size() == own_size;
        (!camera, !size)
    });
    if let Some(sz) = own_size {
        candidates.retain(|c| c.size() == Some(sz));
    }
    if let Some(cam) = &own_camera {
        if candidates
            .iter()
            .any(|c| c.make_model.as_ref() == Some(cam))
        {
            candidates.retain(|c| c.make_model.as_ref() == Some(cam));
        }
    }

    // The compressed data's own start survived: its SOS segment is there.
    if let Some(scan) = find_scan_start(data) {
        for c in &candidates {
            let mut out = c.bytes();
            out.extend(&data[scan..]);
            if let Some(fixed) = finish(out) {
                return Some((fixed, c.index));
            }
        }
    }
    // Only data after the lost start: pick it up at a restart marker.
    for c in candidates.iter().filter(|c| c.restart_interval().is_some()) {
        if let Some(fixed) = resume_at_restart(data, c) {
            return Some((fixed, c.index));
        }
    }
    None
}

/// Ends a rebuilt JPEG properly and keeps it if it decodes.
fn finish(mut out: Vec<u8>) -> Option<Vec<u8>> {
    if !jpeg_whole(&out) {
        return partial(&out);
    }
    if let Ok(f) = jpeg::walk(&mut Reader::new(&out, 0), 0, out.len() as u64) {
        out.truncate(f.len as usize);
    }
    decodes(&out).map(|_| out)
}

/// Where the compressed data starts after a start-of-scan segment for one
/// (greyscale) or three (colour) components, if one is in the first part
/// of the file.
fn find_scan_start(data: &[u8]) -> Option<usize> {
    let window = &data[..data.len().min(4 << 20)];
    memchr::memmem::find_iter(window, &[0xFF, 0xDA]).find_map(|i| {
        let s = window.get(i..i + 14)?;
        let ok = match (s[2], s[3], s[4]) {
            (0, 12, 3) => s[11] == 0 && s[12] == 0x3F && s[13] == 0,
            (0, 8, 1) => s[7] == 0 && s[8] == 0x3F && s[9] == 0,
            _ => false,
        };
        ok.then_some(i + 2 + usize::from(s[3]))
    })
}

/// Huffman codes of a table: (code, length) by symbol.
struct Huffman {
    codes: [(u16, u8); 256],
}

/// The Huffman tables in DHT segments: (class, id) -> codes.
fn huffman_tables(dht: &[Vec<u8>]) -> Vec<((u8, u8), Huffman)> {
    let mut out = Vec::new();
    for seg in dht {
        let mut p = 4;
        while p + 17 <= seg.len() {
            let tc_th = seg[p];
            let counts = &seg[p + 1..p + 17];
            let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
            let Some(symbols) = seg.get(p + 17..p + 17 + total) else {
                break;
            };
            let mut h = Huffman {
                codes: [(0, 0); 256],
            };
            let mut code = 0u32;
            let mut k = 0;
            for (len, &n) in counts.iter().enumerate() {
                for _ in 0..n {
                    h.codes[usize::from(symbols[k])] = (code as u16, len as u8 + 1);
                    code += 1;
                    k += 1;
                }
                code <<= 1;
            }
            out.push(((tc_th >> 4, tc_th & 0xF), h));
            p += 17 + total;
        }
    }
    out
}

/// Writes entropy-coded bits with byte stuffing.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, code: u16, len: u8) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | u32::from((code >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.byte(self.acc as u8);
            }
        }
    }

    fn byte(&mut self, b: u8) {
        self.out.push(b);
        if b == 0xFF {
            self.out.push(0);
        }
        self.acc = 0;
        self.n = 0;
    }

    /// Pads the last byte with ones, as before a marker.
    fn align(&mut self) {
        if self.n > 0 {
            let b = ((self.acc << (8 - self.n)) | ((1 << (8 - self.n)) - 1)) as u8;
            self.byte(b);
        }
    }
}

/// Picks up a JPEG whose start is lost at its first restart marker, with
/// a reference's header: the restart intervals before it are filled with
/// grey so the rest lands in place.
fn resume_at_restart(data: &[u8], h: &Header) -> Option<Vec<u8>> {
    let ri = u64::from(h.restart_interval()?);
    let sof = h.sof.as_ref()?;
    let (width, height) = h.size()?;
    let ncomp = usize::from(*sof.get(9)?);
    let comps: Vec<(u8, u8, u8)> = (0..ncomp)
        .map(|k| {
            let c = sof.get(10 + 3 * k..13 + 3 * k)?;
            Some((c[0], c[1] >> 4, c[1] & 0xF))
        })
        .collect::<Option<_>>()?;
    let hmax = u64::from(comps.iter().map(|c| c.1).max()?.max(1));
    let vmax = u64::from(comps.iter().map(|c| c.2).max()?.max(1));
    let mcus = u64::from(width).div_ceil(8 * hmax) * u64::from(height).div_ceil(8 * vmax);
    let intervals = mcus.div_ceil(ri);
    // Table selectors per component, from the scan header.
    let sos = h.sos.as_ref()?;
    let tables: Vec<(u8, u8)> = (0..ncomp)
        .map(|k| {
            let sel = *sos.get(6 + 2 * k)?;
            Some((sel >> 4, sel & 0xF))
        })
        .collect::<Option<_>>()?;
    let huff = huffman_tables(&h.dht);
    let code = |class: u8, id: u8, sym: u8| -> Option<(u16, u8)> {
        let t = &huff.iter().find(|(k, _)| *k == (class, id))?.1;
        Some(t.codes[usize::from(sym)]).filter(|c| c.1 > 0)
    };

    // The first run of restart markers counting up in order.
    let restarts: Vec<usize> = (0..data.len().saturating_sub(1))
        .filter(|&i| data[i] == 0xFF && (0xD0..=0xD7).contains(&data[i + 1]))
        .collect();
    let first = (0..restarts.len()).find(|&k| {
        (1..4).all(|j| {
            restarts.get(k + j).is_some_and(|&p| {
                data[p + 1] == 0xD0 + (data[restarts[k] + 1] - 0xD0 + j as u8) % 8
            })
        })
    })?;
    let at = restarts[first];
    let m = u64::from(data[at + 1] - 0xD0);
    let end = memchr::memmem::find(&data[at..], &[0xFF, 0xD9]).map_or(data.len(), |e| at + e + 2);
    let after = restarts.iter().filter(|&&p| p > at && p < end).count() as u64;
    // The data after the marker is interval `k`; `after` markers follow.
    let k = intervals.checked_sub(after + 1)?;
    if k == 0 || (k - 1) % 8 != m {
        return None;
    }

    let mut out = h.bytes();
    let mut w = BitWriter {
        out: Vec::new(),
        acc: 0,
        n: 0,
    };
    let grey_mcu: Vec<(u16, u8)> = comps
        .iter()
        .zip(&tables)
        .flat_map(|(c, &(td, ta))| {
            let blocks = usize::from(c.1) * usize::from(c.2);
            std::iter::repeat_n([code(0, td, 0), code(1, ta, 0)], blocks)
        })
        .flatten()
        .collect::<Option<_>>()?;
    for i in 0..k {
        for _ in 0..ri {
            for &(c, l) in &grey_mcu {
                w.put(c, l);
            }
        }
        w.align();
        w.out.extend([0xFF, 0xD0 + (i % 8) as u8]);
        if w.out.len() > 256 << 20 {
            return None;
        }
    }
    out.extend(w.out);
    out.extend(&data[at + 2..end.min(data.len())]);
    finish(out)
}

/// The largest whole JPEG embedded in a photo: its large preview (MPF),
/// EXIF thumbnail, or any other copy that decodes.
fn best_preview(data: &[u8]) -> Option<Vec<u8>> {
    let v = data.to_vec();
    let mut best: Option<Vec<u8>> = None;
    for i in memchr::memmem::find_iter(data, &[0xFF, 0xD8, 0xFF]) {
        if i == 0 {
            continue;
        }
        let Ok(f) = jpeg::walk(&mut Reader::new(&v, 0), i as u64, v.len() as u64) else {
            continue;
        };
        let jpg = &data[i..i + f.len as usize];
        if best.as_ref().is_some_and(|b| b.len() >= jpg.len()) {
            continue;
        }
        if decodes(jpg).is_some() {
            best = Some(jpg.to_vec());
        }
    }
    best
}

// ----------------------------------------------------------------- HEIC

/// A box: type, body range in the data.
fn boxes(data: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut p = from;
    while p + 8 <= to && out.len() < 4096 {
        let size = u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = data[p + 4..p + 8].try_into().unwrap();
        let (size, header) = match size {
            0 => (to - p, 8),
            1 => match data.get(p + 8..p + 16) {
                Some(b) => (
                    usize::try_from(u64::from_be_bytes(b.try_into().unwrap()))
                        .unwrap_or(usize::MAX),
                    16,
                ),
                None => break,
            },
            s => (s, 8),
        };
        if size < header {
            break;
        }
        // A box running past the data (a file cut short) is listed with
        // what there is of it.
        let end = p.saturating_add(size).min(to);
        out.push((typ, p + header, end));
        if p.saturating_add(size) > to {
            break;
        }
        p += size;
    }
    out
}

fn be(d: &[u8], at: usize, n: usize) -> Option<u64> {
    let b = d.get(at..at.checked_add(n)?)?;
    Some(b.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x)))
}

#[derive(Debug, Clone)]
struct ItemInfo {
    id: u32,
    typ: [u8; 4],
    /// The whole `infe` box.
    infe: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct Location {
    method: u8,
    /// (offset, length) pieces; length 0 means "to the end".
    extents: Vec<(u64, u64)>,
}

#[derive(Debug, Default)]
struct Heif {
    ftyp: Vec<u8>,
    hdlr: Vec<u8>,
    primary: u32,
    items: Vec<ItemInfo>,
    locations: std::collections::HashMap<u32, Location>,
    /// (type, from, to list)
    refs: Vec<([u8; 4], u32, Vec<u32>)>,
    /// Property boxes, whole, in order (indexes are 1-based).
    props: Vec<Vec<u8>>,
    /// item -> (essential, property index)
    assoc: std::collections::HashMap<u32, Vec<(bool, u16)>>,
    idat: Vec<u8>,
}

impl Heif {
    fn parse(data: &[u8]) -> Option<Heif> {
        let top = boxes(data, 0, data.len());
        let mut h = Heif::default();
        let &(_, fs, fe) = top.iter().find(|b| &b.0 == b"ftyp")?;
        h.ftyp = data[fs - 8..fe].to_vec();
        let &(_, ms, me) = top.iter().find(|b| &b.0 == b"meta")?;
        for (typ, s, e) in boxes(data, ms + 4, me) {
            let b = &data[s..e];
            match &typ {
                b"hdlr" => h.hdlr = data[s - 8..e].to_vec(),
                b"pitm" => {
                    h.primary = if b.first() == Some(&0) {
                        be(b, 4, 2)?
                    } else {
                        be(b, 4, 4)?
                    } as u32
                }
                b"iinf" => {
                    let start = if b.first() == Some(&0) { 6 } else { 8 };
                    for (t, is, ie) in boxes(data, s + start, e) {
                        if &t != b"infe" {
                            continue;
                        }
                        let ib = &data[is..ie];
                        let v = *ib.first()?;
                        let (id, typ_at) = match v {
                            2 => (be(ib, 4, 2)? as u32, 8),
                            3 => (be(ib, 4, 4)? as u32, 10),
                            _ => continue,
                        };
                        h.items.push(ItemInfo {
                            id,
                            typ: ib.get(typ_at..typ_at + 4)?.try_into().ok()?,
                            infe: data[is - 8..ie].to_vec(),
                        });
                    }
                }
                b"iloc" => h.parse_iloc(b)?,
                b"iref" => {
                    let wide = b.first() != Some(&0);
                    let n = if wide { 4 } else { 2 };
                    for (t, rs, re) in boxes(data, s + 4, e) {
                        let rb = &data[rs..re];
                        let from = be(rb, 0, n)? as u32;
                        let count = be(rb, n, 2)? as usize;
                        let to = (0..count)
                            .map(|k| be(rb, n + 2 + k * n, n).map(|v| v as u32))
                            .collect::<Option<Vec<_>>>()?;
                        h.refs.push((t, from, to));
                    }
                }
                b"iprp" => {
                    for (t, ps, pe) in boxes(data, s, e) {
                        match &t {
                            b"ipco" => {
                                h.props = boxes(data, ps, pe)
                                    .iter()
                                    .map(|&(_, a, z)| data[a - 8..z].to_vec())
                                    .collect()
                            }
                            b"ipma" => h.parse_ipma(&data[ps..pe])?,
                            _ => {}
                        }
                    }
                }
                b"idat" => h.idat = b.to_vec(),
                _ => {}
            }
        }
        (!h.items.is_empty()).then_some(h)
    }

    fn parse_iloc(&mut self, b: &[u8]) -> Option<()> {
        let v = *b.first()?;
        let sizes = be(b, 4, 2)? as u16;
        let (off_sz, len_sz, base_sz) = (
            usize::from(sizes >> 12),
            usize::from((sizes >> 8) & 0xF),
            usize::from((sizes >> 4) & 0xF),
        );
        let idx_sz = if v >= 1 { usize::from(sizes & 0xF) } else { 0 };
        let (count, mut p) = if v < 2 {
            (be(b, 6, 2)?, 8)
        } else {
            (be(b, 6, 4)?, 10)
        };
        for _ in 0..count.min(65536) {
            let id_sz = if v < 2 { 2 } else { 4 };
            let id = be(b, p, id_sz)? as u32;
            p += id_sz;
            let method = if v >= 1 {
                p += 2;
                (be(b, p - 2, 2)? & 0xF) as u8
            } else {
                0
            };
            p += 2; // data reference index
            let base = if base_sz > 0 { be(b, p, base_sz)? } else { 0 };
            p += base_sz;
            let n = be(b, p, 2)?;
            p += 2;
            let mut loc = Location {
                method,
                extents: Vec::new(),
            };
            for _ in 0..n {
                p += idx_sz;
                let off = if off_sz > 0 { be(b, p, off_sz)? } else { 0 };
                p += off_sz;
                let len = if len_sz > 0 { be(b, p, len_sz)? } else { 0 };
                p += len_sz;
                loc.extents.push((base.checked_add(off)?, len));
            }
            self.locations.insert(id, loc);
        }
        Some(())
    }

    fn parse_ipma(&mut self, b: &[u8]) -> Option<()> {
        let v = *b.first()?;
        let wide = b.get(3)? & 1 == 1;
        let count = be(b, 4, 4)?;
        let mut p = 8;
        for _ in 0..count.min(65536) {
            let id_sz = if v < 1 { 2 } else { 4 };
            let id = be(b, p, id_sz)? as u32;
            p += id_sz;
            let n = *b.get(p)?;
            p += 1;
            let mut list = Vec::new();
            for _ in 0..n {
                let (ess, idx) = if wide {
                    let x = be(b, p, 2)? as u16;
                    p += 2;
                    (x & 0x8000 != 0, x & 0x7FFF)
                } else {
                    let x = *b.get(p)?;
                    p += 1;
                    (x & 0x80 != 0, u16::from(x & 0x7F))
                };
                list.push((ess, idx));
            }
            self.assoc.insert(id, list);
        }
        Some(())
    }

    /// An item's bytes, if all of them are in the file.
    fn item_data(&self, data: &[u8], id: u32) -> Option<Vec<u8>> {
        let loc = self.locations.get(&id)?;
        let src: &[u8] = match loc.method {
            0 => data,
            1 => &self.idat,
            _ => return None,
        };
        let mut out = Vec::new();
        for &(off, len) in &loc.extents {
            let s = usize::try_from(off).ok()?;
            let e = if len == 0 {
                src.len()
            } else {
                s.checked_add(usize::try_from(len).ok()?)?
            };
            out.extend(src.get(s..e)?);
        }
        Some(out)
    }

    fn item(&self, id: u32) -> Option<&ItemInfo> {
        self.items.iter().find(|i| i.id == id)
    }

    fn refs_from(&self, typ: &[u8; 4], from: u32) -> Vec<u32> {
        self.refs
            .iter()
            .filter(|r| &r.0 == typ && r.1 == from)
            .flat_map(|r| r.2.clone())
            .collect()
    }
}

/// An item of a rebuilt HEIF.
struct NewItem {
    info: Vec<u8>,
    id: u32,
    data: Vec<u8>,
    props: Vec<(bool, u16)>,
}

fn full(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(((12 + body.len()) as u32).to_be_bytes());
    v.extend(typ);
    v.push(version);
    v.extend(&flags.to_be_bytes()[1..]);
    v.extend(body);
    v
}

fn plain(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(((8 + body.len()) as u32).to_be_bytes());
    v.extend(typ);
    v.extend(body);
    v
}

/// Writes a HEIF with the given items (all stored in its `mdat`).
fn write_heif(
    h: &Heif,
    primary: u32,
    items: &[NewItem],
    refs: &[([u8; 4], u32, Vec<u32>)],
    props: &[Vec<u8>],
) -> Vec<u8> {
    let mut iinf = (items.len() as u16).to_be_bytes().to_vec();
    for it in items {
        iinf.extend(&it.info);
    }
    let iinf = full(b"iinf", 0, 0, &iinf);
    let pitm = full(b"pitm", 1, 0, &primary.to_be_bytes());
    let mut iref = Vec::new();
    for (t, from, to) in refs {
        let mut body = from.to_be_bytes().to_vec();
        body.extend((to.len() as u16).to_be_bytes());
        for x in to {
            body.extend(x.to_be_bytes());
        }
        iref.extend(plain(t, &body));
    }
    let iref = if refs.is_empty() {
        Vec::new()
    } else {
        full(b"iref", 1, 0, &iref)
    };
    let ipco = plain(b"ipco", &props.concat());
    let mut ipma = (items.len() as u32).to_be_bytes().to_vec();
    for it in items {
        ipma.extend(it.id.to_be_bytes());
        ipma.push(it.props.len() as u8);
        for &(ess, idx) in &it.props {
            ipma.extend((idx | if ess { 0x8000 } else { 0 }).to_be_bytes());
        }
    }
    let ipma = full(b"ipma", 1, 1, &ipma);
    let mut iprp = ipco;
    iprp.extend(ipma);
    let iprp = plain(b"iprp", &iprp);

    // iloc version 1: 4-byte offsets and lengths, no base offset.
    let iloc_len = 12 + 4 + items.len() * 16;
    let meta_len = 12 + h.hdlr.len() + pitm.len() + iloc_len + iinf.len() + iref.len() + iprp.len();
    let mut data_at = (h.ftyp.len() + meta_len + 8) as u32;
    let mut iloc = vec![0x44, 0x00];
    iloc.extend((items.len() as u16).to_be_bytes());
    for it in items {
        iloc.extend(it.id.to_be_bytes()[2..].iter());
        iloc.extend([0, 0, 0, 0]); // construction method 0, data reference 0
        iloc.extend(1u16.to_be_bytes());
        iloc.extend(data_at.to_be_bytes());
        iloc.extend((it.data.len() as u32).to_be_bytes());
        data_at += it.data.len() as u32;
    }
    // Item IDs must fit iloc version 1's 16-bit field.
    let iloc = full(b"iloc", 1, 0, &iloc);
    debug_assert_eq!(iloc.len(), iloc_len);

    let mut meta = h.hdlr.clone();
    meta.extend(pitm);
    meta.extend(iloc);
    meta.extend(iinf);
    meta.extend(iref);
    meta.extend(iprp);
    let meta = full(b"meta", 0, 0, &meta);
    let mut out = h.ftyp.clone();
    out.extend(meta);
    let body: Vec<u8> = items.iter().flat_map(|i| i.data.iter().copied()).collect();
    out.extend(plain(b"mdat", &body));
    out
}

pub fn repair_heic(data: &[u8]) -> Option<(Vec<u8>, PhotoRepair)> {
    let h = Heif::parse(data)?;
    // iloc version 1 holds 16-bit item IDs.
    if h.items.iter().any(|i| i.id > 0xFFFF) {
        return None;
    }
    let primary = h.item(h.primary)?;
    if &primary.typ == b"grid" {
        let tiles = h.refs_from(b"dimg", h.primary);
        if h.item_data(data, h.primary).is_some()
            && !tiles.is_empty()
            && tiles.iter().all(|&t| h.item_data(data, t).is_some())
        {
            // The picture is whole; nothing to repair here.
            return None;
        }
        if let Some(out) = heic_rows(data, &h) {
            return Some((out, PhotoRepair::HeicPartial));
        }
    } else if h.item_data(data, h.primary).is_some() {
        return None;
    }
    let thumb = h
        .refs
        .iter()
        .find(|r| &r.0 == b"thmb" && r.2.contains(&h.primary))
        .map(|r| r.1)?;
    let info = h.item(thumb)?;
    let item = NewItem {
        info: info.infe.clone(),
        id: thumb,
        data: h.item_data(data, thumb)?,
        props: h.assoc.get(&thumb).cloned().unwrap_or_default(),
    };
    let out = write_heif(&h, thumb, &[item], &[], &h.props);
    Some((out, PhotoRepair::HeicThumbnail))
}

/// A grid picture cut short, as a shorter grid of its complete rows.
fn heic_rows(data: &[u8], h: &Heif) -> Option<Vec<u8>> {
    let grid = h.item_data(data, h.primary)?;
    let wide = grid.get(1)? & 1 == 1;
    let rows = usize::from(*grid.get(2)?) + 1;
    let cols = usize::from(*grid.get(3)?) + 1;
    let n = if wide { 4 } else { 2 };
    let out_w = be(&grid, 4, n)?;
    let out_h = be(&grid, 4 + n, n)?;
    let tiles = h.refs_from(b"dimg", h.primary);
    if tiles.len() != rows * cols {
        return None;
    }
    let whole = tiles
        .iter()
        .take_while(|&&t| h.item_data(data, t).is_some())
        .count();
    let keep_rows = whole / cols;
    if keep_rows == 0 || keep_rows == rows {
        return None;
    }
    // Tile height from the first tile's size property.
    let tile_h = h
        .assoc
        .get(&tiles[0])?
        .iter()
        .filter_map(|&(_, i)| h.props.get(usize::from(i).checked_sub(1)?))
        .find(|p| p.get(4..8) == Some(b"ispe"))
        .and_then(|p| be(p, 16, 4))?;
    let new_h = out_h.min(tile_h * keep_rows as u64);

    let mut props = h.props.clone();
    let mut ispe = Vec::new();
    ispe.extend((out_w as u32).to_be_bytes());
    ispe.extend((new_h as u32).to_be_bytes());
    props.push(full(b"ispe", 0, 0, &ispe));
    let new_ispe = props.len() as u16;
    let grid_props: Vec<(bool, u16)> = h
        .assoc
        .get(&h.primary)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(e, i)| {
            let is_ispe = h
                .props
                .get(usize::from(i).wrapping_sub(1))
                .is_some_and(|p| p.get(4..8) == Some(b"ispe"));
            (e, if is_ispe { new_ispe } else { i })
        })
        .collect();
    let mut new_grid = vec![0, u8::from(wide), (keep_rows - 1) as u8, (cols - 1) as u8];
    if wide {
        new_grid.extend((out_w as u32).to_be_bytes());
        new_grid.extend((new_h as u32).to_be_bytes());
    } else {
        new_grid.extend((out_w as u16).to_be_bytes());
        new_grid.extend((new_h as u16).to_be_bytes());
    }

    let mut items = vec![NewItem {
        info: h.item(h.primary)?.infe.clone(),
        id: h.primary,
        data: new_grid,
        props: grid_props,
    }];
    let kept = &tiles[..keep_rows * cols];
    for &t in kept {
        items.push(NewItem {
            info: h.item(t)?.infe.clone(),
            id: t,
            data: h.item_data(data, t)?,
            props: h.assoc.get(&t).cloned().unwrap_or_default(),
        });
    }
    let mut refs = vec![(*b"dimg", h.primary, kept.to_vec())];
    // Metadata about the picture (EXIF: its orientation) comes along when whole.
    for r in h
        .refs
        .iter()
        .filter(|r| &r.0 == b"cdsc" && r.2.contains(&h.primary))
    {
        if let (Some(info), Some(d)) = (h.item(r.1), h.item_data(data, r.1)) {
            items.push(NewItem {
                info: info.infe.clone(),
                id: r.1,
                data: d,
                props: h.assoc.get(&r.1).cloned().unwrap_or_default(),
            });
            refs.push((*b"cdsc", r.1, vec![h.primary]));
        }
    }
    Some(write_heif(h, h.primary, &items, &refs, &props))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(w: u32, h: u32, restart: bool) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([
                (x * 3 % 256) as u8,
                (y * 5 % 256) as u8,
                ((x + y) % 256) as u8,
            ])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&img)
            .unwrap();
        if restart {
            // Re-encode with restart markers is not offered by the encoder;
            // tests needing them use the scripts' camera-like files.
        }
        out
    }

    #[test]
    fn keeps_the_surviving_part() {
        let p = photo(320, 240, false);
        assert!(repair_jpeg(&p, &[]).is_none());
        let mut cut = p[..p.len() / 2].to_vec();
        cut.extend(vec![0u8; 3000]);
        let (fixed, how) = repair_jpeg(&cut, &[]).unwrap();
        assert_eq!(how, PhotoRepair::Partial);
        assert_eq!(decodes(&fixed), Some((320, 240)));
    }

    #[test]
    fn transplants_a_lost_header() {
        let reference = photo(320, 240, false);
        let damaged = photo(320, 240, false);
        let h = Header::parse(&damaged).unwrap();
        // Tables wiped; the scan header and data survive.
        let mut bad = damaged.clone();
        for b in &mut bad[2..h.scan_start - 14] {
            *b = 0;
        }
        let (fixed, how) = repair_jpeg(&bad, &[&reference]).unwrap();
        assert_eq!(how, PhotoRepair::Header);
        assert_eq!(decodes(&fixed), Some((320, 240)));
        // No reference: nothing to transplant.
        assert!(repair_jpeg(&bad, &[]).is_none());
    }

    #[test]
    fn rescues_an_embedded_preview() {
        let preview = photo(160, 120, false);
        let mut p = photo(640, 480, false);
        // Overwrite the main picture's header and data, keep the preview
        // that follows it.
        for b in &mut p[..400] {
            *b = 0x5A;
        }
        p.extend(&preview);
        let (fixed, how) = repair_jpeg(&p, &[]).unwrap();
        assert_eq!(how, PhotoRepair::Preview);
        assert_eq!(decodes(&fixed), Some((160, 120)));
    }

    #[test]
    fn survives_garbage() {
        let junk: Vec<u8> = (0..50_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let mut j = vec![0xFF, 0xD8];
        j.extend(&junk);
        let _ = repair(&j, &[&junk]);
        let mut heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
        heic.extend(&junk);
        let _ = repair(&heic, &[]);
    }

    /// A 2x2 grid of 512-pixel tiles, with a thumbnail stored before them.
    fn synthetic_heic() -> Vec<u8> {
        let infe = |id: u16, typ: &[u8; 4]| {
            let mut b = id.to_be_bytes().to_vec();
            b.extend([0, 0]);
            b.extend(typ);
            b.push(0);
            full(b"infe", 2, 0, &b)
        };
        let ispe = |w: u32, h: u32| {
            let mut b = w.to_be_bytes().to_vec();
            b.extend(h.to_be_bytes());
            full(b"ispe", 0, 0, &b)
        };
        let h = Heif {
            ftyp: plain(b"ftyp", b"heic\0\0\0\0mif1heic"),
            hdlr: full(
                b"hdlr",
                0,
                0,
                &[&[0u8; 4][..], b"pict", &[0; 12], b"\0"].concat(),
            ),
            ..Default::default()
        };
        let item = |id: u16, typ: &[u8; 4], data: Vec<u8>, props: Vec<(bool, u16)>| NewItem {
            info: infe(id, typ),
            id: u32::from(id),
            data,
            props,
        };
        let mut items = vec![
            item(
                1,
                b"grid",
                vec![0, 0, 1, 1, 0x04, 0, 0x04, 0],
                vec![(false, 2)],
            ),
            item(6, b"hvc1", vec![0x77; 3000], vec![(false, 3)]),
        ];
        for t in 2..=5u16 {
            items.push(item(t, b"hvc1", vec![t as u8; 5000], vec![(false, 1)]));
        }
        let refs = vec![(*b"dimg", 1, vec![2, 3, 4, 5]), (*b"thmb", 6, vec![1])];
        write_heif(
            &h,
            1,
            &items,
            &refs,
            &[ispe(512, 512), ispe(1024, 1024), ispe(160, 160)],
        )
    }

    #[test]
    fn keeps_whole_rows_of_a_cut_heic() {
        let whole = synthetic_heic();
        assert!(repair(&whole, &[]).is_none());
        // Cut inside the last tile: the first row is whole.
        let cut = &whole[..whole.len() - 2500];
        let (fixed, how) = repair(cut, &[]).unwrap();
        assert_eq!(how, PhotoRepair::HeicPartial);
        let h = Heif::parse(&fixed).unwrap();
        let grid = h.item_data(&fixed, h.primary).unwrap();
        assert_eq!((grid[2], grid[3]), (0, 1), "one row of two tiles");
        assert_eq!(h.refs_from(b"dimg", h.primary), vec![2, 3]);
        assert!(h.item_data(&fixed, 3).is_some());
        // The picture's new height is one tile's.
        let ispe = h.assoc[&h.primary]
            .iter()
            .filter_map(|&(_, i)| h.props.get(usize::from(i) - 1))
            .find(|p| p.get(4..8) == Some(b"ispe"))
            .unwrap();
        assert_eq!(be(ispe, 16, 4), Some(512));
    }

    #[test]
    fn rescues_the_thumbnail_of_a_heic_cut_early() {
        let whole = synthetic_heic();
        // Cut inside the first tile: no row is whole, the thumbnail is.
        let cut = &whole[..whole.len() - 17_000];
        let (fixed, how) = repair(cut, &[]).unwrap();
        assert_eq!(how, PhotoRepair::HeicThumbnail);
        let h = Heif::parse(&fixed).unwrap();
        assert_eq!(h.primary, 6);
        assert_eq!(h.item_data(&fixed, 6).unwrap(), vec![0x77; 3000]);
    }
}

//! TrueType and OpenType fonts, and TrueType collections. A font is a
//! directory of tables, each with an offset and length; the font ends
//! with its last table, padded to four bytes. The directory's search
//! fields are derived from its table count and the `head` table carries
//! a fixed magic number, which together rule out data that merely starts
//! with the common bytes `00 01 00 00`.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Font;

/// More tables than any real font has.
const MAX_TABLES: u16 = 128;
const MAX_FONTS: u32 = 256;

impl Format for Font {
    fn name(&self) -> &'static str {
        "font"
    }

    fn max_len(&self) -> u64 {
        256 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        if &head[..4] == b"ttcf" {
            let version = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
            let count = u32::from_be_bytes([head[8], head[9], head[10], head[11]]);
            return matches!(version, 0x0001_0000 | 0x0002_0000)
                && (1..=MAX_FONTS).contains(&count);
        }
        is_sfnt(&head[..4]) && search_fields_ok(&head[4..12])
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let tag = r.array::<4>()?;
        let (mut end, ext) = if &tag == b"ttcf" {
            let version = r.u32be()?;
            let count = r.u32be()?;
            if !(1..=MAX_FONTS).contains(&count) {
                return None;
            }
            let mut offsets = Vec::with_capacity(count as usize);
            for _ in 0..count {
                offsets.push(u64::from(r.u32be()?));
            }
            let mut end = 12 + 4 * u64::from(count);
            if version == 0x0002_0000 {
                // A digital signature table may follow the fonts.
                let dsig = r.array::<12>()?;
                end += 12;
                if &dsig[..4] == b"DSIG" {
                    let len = u64::from(u32::from_be_bytes(dsig[4..8].try_into().ok()?));
                    let off = u64::from(u32::from_be_bytes(dsig[8..12].try_into().ok()?));
                    end = end.max(off + len);
                }
            } else if version != 0x0001_0000 {
                return None;
            }
            for off in offsets {
                end = end.max(sfnt_end(r, start, off, limit)?.0);
            }
            (end, "ttc")
        } else {
            let (end, cff) = sfnt_end(r, start, 0, limit)?;
            (end, if cff { "otf" } else { "ttf" })
        };
        // The last table is padded to four bytes with zeros, except in the
        // odd font that stops short.
        let pad = end.next_multiple_of(4) - end;
        if pad > 0 {
            r.seek(start + end);
            if r.bytes(pad as usize)
                .is_some_and(|b| b.iter().all(|&x| x == 0))
            {
                end += pad;
            }
        }
        (start.checked_add(end)? <= limit).then_some(Found {
            len: end,
            ext,
            kind: Kind::Other,
            taken: None,
        })
    }
}

fn is_sfnt(tag: &[u8]) -> bool {
    matches!(tag, [0, 1, 0, 0] | b"OTTO" | b"true")
}

/// The binary-search fields after the table count, which every writer
/// derives from it.
fn search_fields_ok(f: &[u8]) -> bool {
    let n = u16::from_be_bytes([f[0], f[1]]);
    if n == 0 || n > MAX_TABLES {
        return false;
    }
    let log = 15 - n.leading_zeros() as u16;
    let range = (1u16 << log) * 16;
    f[2..8]
        == [
            range.to_be_bytes(),
            log.to_be_bytes(),
            (n * 16 - range).to_be_bytes(),
        ]
        .concat()[..]
}

/// The end of the single font whose table directory is at `start + off`,
/// relative to `start`, and whether it has PostScript (CFF) outlines.
fn sfnt_end(r: &mut Reader, start: u64, off: u64, limit: u64) -> Option<(u64, bool)> {
    r.seek(start.checked_add(off)?);
    let dir = r.array::<12>()?;
    if !is_sfnt(&dir[..4]) || !search_fields_ok(&dir[4..12]) {
        return None;
    }
    let n = usize::from(u16::from_be_bytes([dir[4], dir[5]]));
    let records = r.bytes(16 * n)?;
    let mut end = off + 12 + 16 * n as u64;
    let mut tags = Vec::with_capacity(n);
    let mut head = None;
    let mut cff = false;
    for rec in records.chunks_exact(16) {
        let tag: [u8; 4] = rec[..4].try_into().ok()?;
        let at = u64::from(u32::from_be_bytes(rec[8..12].try_into().ok()?));
        let len = u64::from(u32::from_be_bytes(rec[12..16].try_into().ok()?));
        if !tag.iter().all(|b| (0x20..0x7F).contains(b)) || tags.contains(&tag) || at < 12 {
            return None;
        }
        if &tag == b"head" || &tag == b"bhed" {
            head = Some((at, len));
        }
        cff |= &tag == b"CFF " || &tag == b"CFF2";
        tags.push(tag);
        end = end.max(at + len);
    }
    if start + end > limit {
        return None;
    }
    let (at, len) = head?;
    r.seek(start + at + 12);
    (len >= 54 && r.u32be()? == 0x5F0F_3CF5).then_some((end, cff))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A font with a `head` table and one other, `body_len` bytes long.
    pub(crate) fn build(body_len: usize) -> Vec<u8> {
        let mut d = vec![0, 1, 0, 0, 0, 2, 0, 32, 0, 1, 0, 0];
        let head_at = 12 + 32;
        let body_at = head_at + 56;
        for (tag, at, len) in [(b"cmap", body_at, body_len), (b"head", head_at, 54)] {
            d.extend(tag);
            d.extend([0; 4]);
            d.extend((at as u32).to_be_bytes());
            d.extend((len as u32).to_be_bytes());
        }
        let mut head = vec![0u8; 56];
        head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
        d.extend(head);
        d.extend(vec![0x21; body_len]);
        d.resize(d.len().next_multiple_of(4), 0);
        d
    }

    #[test]
    fn ends_with_the_last_table() {
        let carve = |d: &Vec<u8>| {
            Font.carve(&mut Reader::new(d, 0), 0, d.len() as u64)
                .map(|f| (f.len, f.ext))
        };
        let good = build(1001);
        let mut d = good.clone();
        assert!(Font.matches(&[d.clone(), vec![0; 512]].concat()));
        d.extend([0x77; 800]);
        assert_eq!(carve(&d), Some((good.len() as u64, "ttf")));
        // Unpadded and followed by other data.
        let mut short = good[..good.len() - 3].to_vec();
        short.extend([0x77; 800]);
        assert_eq!(carve(&short), Some((good.len() as u64 - 3, "ttf")));
        d[12 + 32 + 12] ^= 1; // the head table's magic number
        assert_eq!(carve(&d), None);
    }
}

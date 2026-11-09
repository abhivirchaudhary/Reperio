//! RAR archives, versions 4 and 5. Both are a signature followed by
//! headers, each protected by a CRC and saying how much data follows it,
//! up to an end-of-archive header. Archives whose headers are encrypted
//! cannot be walked without the password and are not carved.

use super::crc::crc32;
use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Rar;

const SIG4: &[u8; 7] = b"Rar!\x1a\x07\x00";
const SIG5: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";

impl Format for Rar {
    fn name(&self) -> &'static str {
        "rar"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        // A version 4 archive opens with its main header (type 0x73).
        (head[..7] == SIG4[..] && head[9] == 0x73) || head[..8] == SIG5[..]
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let sig = r.array::<8>()?;
        let end = if sig == *SIG5 {
            walk5(r, start + 8, limit)?
        } else if sig[..7] == SIG4[..] {
            walk4(r, start + 7, limit)?
        } else {
            return None;
        };
        (end <= limit).then_some(Found {
            len: end - start,
            ext: "rar",
            kind: Kind::Other,
            taken: None,
        })
    }
}

/// Walks version 4 blocks from `pos` to just past the end-of-archive block.
fn walk4(r: &mut Reader, mut pos: u64, limit: u64) -> Option<u64> {
    let mut first = true;
    while pos < limit {
        r.seek(pos);
        let base = r.array::<7>()?;
        let crc = u16::from_le_bytes([base[0], base[1]]);
        let typ = base[2];
        let flags = u16::from_le_bytes([base[3], base[4]]);
        let size = usize::from(u16::from_le_bytes([base[5], base[6]]));
        if size < 7 || !(0x73..=0x7B).contains(&typ) || first != (typ == 0x73) {
            return None;
        }
        first = false;
        r.seek(pos);
        let h = r.bytes(size)?;
        let le32 = |o: usize| -> Option<u64> {
            Some(u64::from(u32::from_le_bytes(
                h.get(o..o + 4)?.try_into().ok()?,
            )))
        };
        // Which bytes the CRC covers depends on the block: old archives
        // keep comments inside headers but outside their CRC.
        let covered = match typ {
            0x73 => Some(if flags & 0x0200 != 0 { 14 } else { 13 }.min(size)),
            0x74 if flags & 0x0008 != 0 => None,
            0x77 => None,
            _ => Some(size),
        };
        if let Some(n) = covered {
            if crc32(&h[2..n]) as u16 != crc {
                return None;
            }
        }
        let data = match typ {
            // Main header with encrypted headers after it.
            0x73 if flags & 0x0080 != 0 => return None,
            0x74 | 0x7A => {
                let high = if flags & 0x0100 != 0 { le32(32)? } else { 0 };
                le32(7)? | high << 32
            }
            _ if flags & 0x8000 != 0 => le32(7)?,
            _ => 0,
        };
        pos = pos.checked_add(size as u64)?;
        if typ == 0x7B {
            return Some(pos);
        }
        pos = pos.checked_add(data)?;
    }
    None
}

/// Walks version 5 headers from `pos` to just past the end-of-archive
/// header.
fn walk5(r: &mut Reader, mut pos: u64, limit: u64) -> Option<u64> {
    let mut first = true;
    while pos < limit {
        r.seek(pos);
        let crc = r.u32le()?;
        let size_at = r.pos();
        let size = vint(r)?;
        let size_len = (r.pos() - size_at) as usize;
        // The format caps headers at 2 MB.
        if size == 0 || size > 2 << 20 {
            return None;
        }
        r.seek(size_at);
        let h = r.bytes(size_len + size as usize)?;
        if crc32(&h) != crc {
            return None;
        }
        let mut body = &h[size_len..];
        let typ = take_vint(&mut body)?;
        let flags = take_vint(&mut body)?;
        if flags & 0x0001 != 0 {
            take_vint(&mut body)?; // extra area size
        }
        let data = if flags & 0x0002 != 0 {
            take_vint(&mut body)?
        } else {
            0
        };
        let known = (1..=5).contains(&typ);
        if first != (typ == 1) || typ == 4 || (!known && flags & 0x0004 == 0) {
            return None;
        }
        first = false;
        pos = r.pos().checked_add(data)?;
        if typ == 5 {
            let mut end = r.pos();
            // A volume other than the last is padded with a few zeros to
            // the size asked for.
            if take_vint(&mut body).is_some_and(|f| f & 0x0001 != 0) {
                for _ in 0..16 {
                    if end >= limit || r.u8() != Some(0) {
                        break;
                    }
                    end += 1;
                }
            }
            return Some(end);
        }
    }
    None
}

/// A RAR 5 variable-length integer: seven bits per byte, low bits first.
fn vint(r: &mut Reader) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = r.u8()?;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn take_vint(s: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&b, rest) = s.split_first()?;
        *s = rest;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn block4(typ: u8, flags: u16, body: &[u8], data: &[u8]) -> Vec<u8> {
        let mut h = vec![typ];
        h.extend(flags.to_le_bytes());
        h.extend((7 + body.len() as u16).to_le_bytes());
        h.extend(body);
        let mut out = (crc32(&h) as u16).to_le_bytes().to_vec();
        out.extend(h);
        out.extend(data);
        out
    }

    pub(crate) fn rar4() -> Vec<u8> {
        let data = vec![0x33; 5000];
        let mut body = Vec::new();
        body.extend((data.len() as u32).to_le_bytes());
        body.extend((data.len() as u32).to_le_bytes());
        body.extend([2, 0, 0, 0, 0, 0, 0, 0, 0, 20, 0x30]);
        body.extend(5u16.to_le_bytes());
        body.extend(0x20u32.to_le_bytes());
        body.extend(b"a.txt");
        let mut out = SIG4.to_vec();
        out.extend(block4(0x73, 0, &[0; 6], &[]));
        out.extend(block4(0x74, 0x8000, &body, &data));
        out.extend(block4(0x7B, 0x4000, &[], &[]));
        out
    }

    fn header5(typ: u8, flags: u8, extra: &[u8]) -> Vec<u8> {
        let mut body = vec![typ, flags];
        body.extend(extra);
        let mut h = vec![body.len() as u8];
        h.extend(body);
        let mut out = crc32(&h).to_le_bytes().to_vec();
        out.extend(h);
        out
    }

    pub(crate) fn rar5() -> Vec<u8> {
        let mut out = SIG5.to_vec();
        out.extend(header5(1, 0, &[0]));
        // A file header with a 300-byte data area (300 = 0xAC 0x02).
        out.extend(header5(2, 0x02, &[0xAC, 0x02, 0, 0, 0, 0]));
        out.extend([0x44; 300]);
        out.extend(header5(5, 0, &[0]));
        out
    }

    fn carve(data: &[u8]) -> Option<u64> {
        let data = data.to_vec();
        Rar.carve(&mut Reader::new(&data, 0), 0, data.len() as u64)
            .map(|f| f.len)
    }

    #[test]
    fn carves_both_versions() {
        for good in [rar4(), rar5()] {
            assert!(Rar.matches(&[good.clone(), vec![0; 512]].concat()));
            let mut data = good.clone();
            data.extend([0x91; 700]);
            assert_eq!(carve(&data), Some(good.len() as u64));
            // A damaged header fails its CRC.
            let mut bad = data.clone();
            bad[good.len() - 3] ^= 0x10;
            assert_eq!(carve(&bad), None);
            assert_eq!(carve(&good[..good.len() - 1]), None);
        }
    }
}

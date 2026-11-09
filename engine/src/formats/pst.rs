//! Outlook data files: PST mailboxes and OST offline caches, in both the
//! Unicode layout (Outlook 2003 on) and the older ANSI one. The header
//! records the file's end in its ROOT structure and carries CRCs over
//! itself, so a header that checks out gives the exact length.

use super::crc::crc32_update;
use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Pst;

impl Format for Pst {
    fn name(&self) -> &'static str {
        "pst"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let version = u16::from_le_bytes([head[10], head[11]]);
        &head[..4] == b"!BDN"
            && matches!(&head[8..10], b"SM" | b"SO")
            && matches!(version, 14 | 15 | 23 | 36)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let h = r.array::<564>()?;
        if !self.matches(&h) {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
        let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap_or_default());
        // Outlook's CRC is the plain table CRC-32 without inversion.
        let crc = |n: usize| crc32_update(0, &h[8..8 + n]);
        let unicode = u16::from_le_bytes([h[10], h[11]]) >= 23;
        let (eof, amap_last, sentinel, crypt) = if unicode {
            (u64_at(184), u64_at(192), h[512], h[513])
        } else {
            (
                u64::from(u32_at(168)),
                u64::from(u32_at(172)),
                h[460],
                h[461],
            )
        };
        let valid = crc(471) == u32_at(4)
            && (!unicode || crc(516) == u32_at(524))
            && sentinel == 0x80
            && matches!(crypt, 0 | 1 | 2 | 0x10)
            && eof >= 564
            && amap_last < eof;
        if !valid {
            return None;
        }
        let end = start.checked_add(eof)?;
        (end <= limit).then_some(Found {
            len: eof,
            ext: if &h[8..10] == b"SO" { "ost" } else { "pst" },
            kind: Kind::Other,
            taken: None,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn build(unicode: bool) -> Vec<u8> {
        let eof = 0x4400 + 1024usize;
        let mut h = vec![0u8; eof];
        h[..4].copy_from_slice(b"!BDN");
        h[8..10].copy_from_slice(b"SM");
        h[10] = if unicode { 23 } else { 14 };
        h[12] = 19;
        if unicode {
            h[184..192].copy_from_slice(&(eof as u64).to_le_bytes());
            h[512] = 0x80;
            let full = crc32_update(0, &h[8..524]);
            h[524..528].copy_from_slice(&full.to_le_bytes());
        } else {
            h[168..172].copy_from_slice(&(eof as u32).to_le_bytes());
            h[460] = 0x80;
        }
        let partial = crc32_update(0, &h[8..479]);
        h[4..8].copy_from_slice(&partial.to_le_bytes());
        h
    }

    #[test]
    fn reads_the_end_of_file() {
        for unicode in [true, false] {
            let good = build(unicode);
            let mut d = good.clone();
            d.extend([0x31; 900]);
            let carve = |d: &Vec<u8>| {
                Pst.carve(&mut Reader::new(d, 0), 0, d.len() as u64)
                    .map(|f| f.len)
            };
            assert_eq!(carve(&d), Some(good.len() as u64));
            d[100] ^= 4;
            assert_eq!(carve(&d), None);
        }
    }
}

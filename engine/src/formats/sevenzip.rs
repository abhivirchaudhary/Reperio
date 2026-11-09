//! 7z archives. The 32-byte signature header says where the archive's
//! index (the "next header") lies and how long it is, with a CRC over
//! those numbers and another over the index; the index is the last thing
//! in the archive.

use super::crc::{crc32, crc32_update};
use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct SevenZip;

const SIGNATURE: &[u8; 6] = b"7z\xBC\xAF\x27\x1C";

impl Format for SevenZip {
    fn name(&self) -> &'static str {
        "7z"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        head[..6] == SIGNATURE[..] && head[6] == 0
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let h = r.array::<32>()?;
        let stored = u32::from_le_bytes(h[8..12].try_into().ok()?);
        if !self.matches(&h) || crc32(&h[12..32]) != stored {
            return None;
        }
        let offset = u64::from_le_bytes(h[12..20].try_into().ok()?);
        let size = u64::from_le_bytes(h[20..28].try_into().ok()?);
        let index_crc = u32::from_le_bytes(h[28..32].try_into().ok()?);
        // An index over 1 GB is not a real archive's.
        if size == 0 || size > 1 << 30 {
            return None;
        }
        let index = start.checked_add(32)?.checked_add(offset)?;
        let end = index.checked_add(size)?;
        if end > limit {
            return None;
        }
        // The index is a header (1) or a compressed header (0x17).
        r.seek(index);
        let mut crc = !0;
        let mut left = size;
        let mut buf = vec![0u8; size.min(64 << 10) as usize];
        while left > 0 {
            let n = buf.len().min(left as usize);
            r.read_exact(&mut buf[..n])?;
            if left == size && !matches!(buf[0], 0x01 | 0x17) {
                return None;
            }
            crc = crc32_update(crc, &buf[..n]);
            left -= n as u64;
        }
        (!crc == index_crc).then_some(Found {
            len: end - start,
            ext: "7z",
            kind: Kind::Other,
            taken: None,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn build() -> Vec<u8> {
        let body = vec![0x6B; 3000];
        let index = [0x17, 0x06, 0x80, 0x00, 0x01, 0x09, 0x20, 0x00];
        let mut tail = Vec::new();
        tail.extend((body.len() as u64).to_le_bytes());
        tail.extend((index.len() as u64).to_le_bytes());
        tail.extend(crc32(&index).to_le_bytes());
        let mut out = SIGNATURE.to_vec();
        out.extend([0, 4]);
        out.extend(crc32(&tail).to_le_bytes());
        out.extend(tail);
        out.extend(body);
        out.extend(index);
        out
    }

    #[test]
    fn ends_after_the_index() {
        let good = build();
        let mut data = good.clone();
        data.extend([0x11; 600]);
        let carve = |d: &Vec<u8>| {
            SevenZip
                .carve(&mut Reader::new(d, 0), 0, d.len() as u64)
                .map(|f| f.len)
        };
        assert_eq!(carve(&data), Some(good.len() as u64));
        let mut bad = data.clone();
        *bad.get_mut(good.len() - 2).unwrap() ^= 1;
        assert_eq!(carve(&bad), None);
        bad = data.clone();
        bad[14] ^= 1;
        assert_eq!(carve(&bad), None);
    }
}

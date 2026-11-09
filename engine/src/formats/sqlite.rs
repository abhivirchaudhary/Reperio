//! SQLite 3 databases, as kept by phones, browsers and many apps. The
//! header records the page size and the number of pages, but the page
//! count is only trustworthy when the "version valid for" number matches
//! the change counter: older writers left it stale. Without that match
//! the length is unknown and the database is not carved, rather than
//! guessed.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Sqlite;

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

impl Format for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        head[..16] == MAGIC[..] && head[21..24] == [64, 32, 32]
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let h = r.array::<101>()?;
        let be32 = |o: usize| u32::from_be_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
        let page_size = match u16::from_be_bytes([h[16], h[17]]) {
            1 => 65536,
            n if n >= 512 && n.is_power_of_two() => u64::from(n),
            _ => return None,
        };
        let pages = be32(28);
        let valid = self.matches(&h)
            && matches!(h[18], 1 | 2)
            && matches!(h[19], 1 | 2)
            && page_size - u64::from(h[20]) >= 480
            && pages > 0
            && be32(92) == be32(24)
            && be32(32) <= pages // first freelist trunk page
            && be32(36) < pages // freelist page count
            && be32(44) <= 4 // schema format
            && be32(56) <= 3 // text encoding
            && h[72..92].iter().all(|&b| b == 0)
            // Page 1 holds the schema table's B-tree root.
            && matches!(h[100], 0x05 | 0x0D);
        if !valid {
            return None;
        }
        let end = start.checked_add(page_size * u64::from(pages))?;
        (end <= limit).then_some(Found {
            len: end - start,
            ext: "sqlite",
            kind: Kind::Other,
            taken: None,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn build() -> Vec<u8> {
        let mut d = vec![0u8; 4096 * 3];
        d[..16].copy_from_slice(MAGIC);
        d[16..18].copy_from_slice(&4096u16.to_be_bytes());
        d[18] = 1;
        d[19] = 1;
        d[21..24].copy_from_slice(&[64, 32, 32]);
        d[24..28].copy_from_slice(&7u32.to_be_bytes());
        d[28..32].copy_from_slice(&3u32.to_be_bytes());
        d[44..48].copy_from_slice(&4u32.to_be_bytes());
        d[56..60].copy_from_slice(&1u32.to_be_bytes());
        d[92..96].copy_from_slice(&7u32.to_be_bytes());
        d[100] = 0x0D;
        d
    }

    #[test]
    fn needs_a_current_page_count() {
        let carve = |d: &Vec<u8>| {
            Sqlite
                .carve(&mut Reader::new(d, 0), 0, d.len() as u64)
                .map(|f| f.len)
        };
        let mut d = build();
        d.extend([0x55; 5000]);
        assert_eq!(carve(&d), Some(4096 * 3));
        d[95] = 6;
        assert_eq!(carve(&d), None);
    }
}

//! Fujifilm RAW (RAF): a fixed header pointing at an embedded JPEG
//! preview, the sensor data header and the sensor data.

use super::{jpeg, Format, Found, Kind};
use crate::extent::{Extent, MappedSource};
use crate::reader::Reader;

pub struct Raf;

const MAGIC: &[u8; 16] = b"FUJIFILMCCD-RAW ";

impl Format for Raf {
    fn name(&self) -> &'static str {
        "raw"
    }

    fn max_len(&self) -> u64 {
        1 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        &head[..16] == MAGIC
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 84);
        let t = r.array::<24>()?;
        let at = |i: usize| u64::from(u32::from_be_bytes(t[i * 4..i * 4 + 4].try_into().unwrap()));
        let parts = [(at(0), at(1)), (at(2), at(3)), (at(4), at(5))];
        let len = parts
            .iter()
            .filter(|(o, l)| *o > 0 && *l > 0)
            .map(|(o, l)| o + l)
            .max()?;
        if len < 160 || start + len > limit {
            return None;
        }
        let (jpeg_off, jpeg_len) = parts[0];
        let taken = (jpeg_off > 0 && jpeg_len > 0)
            .then(|| {
                let src = r.source();
                jpeg::date_from_head(&MappedSource::new(
                    src,
                    vec![Extent::new(start + jpeg_off, jpeg_len)],
                ))
            })
            .flatten();
        Some(Found {
            len,
            ext: "raf",
            kind: Kind::Raw,
            taken,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_from_header_offsets() {
        let mut raf = MAGIC.to_vec();
        raf.resize(84, b'0');
        for v in [160u32, 1000, 1160, 200, 1360, 50_000] {
            raf.extend(v.to_be_bytes());
        }
        raf.resize(160, 0);
        raf.extend([0xFF, 0xD8, 0xFF, 0xD9]);
        raf.resize(51_360, 7);
        let len = raf.len() as u64;
        raf.extend([0u8; 5000]);
        let found = Raf
            .carve(&mut Reader::new(&raf, 0), 0, raf.len() as u64)
            .unwrap();
        assert_eq!((found.len, found.ext), (len, "raf"));
    }
}

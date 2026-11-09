//! PNG: a signature followed by length-prefixed chunks, ending at `IEND`.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Png;

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

impl Format for Png {
    fn name(&self) -> &'static str {
        "png"
    }

    fn max_len(&self) -> u64 {
        512 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        &head[..8] == SIGNATURE && &head[12..16] == b"IHDR"
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 8);
        while r.pos() < limit {
            let len = r.u32be()?;
            let typ = r.array::<4>()?;
            if len > 0x7FFF_FFFF || !typ.iter().all(u8::is_ascii_alphabetic) {
                return None;
            }
            r.skip(u64::from(len) + 4); // data and CRC
            if &typ == b"IEND" {
                let len = r.pos() - start;
                return (r.pos() <= limit).then_some(Found {
                    len,
                    ext: "png",
                    kind: Kind::Photo,
                    taken: None,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carves_to_iend() {
        let mut data = SIGNATURE.to_vec();
        for (typ, len) in [(b"IHDR", 13usize), (b"IDAT", 5000), (b"IEND", 0)] {
            data.extend((len as u32).to_be_bytes());
            data.extend(typ);
            data.extend(vec![7; len]);
            data.extend([0; 4]);
        }
        let len = data.len() as u64;
        data.extend([0xEE; 600]);
        assert!(Png.matches(&data));
        let f = Png
            .carve(&mut Reader::new(&data, 0), 0, data.len() as u64)
            .unwrap();
        assert_eq!(f.len, len);
    }
}

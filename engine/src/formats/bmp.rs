//! Windows bitmaps. The header states the file size; it is accepted only
//! if it agrees with the image's dimensions.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Bmp;

impl Format for Bmp {
    fn name(&self) -> &'static str {
        "bmp"
    }

    fn max_len(&self) -> u64 {
        1 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let u32_at = |o: usize| u32::from_le_bytes(head[o..o + 4].try_into().unwrap());
        &head[..2] == b"BM"
            && u32_at(6) == 0
            && matches!(u32_at(14), 12 | 40 | 52 | 56 | 108 | 124)
            && u16::from_le_bytes([head[26], head[27]]) == 1
            && matches!(
                u16::from_le_bytes([head[28], head[29]]),
                1 | 4 | 8 | 16 | 24 | 32
            )
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let h = r.array::<34>()?;
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let size = u64::from(u32_at(2));
        let pixels = u64::from(u32_at(10));
        let width = u64::from(i32::from_le_bytes(h[18..22].try_into().unwrap()).unsigned_abs());
        let height = u64::from(i32::from_le_bytes(h[22..26].try_into().unwrap()).unsigned_abs());
        let bpp = u64::from(u16::from_le_bytes([h[28], h[29]]));
        let compression = u32_at(30);
        if width == 0
            || height == 0
            || width > 65_535
            || height > 65_535
            || pixels < 26
            || pixels >= size
        {
            return None;
        }
        if compression == 0 {
            let row = (width * bpp).div_ceil(32) * 4;
            let needed = pixels + row * height;
            // Some writers pad the file by a few bytes.
            if needed > size || size > needed + 4096 {
                return None;
            }
        }
        (start + size <= limit).then_some(Found {
            len: size,
            ext: "bmp",
            kind: Kind::Photo,
            taken: None,
        })
    }
}

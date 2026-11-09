//! Photoshop documents (PSD, and PSB for large ones): a header, then
//! length-prefixed sections, then the composite image whose size follows
//! from the dimensions and its compression table.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Psd;

impl Format for Psd {
    fn name(&self) -> &'static str {
        "psd"
    }

    fn max_len(&self) -> u64 {
        4 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let version = u16::from_be_bytes([head[4], head[5]]);
        let channels = u16::from_be_bytes([head[12], head[13]]);
        &head[..4] == b"8BPS"
            && (version == 1 || version == 2)
            && head[6..12] == [0; 6]
            && (1..=56).contains(&channels)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let h = r.array::<26>()?;
        let big = u16::from_be_bytes([h[4], h[5]]) == 2;
        let channels = u64::from(u16::from_be_bytes([h[12], h[13]]));
        let height = u64::from(u32::from_be_bytes(h[14..18].try_into().unwrap()));
        let width = u64::from(u32::from_be_bytes(h[18..22].try_into().unwrap()));
        let depth = u64::from(u16::from_be_bytes([h[22], h[23]]));
        if width == 0
            || height == 0
            || width > 300_000
            || height > 300_000
            || !matches!(depth, 1 | 8 | 16 | 32)
        {
            return None;
        }
        // Colour mode data and image resources, then layers and masks.
        let color = u64::from(r.u32be()?);
        r.skip(color);
        let resources = u64::from(r.u32be()?);
        r.skip(resources);
        let layers = if big {
            r.u64be()?
        } else {
            u64::from(r.u32be()?)
        };
        r.skip(layers);
        if r.pos() > limit {
            return None;
        }

        let compression = r.u16be()?;
        let rows = channels * height;
        let row_bytes = (width * depth).div_ceil(8);
        match compression {
            0 => r.skip(rows * row_bytes),
            1 => {
                // PackBits: a table of every row's compressed length.
                let mut total = 0u64;
                for _ in 0..rows {
                    total += if big {
                        u64::from(r.u32be()?)
                    } else {
                        u64::from(r.u16be()?)
                    };
                }
                r.skip(total);
            }
            _ => return None,
        }
        let end = r.pos();
        (end <= limit).then_some(Found {
            len: end - start,
            ext: "psd",
            kind: Kind::Photo,
            taken: None,
        })
    }
}

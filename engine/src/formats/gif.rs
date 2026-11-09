//! GIF images: a header, then blocks (extensions and images, each a chain
//! of length-prefixed sub-blocks) up to the 0x3B trailer.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Gif;

impl Format for Gif {
    fn name(&self) -> &'static str {
        "gif"
    }

    fn max_len(&self) -> u64 {
        256 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        (head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"))
            && (head[6] != 0 || head[7] != 0)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 10);
        let flags = r.u8()?;
        r.skip(2);
        if flags & 0x80 != 0 {
            r.skip(3 << ((flags & 7) + 1));
        }
        let mut images = 0;
        loop {
            if r.pos() >= limit {
                return None;
            }
            match r.u8()? {
                0x21 => {
                    r.skip(1);
                    skip_sub_blocks(r, limit)?;
                }
                0x2C => {
                    let desc = r.array::<9>()?;
                    if desc[8] & 0x80 != 0 {
                        r.skip(3 << ((desc[8] & 7) + 1));
                    }
                    if !(2..=12).contains(&r.u8()?) {
                        return None;
                    }
                    skip_sub_blocks(r, limit)?;
                    images += 1;
                }
                0x3B if images > 0 => {
                    return Some(Found {
                        len: r.pos() - start,
                        ext: "gif",
                        kind: Kind::Photo,
                        taken: None,
                    });
                }
                _ => return None,
            }
        }
    }
}

fn skip_sub_blocks(r: &mut Reader, limit: u64) -> Option<()> {
    loop {
        let n = r.u8()?;
        if n == 0 {
            return Some(());
        }
        r.skip(u64::from(n));
        if r.pos() > limit {
            return None;
        }
    }
}

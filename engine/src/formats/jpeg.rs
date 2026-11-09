//! JPEG: walks the marker segments and entropy-coded scans to the real end
//! of image. Because segment lengths are followed, the EXIF thumbnail
//! embedded in every camera JPEG is never mistaken for the end of the photo.

use super::{tiff, DateTime, Format, Found, Kind};
use crate::reader::Reader;
use crate::source::Source;

pub struct Jpeg;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const APP1: u8 = 0xE1;

impl Format for Jpeg {
    fn name(&self) -> &'static str {
        "jpeg"
    }

    fn max_len(&self) -> u64 {
        256 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        head[0] == 0xFF
            && head[1] == SOI
            && head[2] == 0xFF
            && head[3] >= 0xC0
            && head[3] < 0xFF
            && head[3] != SOI
            && head[3] != EOI
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        walk(r, start, limit).ok()
    }
}

/// Walks a JPEG from `start`. On failure, returns the position where the
/// data stopped making sense, which is how fragment repair knows roughly
/// where a file's first piece ends.
pub fn walk(r: &mut Reader, start: u64, limit: u64) -> Result<Found, u64> {
    macro_rules! get {
        ($e:expr) => {
            match $e {
                Some(v) => v,
                None => return Err(r.pos()),
            }
        };
    }
    r.seek(start + 2);
    let mut seen_frame = false;
    let mut seen_scan = false;
    let mut taken = None;

    loop {
        if r.pos() >= limit || get!(r.u8()) != 0xFF {
            return Err(r.pos());
        }
        let mut marker = get!(r.u8());
        while marker == 0xFF {
            marker = get!(r.u8());
        }
        match marker {
            EOI if seen_frame && seen_scan => {
                return Ok(Found {
                    len: r.pos() - start,
                    ext: "jpg",
                    kind: Kind::Photo,
                    taken,
                });
            }
            EOI | SOI | 0x00 => return Err(r.pos()),
            // Restart markers and TEM carry no length.
            0xD0..=0xD7 | 0x01 => continue,
            _ => {}
        }

        let seg_len = u64::from(get!(r.u16be()));
        if seg_len < 2 {
            return Err(r.pos());
        }
        let body = r.pos();
        match marker {
            // Start-of-frame markers; C4, C8 and CC are DHT, JPG and DAC.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => seen_frame = true,
            APP1 if taken.is_none() => taken = exif_date(&get!(r.bytes(seg_len as usize - 2))),
            _ => {}
        }
        r.seek(body + seg_len - 2);

        if marker == SOS {
            if !seen_frame {
                return Err(r.pos());
            }
            seen_scan = true;
            skip_entropy_data(r, limit)?;
        }
    }
}

/// Advances to the first marker after a scan. In entropy-coded data a 0xFF
/// is followed by 0x00 (byte stuffing), a restart marker, or a real marker.
/// Restart markers must count D0..D7 in order; a break in the sequence
/// means the data is no longer this image's.
fn skip_entropy_data(r: &mut Reader, limit: u64) -> Result<(), u64> {
    let mut expected_rst: Option<u8> = None;
    loop {
        r.skip_past(0xFF, limit).ok_or(r.pos())?;
        let mut next = r.u8().ok_or(r.pos())?;
        while next == 0xFF {
            next = r.u8().ok_or(r.pos())?;
        }
        match next {
            0x00 => {}
            0xD0..=0xD7 => {
                if expected_rst.is_some_and(|e| e != next) {
                    return Err(r.pos());
                }
                expected_rst = Some(0xD0 + (next - 0xD0 + 1) % 8);
            }
            _ => {
                // Leave the reader on the 0xFF that introduces the marker.
                r.seek(r.pos() - 2);
                return Ok(());
            }
        }
    }
}

/// Reads the capture date from the header segments of a JPEG without
/// walking the image data, for files that do not need checking.
pub fn date_from_head(src: &dyn Source) -> Option<DateTime> {
    // The date is near the start; one small read per photo.
    let mut r = Reader::with_capacity(src, 0, 64 << 10);
    if r.array::<2>()? != [0xFF, SOI] {
        return None;
    }
    for _ in 0..32 {
        if r.u8()? != 0xFF {
            return None;
        }
        let mut marker = r.u8()?;
        while marker == 0xFF {
            marker = r.u8()?;
        }
        if marker == SOS || marker == EOI {
            return None;
        }
        let len = u64::from(r.u16be()?).checked_sub(2)?;
        if marker == APP1 {
            if let Some(d) = exif_date(&r.bytes(len as usize)?) {
                return Some(d);
            }
        } else {
            r.skip(len);
        }
    }
    None
}

/// Reads the capture date from an APP1 segment body.
fn exif_date(body: &[u8]) -> Option<DateTime> {
    let tiff = body.strip_prefix(b"Exif\0\0")?.to_vec();
    let len = tiff.len() as u64;
    tiff::walk(&mut Reader::new(&tiff, 0), 0, len)?.taken
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A structurally valid baseline JPEG: the parser never decodes pixels,
    /// so the segment contents only need correct lengths.
    pub fn sample(scan_len: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        let seg = |v: &mut Vec<u8>, marker: u8, body: &[u8]| {
            v.extend([0xFF, marker]);
            v.extend(((body.len() + 2) as u16).to_be_bytes());
            v.extend(body);
        };
        seg(&mut v, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        seg(&mut v, 0xDB, &[0; 65]);
        seg(&mut v, 0xC0, &[8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
        seg(&mut v, 0xC4, &[0; 29]);
        seg(&mut v, 0xDA, &[1, 1, 0, 0, 63, 0]);
        // Entropy data with stuffed bytes and a restart marker.
        let mut rst = 0;
        for i in 0..scan_len {
            match i % 97 {
                0 => v.extend([0xFF, 0x00]),
                50 => {
                    v.extend([0xFF, 0xD0 + rst]);
                    rst = (rst + 1) % 8;
                }
                _ => v.push((i % 251) as u8 | 1),
            }
        }
        v.extend([0xFF, 0xD9]);
        v
    }

    fn carve(data: &[u8]) -> Option<Found> {
        let src = data.to_vec();
        Jpeg.carve(&mut Reader::new(&src, 0), 0, src.len() as u64)
    }

    #[test]
    fn finds_exact_end() {
        let mut data = sample(10_000);
        let len = data.len() as u64;
        data.extend([0xAB; 1000]);
        assert_eq!(carve(&data).unwrap().len, len);
    }

    #[test]
    fn rejects_truncated_and_scanless() {
        let data = sample(10_000);
        assert!(carve(&data[..data.len() - 200]).is_none());
        assert!(carve(&[0xFF, 0xD8, 0xFF, 0xD9]).is_none());
    }
}

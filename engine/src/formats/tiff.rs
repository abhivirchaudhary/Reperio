//! TIFF structure walking. Used for camera RAW files (CR2, NEF, ARW, DNG,
//! ORF, RW2, PEF), which are TIFF containers, and for the EXIF block inside
//! JPEGs.
//!
//! A TIFF file has no end marker, so its length is the furthest byte that
//! any IFD, tag value, image strip or tile refers to.

use super::{DateTime, Format, Found, Kind};
use crate::reader::Reader;

pub struct TiffRaw;

/// Panasonic RW2's full-size JPEG, stored whole as one tag value.
const TAG_RW2_JPG_FROM_RAW: u16 = 0x002E;
const TAG_COMPRESSION: u16 = 259;
const TAG_MAKE: u16 = 271;
const TAG_ORIENTATION: u16 = 274;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_STRIP_BYTES: u16 = 279;
const TAG_DATETIME: u16 = 306;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTES: u16 = 325;
const TAG_SUB_IFDS: u16 = 330;
const TAG_JPEG_OFFSET: u16 = 513;
const TAG_JPEG_BYTES: u16 = 514;
const TAG_EXIF_IFD: u16 = 34665;
const TAG_GPS_IFD: u16 = 34853;
const TAG_DATETIME_ORIGINAL: u16 = 36867;
const TAG_DNG_VERSION: u16 = 50706;

const MAX_IFDS: usize = 64;
const MAX_ENTRIES: u16 = 1000;

impl Format for TiffRaw {
    fn name(&self) -> &'static str {
        "raw"
    }

    fn max_len(&self) -> u64 {
        1 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        matches!(
            &head[..4],
            b"II*\0" | b"MM\0*" | b"IIRO" | b"IIRS" | b"IIU\0"
        )
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        let info = walk(r, start, limit - start)?;
        // An IFD's entries are counted into the length even when a corrupt
        // count runs them past the limit; such a file does not fit.
        if !info.has_image_data || info.end < 1024 || info.end > limit - start {
            return None;
        }
        r.seek(start);
        let head: [u8; 10] = r.array()?;
        let make = info.make.as_deref().unwrap_or("").to_ascii_uppercase();
        let ext = if &head[8..10] == b"CR" {
            "cr2"
        } else if &head[..4] == b"IIRO" || &head[..4] == b"IIRS" {
            "orf"
        } else if &head[..4] == b"IIU\0" {
            "rw2"
        } else if info.dng {
            "dng"
        } else if make.starts_with("NIKON") {
            "nef"
        } else if make.starts_with("SONY") {
            "arw"
        } else if make.starts_with("PENTAX") || make.starts_with("RICOH") {
            "pef"
        } else {
            "tif"
        };
        let kind = if ext == "tif" { Kind::Photo } else { Kind::Raw };
        Some(Found {
            len: info.end,
            ext,
            kind,
            taken: info.taken,
        })
    }
}

#[derive(Debug, Default)]
pub struct TiffInfo {
    /// Length of the TIFF, relative to its start.
    pub end: u64,
    pub make: Option<String>,
    pub taken: Option<DateTime>,
    pub dng: bool,
    pub has_image_data: bool,
    /// Embedded JPEG previews as (offset, length) relative to the start.
    pub jpegs: Vec<(u64, u64)>,
    /// Everything that may be a viewable JPEG, as (offset, length): the
    /// `jpegs` above, plus single-strip images stored JPEG-compressed (a
    /// CR2's full-size preview, a DNG's preview IFD) and RW2's JpgFromRaw.
    /// Some are the lossless-JPEG sensor data itself, which looks like a
    /// JPEG but is not a picture; callers check the frame type.
    pub previews: Vec<(u64, u64)>,
    /// EXIF orientation of the first IFD, which also applies to previews
    /// that carry none of their own.
    pub orientation: Option<u16>,
}

struct Tiff<'r, 'a> {
    r: &'r mut Reader<'a>,
    base: u64,
    little_endian: bool,
}

impl Tiff<'_, '_> {
    fn u16_at(&mut self, off: u64) -> Option<u16> {
        self.r.seek(self.base + off);
        let b = self.r.array()?;
        Some(if self.little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32_at(&mut self, off: u64) -> Option<u32> {
        self.r.seek(self.base + off);
        let b = self.r.array()?;
        Some(if self.little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    /// Reads an array of SHORT/LONG/IFD values whose field is at `field`.
    fn values(&mut self, typ: u16, count: u32, field: u64) -> Vec<u64> {
        let size = match typ {
            3 => 2,
            4 | 13 => 4,
            _ => return Vec::new(),
        };
        if count == 0 || count > 65_536 {
            return Vec::new();
        }
        let at = if size * count as u64 <= 4 {
            field
        } else {
            match self.u32_at(field) {
                Some(off) => u64::from(off),
                None => return Vec::new(),
            }
        };
        (0..u64::from(count))
            .map_while(|i| {
                if size == 2 {
                    self.u16_at(at + i * 2).map(u64::from)
                } else {
                    self.u32_at(at + i * 4).map(u64::from)
                }
            })
            .collect()
    }

    /// Reads an ASCII value whose field is at `field`.
    fn ascii(&mut self, count: u32, field: u64) -> Option<Vec<u8>> {
        if count == 0 || count > 256 {
            return None;
        }
        let at = if count <= 4 {
            field
        } else {
            u64::from(self.u32_at(field)?)
        };
        self.r.seek(self.base + at);
        self.r.bytes(count as usize)
    }
}

/// Walks the TIFF whose header is at `base`. Offsets beyond `max_len` are
/// treated as corrupt and ignored.
pub fn walk(r: &mut Reader, base: u64, max_len: u64) -> Option<TiffInfo> {
    r.seek(base);
    let little_endian = match &r.array::<2>()? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let mut t = Tiff {
        r,
        base,
        little_endian,
    };
    // 42 is standard TIFF; ORF uses "RO"/"RS" and RW2 uses 0x55.
    if !matches!(t.u16_at(2)?, 42 | 0x4F52 | 0x5352 | 0x55) {
        return None;
    }

    let mut info = TiffInfo {
        end: 8,
        ..Default::default()
    };
    let mut date_original = None;
    let mut date = None;
    let mut queue = vec![(u64::from(t.u32_at(4)?), true)];
    let mut visited = Vec::new();

    while let Some((ifd, follow_chain)) = queue.pop() {
        if ifd < 8 || ifd >= max_len || visited.contains(&ifd) || visited.len() >= MAX_IFDS {
            continue;
        }
        let first = visited.is_empty();
        visited.push(ifd);
        let count = t.u16_at(ifd).unwrap_or(0);
        if count == 0 || count > MAX_ENTRIES {
            if first {
                return None;
            }
            continue;
        }
        let entries_end = ifd + 2 + 12 * u64::from(count);
        info.end = info.end.max(entries_end + 4);

        let mut extents: [(Vec<u64>, Vec<u64>); 3] = Default::default();
        let mut compression = 0;
        for i in 0..u64::from(count) {
            let e = ifd + 2 + 12 * i;
            let (Some(tag), Some(typ), Some(n)) = (t.u16_at(e), t.u16_at(e + 2), t.u32_at(e + 4))
            else {
                break;
            };
            let field = e + 8;
            let type_size: u64 = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => 0,
            };
            let size = type_size * u64::from(n);
            if size > 4 {
                if let Some(off) = t.u32_at(field) {
                    let end = u64::from(off) + size;
                    if end <= max_len {
                        info.end = info.end.max(end);
                    }
                }
            }
            match tag {
                TAG_STRIP_OFFSETS => extents[0].0 = t.values(typ, n, field),
                TAG_STRIP_BYTES => extents[0].1 = t.values(typ, n, field),
                TAG_TILE_OFFSETS => extents[1].0 = t.values(typ, n, field),
                TAG_TILE_BYTES => extents[1].1 = t.values(typ, n, field),
                TAG_JPEG_OFFSET => extents[2].0 = t.values(typ, n, field),
                TAG_JPEG_BYTES => extents[2].1 = t.values(typ, n, field),
                TAG_SUB_IFDS | TAG_EXIF_IFD | TAG_GPS_IFD => {
                    // Only MAX_IFDS are ever walked; a corrupt IFD listing
                    // 65536 sub-IFDs in each of 1000 entries must not queue
                    // them all.
                    let room = (4 * MAX_IFDS).saturating_sub(queue.len());
                    let subs = t.values(typ, n, field).into_iter().take(room);
                    queue.extend(subs.map(|o| (o, false)));
                }
                TAG_MAKE => {
                    info.make = t.ascii(n, field).map(|s| {
                        String::from_utf8_lossy(&s)
                            .trim_end_matches('\0')
                            .trim()
                            .to_string()
                    });
                }
                TAG_DATETIME_ORIGINAL => {
                    date_original = t.ascii(n, field).and_then(|s| DateTime::parse_exif(&s));
                }
                TAG_DATETIME => date = t.ascii(n, field).and_then(|s| DateTime::parse_exif(&s)),
                TAG_DNG_VERSION => info.dng = true,
                TAG_COMPRESSION => {
                    compression = t.values(typ, n, field).first().copied().unwrap_or(0)
                }
                TAG_ORIENTATION if first => {
                    info.orientation = t.values(typ, n, field).first().map(|&o| o as u16);
                }
                TAG_RW2_JPG_FROM_RAW if typ == 7 && size > 4 => {
                    if let Some(off) = t.u32_at(field) {
                        if u64::from(off) + size <= max_len {
                            info.previews.push((u64::from(off), size));
                        }
                    }
                }
                _ => {}
            }
        }

        for (i, (offsets, lengths)) in extents.iter().enumerate() {
            for (&off, &len) in offsets.iter().zip(lengths) {
                let end = off + len;
                if len > 0 && end <= max_len {
                    info.end = info.end.max(end);
                    info.has_image_data = true;
                    if i == 2 {
                        info.jpegs.push((off, len));
                        info.previews.push((off, len));
                    }
                }
            }
        }
        // Old-style (6) and new-style (7) JPEG compression in one strip.
        let (strips, strip_lens) = &extents[0];
        if matches!(compression, 6 | 7) && strips.len() == 1 && strip_lens.len() == 1 {
            let (off, len) = (strips[0], strip_lens[0]);
            if len > 0 && off + len <= max_len {
                info.previews.push((off, len));
            }
        }

        if follow_chain {
            if let Some(next) = t.u32_at(entries_end) {
                queue.push((u64::from(next), true));
            }
        }
    }

    info.taken = date_original.or(date);
    Some(info)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A little-endian TIFF with one IFD: a Make tag, a date and one strip.
    pub fn sample(make: &str, strip_len: u32) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend(8u32.to_le_bytes());
        let entries: u16 = 5;
        let data_at = 8 + 2 + 12 * u32::from(entries) + 4;
        let make_z = format!("{make}\0");
        let date_at = data_at + make_z.len() as u32;
        let strip_at = date_at + 20;
        v.extend(entries.to_le_bytes());
        let mut entry = |tag: u16, typ: u16, n: u32, val: u32| {
            v.extend(tag.to_le_bytes());
            v.extend(typ.to_le_bytes());
            v.extend(n.to_le_bytes());
            v.extend(val.to_le_bytes());
        };
        entry(TAG_MAKE, 2, make_z.len() as u32, data_at);
        entry(TAG_STRIP_OFFSETS, 4, 1, strip_at);
        entry(TAG_STRIP_BYTES, 4, 1, strip_len);
        entry(TAG_DATETIME, 2, 20, date_at);
        entry(TAG_DNG_VERSION - 1, 3, 1, 0);
        v.extend(0u32.to_le_bytes());
        v.extend(make_z.as_bytes());
        v.extend(b"2011:12:25 09:30:00\0");
        v.extend((0..strip_len).map(|i| (i % 200) as u8));
        v
    }

    #[test]
    fn measures_raw_from_strips() {
        let mut data = sample("NIKON CORPORATION", 50_000);
        let len = data.len() as u64;
        data.extend([0; 4096]);
        let found = TiffRaw
            .carve(&mut Reader::new(&data, 0), 0, data.len() as u64)
            .unwrap();
        assert_eq!(found.len, len);
        assert_eq!(found.ext, "nef");
        assert_eq!(found.taken.unwrap().to_string(), "2011-12-25 09.30.00");
    }
}

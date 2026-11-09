//! Windows Media (WMV, WMA) and other ASF files: top-level objects, each
//! a GUID and a size. The header object comes first, then the data
//! object, then optional indexes. The file properties in the header
//! record the file size and an ID that the data object repeats, so the
//! walked length can be cross-checked.

use super::{DateTime, Format, Found, Kind};
use crate::reader::Reader;

pub struct Asf;

/// A GUID as stored in the file, from its text form.
const fn guid(s: &str) -> [u8; 16] {
    let s = s.as_bytes();
    let mut raw = [0u8; 16];
    let (mut i, mut n) = (0, 0);
    while i < s.len() {
        if s[i] != b'-' {
            let d = match s[i] {
                b'0'..=b'9' => s[i] - b'0',
                b'A'..=b'F' => s[i] - b'A' + 10,
                _ => s[i] - b'a' + 10,
            };
            raw[n / 2] |= d << (4 * (1 - n % 2));
            n += 1;
        }
        i += 1;
    }
    // The first three fields are stored little-endian.
    [
        raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10],
        raw[11], raw[12], raw[13], raw[14], raw[15],
    ]
}

const HEADER: [u8; 16] = guid("75B22630-668E-11CF-A6D9-00AA0062CE6C");
const DATA: [u8; 16] = guid("75B22636-668E-11CF-A6D9-00AA0062CE6C");
const FILE_PROPERTIES: [u8; 16] = guid("8CABDCA1-A947-11CF-8EE4-00C00C205365");
const VIDEO_MEDIA: [u8; 16] = guid("BC19EFC0-5B4D-11CF-A8FD-00805F5C442B");
const AUDIO_MEDIA: [u8; 16] = guid("F8699E40-5B4D-11CF-A8FD-00805F5C442B");
/// Objects that may follow the data object.
const INDEXES: [[u8; 16]; 4] = [
    guid("33000890-E5B1-11CF-89F4-00A0C90349CB"),
    guid("D6E229D3-35DA-11D1-9034-00A0C90349BE"),
    guid("FEB103F8-12AD-4C64-840F-2A1D2F7AD48C"),
    guid("3CB73FD0-0C4A-4803-953D-EDF7B6228F0C"),
];

impl Format for Asf {
    fn name(&self) -> &'static str {
        "asf"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        head[..16] == HEADER && head[29] == 0x02
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 16);
        let header_size = r.array().map(u64::from_le_bytes)?;
        if !(30 + 104..=16 << 20).contains(&header_size) || start + header_size > limit {
            return None;
        }
        r.seek(start);
        let header = r.bytes(header_size as usize)?;
        if !self.matches(&header) {
            return None;
        }
        let count = u32::from_le_bytes(header[24..28].try_into().ok()?);
        let mut props = None;
        let mut at = 30usize;
        for _ in 0..count {
            let obj = header.get(at..at + 24)?;
            let size = usize::try_from(u64::from_le_bytes(obj[16..24].try_into().ok()?)).ok()?;
            // A 64-bit object size near usize::MAX: compare without adding.
            if size < 24 || size > header.len() - at {
                return None;
            }
            if obj[..16] == FILE_PROPERTIES && size >= 104 {
                props = Some(&header[at..at + size]);
            }
            at += size;
        }
        let props = props?;
        if at != header.len() {
            return None;
        }
        let le64 =
            |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        let le32 =
            |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
        let file_id = &props[24..40];
        let file_size = le64(props, 40);
        let broadcast = le32(props, 88) & 1 != 0;
        let packet = le32(props, 92);

        let data_at = start + header_size;
        r.seek(data_at);
        let data = r.array::<50>()?;
        if data[..16] != DATA || &data[24..40] != file_id || data[48..50] != [1, 1] {
            return None;
        }
        let mut data_size = le64(&data, 16);
        if broadcast || data_size < 50 {
            // Written live, so the sizes were never filled in: count the
            // packets, which all have the same size.
            if packet == 0 || packet != le32(props, 96) {
                return None;
            }
            let packets = count_packets(r, data_at + 50, u64::from(packet), limit)?;
            data_size = 50 + packets * u64::from(packet);
        }
        let mut end = data_at.checked_add(data_size)?;
        loop {
            r.seek(end);
            let Some(obj) = r.array::<24>() else { break };
            let size = le64(&obj, 16);
            if !INDEXES.iter().any(|g| obj[..16] == *g) || size < 24 {
                break;
            }
            match end.checked_add(size) {
                Some(next) if next <= limit => end = next,
                _ => break,
            }
        }
        if end > limit || (!broadcast && file_size != 0 && end - start != file_size) {
            return None;
        }

        let has = |g: &[u8; 16]| memchr::memmem::find(&header, g).is_some();
        let (ext, kind) = if has(&VIDEO_MEDIA) {
            ("wmv", Kind::Video)
        } else if has(&AUDIO_MEDIA) {
            ("wma", Kind::Audio)
        } else {
            ("asf", Kind::Video)
        };
        // Creation time: 100 ns units since 1601.
        let created = le64(props, 48) / 10_000_000;
        let taken = i64::try_from(created)
            .ok()
            .and_then(|s| DateTime::from_unix(s - 11_644_473_600));
        Some(Found {
            len: end - start,
            ext,
            kind,
            taken,
        })
    }
}

/// The number of data packets of `size` bytes from `pos` that look like
/// the first: the same error correction byte and property flags, which a
/// stream keeps constant.
fn count_packets(r: &mut Reader, pos: u64, size: u64, limit: u64) -> Option<u64> {
    r.seek(pos);
    let first = r.array::<6>()?;
    // The flags come after the error correction data, if any.
    let at = if first[0] & 0x80 != 0 {
        if first[0] & 0x60 != 0 || first[0] & 0x0F > 2 {
            return None;
        }
        1 + usize::from(first[0] & 0x0F)
    } else {
        0
    };
    let mut n = 0;
    while pos + (n + 1) * size <= limit {
        r.seek(pos + n * size);
        let Some(p) = r.array::<6>() else { break };
        if p[0] != first[0] || p[at] & 0x80 != 0 || p[at + 1] != first[at + 1] {
            break;
        }
        n += 1;
    }
    (n > 0).then_some(n)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn build() -> Vec<u8> {
        let id = [0x42; 16];
        let mut props = FILE_PROPERTIES.to_vec();
        props.extend(104u64.to_le_bytes());
        props.extend(id);
        props.extend(0u64.to_le_bytes()); // file size, set below
                                          // 2004-07-15 14:03:59 UTC.
        props.extend(((1_089_900_239u64 + 11_644_473_600) * 10_000_000).to_le_bytes());
        props.resize(88, 0);
        props.extend([0, 0, 0, 0]);
        props.extend(200u32.to_le_bytes());
        props.extend(200u32.to_le_bytes());
        props.extend([0; 4]);
        let mut stream = guid("B7DC0791-A9B7-11CF-8EE6-00C00C205365").to_vec();
        stream.extend(40u64.to_le_bytes());
        stream.extend(AUDIO_MEDIA);
        let mut out = HEADER.to_vec();
        out.extend((30 + props.len() as u64 + 40).to_le_bytes());
        out.extend(2u32.to_le_bytes());
        out.extend([1, 2]);
        out.extend(props);
        out.extend(stream);
        out.extend(DATA);
        out.extend((50u64 + 600).to_le_bytes());
        out.extend(id);
        out.extend(3u64.to_le_bytes());
        out.extend([1, 1]);
        out.extend([0x82; 600]);
        out.extend(INDEXES[0]);
        out.extend(64u64.to_le_bytes());
        out.extend([0; 40]);
        let size = out.len() as u64;
        out[30 + 40..30 + 48].copy_from_slice(&size.to_le_bytes());
        out
    }

    #[test]
    fn walks_to_the_index() {
        let good = build();
        let mut d = good.clone();
        d.extend([0x19; 700]);
        let f = Asf
            .carve(&mut Reader::new(&d, 0), 0, d.len() as u64)
            .unwrap();
        assert_eq!((f.len, f.ext), (good.len() as u64, "wma"));
        assert_eq!(f.taken.unwrap().to_string(), "2004-07-15 14.03.59");
        // The data object must repeat the header's file ID.
        d[good.len() - 64 - 650 + 24] ^= 1;
        assert!(Asf
            .carve(&mut Reader::new(&d, 0), 0, d.len() as u64)
            .is_none());
    }

    #[test]
    fn guids_match_their_text() {
        assert_eq!(HEADER[..4], [0x30, 0x26, 0xB2, 0x75]);
        assert_eq!(
            HEADER[8..],
            [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]
        );
    }
}

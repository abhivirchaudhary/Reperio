//! Matroska (MKV) and WebM video: EBML elements, each an ID and a size.
//! The file is an EBML header and one Segment; a Segment of known size
//! says where the file ends, and one of unknown size (written live) ends
//! after its last top-level element.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Ebml;

const EBML_ID: u32 = 0x1A45_DFA3;
const SEGMENT_ID: u32 = 0x1853_8067;
const DOC_TYPE_ID: u32 = 0x4282;
/// Elements allowed directly inside a Segment.
const TOP_LEVEL: &[u32] = &[
    0x114D_9B74, // SeekHead
    0x1549_A966, // Info
    0x1654_AE6B, // Tracks
    0x1F43_B675, // Cluster
    0x1C53_BB6B, // Cues
    0x1941_A469, // Attachments
    0x1043_A770, // Chapters
    0x1254_C367, // Tags
    0xEC,        // Void
    0xBF,        // CRC-32
];

impl Format for Ebml {
    fn name(&self) -> &'static str {
        "mkv"
    }

    fn max_len(&self) -> u64 {
        256 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        head[..4] == EBML_ID.to_be_bytes()
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let (id, header_size) = element(r)?;
        if id != EBML_ID {
            return None;
        }
        let header_size = header_size?;
        let header_end = r.pos() + header_size;
        let mut webm = false;
        while r.pos() < header_end {
            let (id, size) = element(r)?;
            let size = size?;
            if id == DOC_TYPE_ID {
                webm = r.bytes(size.min(32) as usize)?.starts_with(b"webm");
            } else {
                r.skip(size);
            }
        }
        r.seek(header_end);
        let (id, size) = element(r)?;
        if id != SEGMENT_ID {
            return None;
        }
        let data = r.pos();
        let end = match size {
            Some(size) => {
                // The Segment must open with one of its own elements.
                let (first, _) = element(r)?;
                if !TOP_LEVEL.contains(&first) {
                    return None;
                }
                data + size
            }
            None => {
                let mut pos = data;
                loop {
                    r.seek(pos);
                    match element(r) {
                        Some((id, Some(size)))
                            if TOP_LEVEL.contains(&id) && r.pos() + size <= limit =>
                        {
                            pos = r.pos() + size;
                        }
                        _ => break,
                    }
                }
                if pos == data {
                    return None;
                }
                pos
            }
        };
        let (ext, kind) = (if webm { "webm" } else { "mkv" }, Kind::Video);
        (end <= limit).then_some(Found {
            len: end - start,
            ext,
            kind,
            taken: None,
        })
    }
}

/// Reads an element header: its ID, and its size (`None` when unknown).
fn element(r: &mut Reader) -> Option<(u32, Option<u64>)> {
    let first = r.u8()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut id = u32::from(first);
    for _ in 1..len {
        id = (id << 8) | u32::from(r.u8()?);
    }
    let first = r.u8()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut size = u64::from(first) & (0xFF >> len);
    let mut all_ones = size == (0xFF >> len);
    for _ in 1..len {
        let b = r.u8()?;
        all_ones &= b == 0xFF;
        size = (size << 8) | u64::from(b);
    }
    Some((id, (!all_ones).then_some(size)))
}

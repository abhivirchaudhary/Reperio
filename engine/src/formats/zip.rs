//! ZIP archives and the formats built on them: Word, Excel and PowerPoint
//! documents, OpenDocument, EPUB, Java and Android packages.
//!
//! A ZIP ends with its central directory and the "end of central
//! directory" record, which says where the directory starts relative to
//! the archive's first byte. The archive's end is the first such record
//! whose offsets point back exactly to where this archive began, which
//! also rejects the records of ZIPs nested inside it.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Zip;

const LOCAL: &[u8; 4] = b"PK\x03\x04";
const CENTRAL: &[u8; 4] = b"PK\x01\x02";
const END: &[u8; 4] = b"PK\x05\x06";
const END64: &[u8; 4] = b"PK\x06\x06";
const LOCATOR64: &[u8; 4] = b"PK\x06\x07";

impl Format for Zip {
    fn name(&self) -> &'static str {
        "zip"
    }

    fn max_len(&self) -> u64 {
        4 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let version = u16::from_le_bytes([head[4], head[5]]);
        let name_len = u16::from_le_bytes([head[26], head[27]]);
        &head[..4] == LOCAL && version <= 63 && (1..=1024).contains(&name_len)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 30);
        loop {
            r.skip_past(b'P', limit)?;
            let at = r.pos() - 1;
            if r.array::<3>()? != END[1..] {
                r.seek(at + 1);
                continue;
            }
            r.seek(at + 4);
            let rec = r.array::<18>()?;
            let cd_size = u64::from(u32::from_le_bytes(rec[8..12].try_into().unwrap()));
            let cd_offset = u64::from(u32::from_le_bytes(rec[12..16].try_into().unwrap()));
            let comment = u64::from(u16::from_le_bytes([rec[16], rec[17]]));
            let rel = at - start;
            let directory = if cd_offset == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
                zip64_directory(r, start, at)
            } else {
                (cd_offset + cd_size == rel).then_some((cd_offset, cd_size))
            };
            if let Some((cd_offset, cd_size)) = directory {
                let end = at + 22 + comment;
                if end > limit {
                    return None;
                }
                let (ext, kind) = classify(r, start, cd_offset, cd_size);
                return Some(Found {
                    len: end - start,
                    ext,
                    kind,
                    taken: None,
                });
            }
            r.seek(at + 1);
        }
    }
}

/// Reads the ZIP64 end record that precedes the classic one at `end`.
fn zip64_directory(r: &mut Reader, start: u64, end: u64) -> Option<(u64, u64)> {
    r.seek(end.checked_sub(20)?);
    let loc = r.array::<20>()?;
    if &loc[..4] != LOCATOR64 {
        return None;
    }
    let rec_rel = u64::from_le_bytes(loc[8..16].try_into().unwrap());
    r.seek(start.checked_add(rec_rel)?);
    let rec = r.array::<56>()?;
    if &rec[..4] != END64 {
        return None;
    }
    let cd_size = u64::from_le_bytes(rec[40..48].try_into().unwrap());
    let cd_offset = u64::from_le_bytes(rec[48..56].try_into().unwrap());
    // 64-bit fields: a sum that overflows cannot match.
    (cd_offset.checked_add(cd_size) == Some(rec_rel)).then_some((cd_offset, cd_size))
}

/// Tells Office documents and other ZIP-based formats from plain archives
/// by the names inside.
fn classify(r: &mut Reader, start: u64, cd_offset: u64, cd_size: u64) -> (&'static str, Kind) {
    // OpenDocument and EPUB store their type, uncompressed, as the first entry.
    r.seek(start);
    if let Some(lfh) = r.array::<30>() {
        let name_len = usize::from(u16::from_le_bytes([lfh[26], lfh[27]]));
        let extra = u64::from(u16::from_le_bytes([lfh[28], lfh[29]]));
        if r.bytes(name_len).as_deref() == Some(b"mimetype") {
            r.skip(extra);
            if let Some(m) = r.bytes(64) {
                let m = String::from_utf8_lossy(&m);
                for (needle, ext) in [
                    ("application/epub+zip", "epub"),
                    ("opendocument.text", "odt"),
                    ("opendocument.spreadsheet", "ods"),
                    ("opendocument.presentation", "odp"),
                ] {
                    if m.contains(needle) {
                        return (ext, Kind::Document);
                    }
                }
            }
        }
    }

    let mut names = Vec::new();
    let mut pos = start.saturating_add(cd_offset);
    let end = pos.saturating_add(cd_size.min(8 << 20));
    while pos.saturating_add(46) <= end && names.len() < 4000 {
        r.seek(pos);
        let Some(h) = r.array::<46>() else { break };
        if &h[..4] != CENTRAL {
            break;
        }
        let n = usize::from(u16::from_le_bytes([h[28], h[29]]));
        let extra = u64::from(u16::from_le_bytes([h[30], h[31]]));
        let comment = u64::from(u16::from_le_bytes([h[32], h[33]]));
        let Some(name) = r.bytes(n) else { break };
        names.push(name);
        pos = pos.saturating_add(46 + n as u64 + extra + comment);
    }
    let any = |prefix: &[u8]| names.iter().any(|n| n.starts_with(prefix));
    if any(b"word/") {
        ("docx", Kind::Document)
    } else if any(b"xl/") {
        ("xlsx", Kind::Document)
    } else if any(b"ppt/") {
        ("pptx", Kind::Document)
    } else if any(b"AndroidManifest.xml") {
        ("apk", Kind::Other)
    } else if any(b"META-INF/MANIFEST.MF") {
        ("jar", Kind::Other)
    } else {
        ("zip", Kind::Other)
    }
}

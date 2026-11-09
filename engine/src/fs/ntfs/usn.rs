//! The NTFS change journal, `$Extend\$UsnJrnl`, stream `$J`.
//!
//! Windows appends a short record to it for every change to every file:
//! created, renamed, written, deleted, each with the file's name, its
//! record reference and its folder's. Old records are dropped from the
//! front by making that part of the stream sparse, so only its tail holds
//! data. Records of deletions give back the names of files whose own
//! records have long been reused, and records of folders give back the
//! names of deleted folders, to rebuild paths with.

use super::filetime;
use crate::formats::DateTime;

/// One change record.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Change {
    pub file: u64,
    pub parent: u64,
    pub time: Option<DateTime>,
    pub reason: u32,
    pub directory: bool,
    pub name: String,
}

/// The file was deleted.
pub(super) const FILE_DELETE: u32 = 0x200;
const ATTR_DIRECTORY: u32 = 0x10;
/// The longest a record can be: 255 UTF-16 units of name after the
/// largest (version 3) header, rounded up.
const MAX_RECORD: usize = 1024;
/// Change times between 1990 and 2100, in 100 ns ticks since 1601.
const SANE_TIMES: std::ops::Range<u64> = 122_756_256_000_000_000..157_469_184_000_000_000;

/// Decodes the record at the start of `b`. `None` if it is not one; the
/// record's length otherwise, with the change if it is believable.
fn record(b: &[u8]) -> Option<(usize, Option<Change>)> {
    if b.len() < 8 {
        return None;
    }
    let len = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
    let major = u16::from_le_bytes([b[4], b[5]]);
    // Version 2 records have 64-bit references; version 3 128-bit ones,
    // whose low half is the NTFS reference.
    let (head, file_at, parent_at, time_at) = match major {
        2 => (60, 8, 16, 32),
        3 => (76, 8, 24, 48),
        _ => return None,
    };
    if len < head || len > MAX_RECORD || !len.is_multiple_of(8) || len > b.len() {
        return None;
    }
    let r = &b[..len];
    let u16_at = |o: usize| usize::from(u16::from_le_bytes([r[o], r[o + 1]]));
    let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap());
    let reason = u32_at(time_at + 8);
    let attributes = u32_at(time_at + 20);
    let (name_len, name_at) = (u16_at(time_at + 24), u16_at(time_at + 26));
    let ticks = u64_at(time_at);
    let units: Option<Vec<u16>> = r
        .get(name_at..name_at.checked_add(name_len)?)
        .filter(|n| !n.is_empty() && n.len() % 2 == 0)
        .map(|n| {
            n.chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect()
        });
    let change = units
        .filter(|u| !u.iter().any(|&c| c < 0x20 || c == u16::from(b'/')))
        .and_then(|u| String::from_utf16(&u).ok())
        .filter(|_| SANE_TIMES.contains(&ticks) && reason != 0)
        .map(|name| Change {
            file: u64_at(file_at),
            parent: u64_at(parent_at),
            time: filetime(&r[time_at..time_at + 8]),
            reason,
            directory: attributes & ATTR_DIRECTORY != 0,
            name,
        });
    Some((len, change))
}

/// Decodes the records in `b`, a stretch of the journal starting on a
/// record boundary. Records are 8-byte aligned and pages end in zeros, so
/// anything that is not a record is stepped over 8 bytes at a time.
/// Returns how far it got: a record cut off at the end starts there.
pub(super) fn parse(b: &[u8], out: &mut Vec<Change>) -> usize {
    let mut at = 0;
    while at + 8 <= b.len() {
        let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        let major = u16::from_le_bytes([b[at + 4], b[at + 5]]);
        if matches!(major, 2 | 3) && (60..=MAX_RECORD).contains(&len) && at + len > b.len() {
            // Possibly a record running into the next stretch.
            return at;
        }
        match record(&b[at..]) {
            Some((len, change)) => {
                out.extend(change);
                at += len;
            }
            None => at += 8,
        }
    }
    at
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A version 2 change record.
    pub fn v2(file: u64, parent: u64, ticks: u64, reason: u32, dir: bool, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + units.len() * 2).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&file.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&1234u64.to_le_bytes());
        r[32..40].copy_from_slice(&ticks.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        let attributes: u32 = if dir { 0x10 } else { 0x20 };
        r[52..56].copy_from_slice(&attributes.to_le_bytes());
        r[56..58].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[60 + i * 2..62 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    const TICKS: u64 = super::super::index::tests::TICKS;

    #[test]
    fn reads_changes_and_skips_padding() {
        let mut j = Vec::new();
        j.extend(v2(
            70 | (2 << 48),
            5 | (5 << 48),
            TICKS,
            0x100,
            true,
            "Trip",
        ));
        j.extend([0u8; 40]);
        j.extend(v2(
            71 | (1 << 48),
            70 | (2 << 48),
            TICKS,
            FILE_DELETE | 0x8000_0000,
            false,
            "beach.jpg",
        ));
        // A version 3 record.
        let mut v3 = vec![0u8; 96];
        v3[..4].copy_from_slice(&96u32.to_le_bytes());
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        v3[8..16].copy_from_slice(&(72u64 | (1 << 48)).to_le_bytes());
        v3[24..32].copy_from_slice(&(5u64 | (5 << 48)).to_le_bytes());
        v3[48..56].copy_from_slice(&TICKS.to_le_bytes());
        v3[56..60].copy_from_slice(&FILE_DELETE.to_le_bytes());
        v3[72..74].copy_from_slice(&6u16.to_le_bytes());
        v3[74..76].copy_from_slice(&76u16.to_le_bytes());
        v3[76..82].copy_from_slice(&[b'a', 0, b'.', 0, b'b', 0]);
        j.extend(v3);
        let mut out = Vec::new();
        assert_eq!(parse(&j, &mut out), j.len());
        let got: Vec<(&str, u64, bool, bool)> = out
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.parent & 0xFFFF_FFFF_FFFF,
                    c.directory,
                    c.reason & FILE_DELETE != 0,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("Trip", 5, true, false),
                ("beach.jpg", 70, false, true),
                ("a.b", 5, false, true)
            ]
        );
        // Cut off partway through the second record: stop before it.
        let cut = 72 + 40 + 20;
        let mut part = Vec::new();
        assert_eq!(parse(&j[..cut], &mut part), 72 + 40);
        assert_eq!(part.len(), 1);
    }

    #[test]
    fn survives_corrupt_journals() {
        let mut good = Vec::new();
        for i in 0..40u64 {
            good.extend(v2(
                100 + i,
                5,
                TICKS,
                FILE_DELETE,
                false,
                &format!("file {i}.txt"),
            ));
        }
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            let mut b = good.clone();
            for _ in 0..1 + rng % 16 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let at = (rng % b.len() as u64) as usize;
                b[at] = (rng >> 40) as u8;
            }
            let mut out = Vec::new();
            let end = parse(&b, &mut out);
            assert!(end <= b.len());
            assert!(out.len() <= 40 * 2);
        }
    }
}

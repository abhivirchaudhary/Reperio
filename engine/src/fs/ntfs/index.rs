//! Old entries in NTFS folder indexes.
//!
//! A folder's entries are kept sorted in its `$I30` index: a small root
//! inside the folder's own record, and 4 KB "INDX" blocks elsewhere once
//! the folder holds more than a few files. Removing an entry slides the
//! entries after it down and shortens the block's used length, but leaves
//! the bytes past the new end alone. That slack keeps whole copies of the
//! entries that were there: names, parent folder, size and times of files
//! long since deleted, even once their file records have been reused.
//!
//! Slack is leftovers, so every entry is checked hard before it is
//! believed: it must name this very folder as its parent, have a sane
//! name and length, and carry four timestamps in a plausible range.

use super::{filetime, fix_block};
use crate::formats::DateTime;

/// One entry found in index slack.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Entry {
    /// The file's record reference (number and sequence count), when the
    /// entry's header survived too.
    pub file: Option<u64>,
    /// The parent folder's reference.
    pub parent: u64,
    pub name: String,
    /// 0 POSIX, 1 Win32, 2 DOS 8.3 short name, 3 both.
    pub namespace: u8,
    pub directory: bool,
    pub size: u64,
    pub created: Option<DateTime>,
    pub modified: Option<DateTime>,
}

/// Bytes of a file name key before the name itself.
const KEY_HEAD: usize = 0x42;
/// Index entry header: file reference, entry length, key length, flags.
const ENTRY_HEAD: usize = 0x10;
const REF_MASK: u64 = 0xFFFF_FFFF_FFFF;
/// The folder-index flag in a file name's attributes.
const DIRECTORY_FLAG: u32 = 0x1000_0000;
/// File times between 1980 and 2100, in 100 ns ticks since 1601. Slack
/// bytes that merely look like an entry almost never carry four of these.
const SANE_TIMES: std::ops::Range<u64> = 119_600_064_000_000_000..157_469_184_000_000_000;

/// Decodes the file name key at the start of `k` if it is a believable
/// entry of the folder whose record number is `dir`. Returns the entry
/// (without its file reference) and the key's length.
fn key(k: &[u8], dir: u64) -> Option<(Entry, usize)> {
    if k.len() < KEY_HEAD {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(k[o..o + 8].try_into().unwrap());
    let parent = u64_at(0);
    if parent & REF_MASK != dir {
        return None;
    }
    if (0..4).any(|i| !SANE_TIMES.contains(&u64_at(8 + i * 8))) {
        return None;
    }
    let size = u64_at(0x30);
    if size >= 1 << 48 {
        return None;
    }
    let (n, namespace) = (usize::from(k[0x40]), k[0x41]);
    let len = KEY_HEAD + n * 2;
    if n == 0 || namespace > 3 || k.len() < len {
        return None;
    }
    let units: Vec<u16> = k[KEY_HEAD..len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // Real names never hold control characters or slashes; strict UTF-16
    // turns away unpaired surrogates.
    if units.iter().any(|&u| u < 0x20 || u == u16::from(b'/')) {
        return None;
    }
    let name = String::from_utf16(&units).ok()?;
    let flags = u32::from_le_bytes(k[0x38..0x3C].try_into().unwrap());
    Some((
        Entry {
            file: None,
            parent,
            name,
            namespace,
            directory: flags & DIRECTORY_FLAG != 0,
            size,
            created: filetime(&k[8..16]),
            modified: filetime(&k[16..24]),
        },
        len,
    ))
}

/// Entries in `b[from..to]`, the slack of an index node of the folder
/// `dir`. Entries are 8-byte aligned, so keys are too; `from` is where the
/// node's live entries end.
fn slack(b: &[u8], from: usize, to: usize, dir: u64, out: &mut Vec<Entry>) {
    let to = to.min(b.len());
    // The first slack key may begin right at the end of the live entries:
    // the end marker slid onto a removed entry's header, but not its key.
    let mut at = from.next_multiple_of(8);
    while at + KEY_HEAD <= to {
        let Some((mut entry, len)) = key(&b[at..to], dir) else {
            at += 8;
            continue;
        };
        // The entry's header, if it lies wholly in slack and agrees with
        // the key, gives the file's record reference.
        if let Some(start) = at.checked_sub(ENTRY_HEAD).filter(|&s| s >= from) {
            let h = &b[start..at];
            let file = u64::from_le_bytes(h[..8].try_into().unwrap());
            let entry_len = usize::from(u16::from_le_bytes([h[8], h[9]]));
            let key_len = usize::from(u16::from_le_bytes([h[10], h[11]]));
            if key_len == len && entry_len >= ENTRY_HEAD + len && entry_len % 8 == 0 {
                entry.file = Some(file).filter(|f| f & REF_MASK != 0);
            }
        }
        out.push(entry);
        at += len.next_multiple_of(8);
    }
}

/// Slack entries in a folder's index root (the `$INDEX_ROOT` attribute's
/// value).
pub(super) fn root_slack(value: &[u8], dir: u64, out: &mut Vec<Entry>) {
    // An index header follows the root's 16-byte preamble; its offsets
    // count from the header itself.
    node_slack(value, 0x10, dir, out);
}

/// Slack entries in one INDX block of a folder's index, as read from disk.
/// Blocks that fail their check are skipped.
pub(super) fn block_slack(block: Vec<u8>, dir: u64, out: &mut Vec<Entry>) {
    if let Some(b) = fix_block(block, b"INDX") {
        node_slack(&b, 0x18, dir, out);
    }
}

fn node_slack(b: &[u8], header: usize, dir: u64, out: &mut Vec<Entry>) {
    let Some(h) = b.get(header..header + 16) else {
        return;
    };
    let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap()) as usize;
    let used = header.saturating_add(u32_at(4));
    let allocated = header.saturating_add(u32_at(8)).min(b.len());
    if used <= allocated {
        slack(b, used, allocated, dir, out);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A file name key as NTFS writes it.
    pub fn key_bytes(parent: u64, name: &str, size: u64, ticks: u64, dir: bool) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut k = vec![0u8; KEY_HEAD + units.len() * 2];
        k[..8].copy_from_slice(&parent.to_le_bytes());
        for i in 0..4 {
            k[8 + i * 8..16 + i * 8].copy_from_slice(&ticks.to_le_bytes());
        }
        k[0x28..0x30].copy_from_slice(&size.next_multiple_of(4096).to_le_bytes());
        k[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        let flags: u32 = if dir { DIRECTORY_FLAG } else { 0x20 };
        k[0x38..0x3C].copy_from_slice(&flags.to_le_bytes());
        k[0x40] = units.len() as u8;
        k[0x41] = 1;
        for (i, u) in units.iter().enumerate() {
            k[KEY_HEAD + i * 2..KEY_HEAD + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        k
    }

    /// An index entry holding `key`.
    pub fn entry_bytes(file: u64, key: &[u8]) -> Vec<u8> {
        let len = (ENTRY_HEAD + key.len()).next_multiple_of(8);
        let mut e = vec![0u8; len];
        e[..8].copy_from_slice(&file.to_le_bytes());
        e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        e[10..12].copy_from_slice(&(key.len() as u16).to_le_bytes());
        e[ENTRY_HEAD..ENTRY_HEAD + key.len()].copy_from_slice(key);
        e
    }

    /// 2024-05-01 in Windows ticks.
    pub const TICKS: u64 = (1_714_521_600 + 11_644_473_600) * 10_000_000;

    /// A 4 KB INDX block, as on disk (fixups applied): one
    /// live entry and the end marker, then slack holding `slack` (which
    /// begins `overlap` bytes before the live part's end, as when the end
    /// marker slid over a removed entry's header).
    pub fn indx_block(live: &[u8], slack: &[u8], overlap: usize) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[..4].copy_from_slice(b"INDX");
        b[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        b[6..8].copy_from_slice(&9u16.to_le_bytes());
        let entries = 0x40usize;
        let mut at = entries;
        b[at..at + live.len()].copy_from_slice(live);
        at += live.len();
        let slack_at = at + 0x10 - overlap;
        b[slack_at..slack_at + slack.len()].copy_from_slice(slack);
        // End marker: no file, 16 bytes, flag "last".
        b[at..at + 0x10].fill(0);
        b[at + 8..at + 10].copy_from_slice(&0x10u16.to_le_bytes());
        b[at + 12..at + 14].copy_from_slice(&2u16.to_le_bytes());
        at += 0x10;
        b[0x18..0x1C].copy_from_slice(&((entries - 0x18) as u32).to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&((at - 0x18) as u32).to_le_bytes());
        b[0x20..0x24].copy_from_slice(&((4096 - 0x18) as u32).to_le_bytes());
        // Sector ends swapped for the check value, real bytes kept in the
        // update sequence array.
        b[0x28..0x2A].copy_from_slice(&[0x07, 0x00]);
        for i in 1..=8 {
            let end = i * 512;
            b[0x28 + i * 2] = b[end - 2];
            b[0x29 + i * 2] = b[end - 1];
            b[end - 2..end].copy_from_slice(&[0x07, 0x00]);
        }
        b
    }

    #[test]
    fn finds_removed_entries_in_slack() {
        let dir = 40 | (3 << 48);
        let live = entry_bytes(
            41 | (1 << 48),
            &key_bytes(dir, "alive.txt", 10, TICKS, false),
        );
        // A removed entry whose header the end marker now covers, then a
        // whole one, then a removed subfolder.
        let gone1 = entry_bytes(
            77 | (2 << 48),
            &key_bytes(dir, "Holiday photo.jpg", 123_456, TICKS, false),
        );
        let gone2 = entry_bytes(
            78 | (5 << 48),
            &key_bytes(dir, "zz_report.zip", 9_999, TICKS, false),
        );
        let gone3 = entry_bytes(90 | (1 << 48), &key_bytes(dir, "Old", 0, TICKS, true));
        let slack: Vec<u8> = [gone1, gone2, gone3].concat();
        let block = indx_block(&live, &slack, 0x10);
        let mut out = Vec::new();
        block_slack(block, 40, &mut out);
        let names: Vec<(&str, Option<u64>, u64, bool)> = out
            .iter()
            .map(|e| (e.name.as_str(), e.file, e.size, e.directory))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Holiday photo.jpg", None, 123_456, false),
                ("zz_report.zip", Some(78 | (5 << 48)), 9_999, false),
                ("Old", Some(90 | (1 << 48)), 0, true),
            ]
        );
        assert_eq!(out[0].modified.unwrap().year, 2024);
        // The same block read as another folder's: nothing fits.
        let mut other = Vec::new();
        block_slack(indx_block(&live, &slack, 0x10), 41, &mut other);
        assert!(other.is_empty());
    }

    #[test]
    fn rejects_entries_that_are_not_believable() {
        let dir = 40;
        let ok = key_bytes(dir, "a.txt", 1, TICKS, false);
        assert!(key(&ok, dir).is_some());
        let mut bad_time = ok.clone();
        bad_time[0x20..0x28].copy_from_slice(&0u64.to_le_bytes());
        assert!(key(&bad_time, dir).is_none());
        let mut slash = ok.clone();
        slash[KEY_HEAD] = b'/';
        assert!(key(&slash, dir).is_none());
        let mut long = ok.clone();
        long[0x40] = 200; // runs past the end
        assert!(key(&long, dir).is_none());
        let mut ns = ok;
        ns[0x41] = 9;
        assert!(key(&ns, dir).is_none());
    }

    /// Damaged blocks must never panic or loop.
    #[test]
    fn survives_corrupt_blocks() {
        let dir = 40;
        let live = entry_bytes(41, &key_bytes(dir, "alive.txt", 10, TICKS, false));
        let slack = entry_bytes(42, &key_bytes(dir, "gone.txt", 10, TICKS, false));
        let good = indx_block(&live, &slack, 0);
        let mut rng = 0x1234_5678_9abc_def0u64;
        for _ in 0..20_000 {
            let mut b = good.clone();
            for _ in 0..1 + (rng % 8) {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let at = (rng % 4096) as usize;
                b[at] = (rng >> 32) as u8;
            }
            if rng.is_multiple_of(5) {
                b.truncate((rng >> 20) as usize % 4096);
            }
            let mut out = Vec::new();
            block_slack(b.clone(), dir, &mut out);
            root_slack(&b, dir, &mut out);
        }
    }
}

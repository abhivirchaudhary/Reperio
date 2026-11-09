//! Partition tables: MBR (with extended partitions) and GPT. Memory cards
//! almost always carry an MBR with one FAT32 or exFAT partition; some
//! cameras format them as a "superfloppy" with the filesystem at sector 0.

use crate::reader::Reader;
use crate::source::Source;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub offset: u64,
    pub len: u64,
    /// What the partition table says the partition holds.
    pub label: String,
}

const SECTOR: u64 = 512;

/// Lists the partitions on `src`. A source without a partition table is
/// returned as a single partition covering all of it.
pub fn partitions(src: &dyn Source) -> Vec<Partition> {
    let mut r = Reader::new(src, 0);
    let Some(mbr) = r.bytes(512) else {
        return Vec::new();
    };
    let whole = || {
        vec![Partition {
            offset: 0,
            len: src.len(),
            label: "Whole disk".into(),
        }]
    };

    if super::detect(src, 0).is_some() || mbr[510..512] != [0x55, 0xAA] {
        return whole();
    }

    let entries: Vec<(u8, u64, u64)> = (0..4)
        .map(|i| {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            let lba = u32::from_le_bytes(e[8..12].try_into().unwrap());
            let count = u32::from_le_bytes(e[12..16].try_into().unwrap());
            (e[4], u64::from(lba), u64::from(count))
        })
        .filter(|&(typ, lba, count)| typ != 0 && lba > 0 && count > 0)
        .collect();
    // A boot sector whose "partition entries" are really code would fail here.
    let fits =
        |&(_, lba, count): &(u8, u64, u64)| (lba + count) * SECTOR <= src.len() + SECTOR * 2048;
    // GPT disks carry an MBR whose one entry (0xEE) only protects them;
    // many tools give it the largest size there is (0xFFFFFFFF sectors, 2
    // TB) whatever the disk's size, so it must not be held to `fits`. An
    // MBR with no usable entries but a GPT behind it is read as GPT too.
    let protective = entries.iter().any(|&(typ, ..)| typ == 0xEE);
    if protective || !entries.iter().any(|e| e.0 != 0xEE && fits(e)) {
        if let Some(gpt) = gpt(&mut r, src.len()).filter(|g| !g.is_empty()) {
            return gpt;
        }
    }
    if !entries.iter().all(fits) {
        return whole();
    }

    let mut out = Vec::new();
    for (typ, lba, count) in entries {
        if matches!(typ, 0x05 | 0x0F | 0x85) {
            logical_partitions(&mut r, lba, &mut out);
        } else {
            out.push(Partition {
                offset: lba * SECTOR,
                len: count * SECTOR,
                label: mbr_type(typ).into(),
            });
        }
    }
    if out.is_empty() {
        return whole();
    }
    out
}

/// Follows the chain of extended boot records inside an extended partition.
fn logical_partitions(r: &mut Reader, ext_start: u64, out: &mut Vec<Partition>) {
    let mut ebr = ext_start;
    for _ in 0..128 {
        r.seek(ebr * SECTOR);
        let Some(sector) = r.bytes(512) else { return };
        if sector[510..512] != [0x55, 0xAA] {
            return;
        }
        let entry = |i: usize| {
            let e = &sector[446 + i * 16..462 + i * 16];
            let lba = u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap()));
            let count = u64::from(u32::from_le_bytes(e[12..16].try_into().unwrap()));
            (e[4], lba, count)
        };
        let (typ, lba, count) = entry(0);
        if typ != 0 && count > 0 {
            out.push(Partition {
                offset: (ebr + lba) * SECTOR,
                len: count * SECTOR,
                label: mbr_type(typ).into(),
            });
        }
        let (next_typ, next_lba, _) = entry(1);
        if next_typ == 0 || next_lba == 0 {
            return;
        }
        ebr = ext_start + next_lba;
    }
}

fn gpt(r: &mut Reader, disk_len: u64) -> Option<Vec<Partition>> {
    // The header is at LBA 1; disks with 4K sectors put it 4096 bytes in.
    for sector in [512u64, 4096] {
        r.seek(sector);
        let header = r.bytes(92)?;
        if &header[..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
        let count = u32::from_le_bytes(header[80..84].try_into().unwrap()).min(256);
        let size = u64::from(u32::from_le_bytes(header[84..88].try_into().unwrap()));
        if size < 128 {
            return None;
        }
        let mut out = Vec::new();
        for i in 0..u64::from(count) {
            // Header fields are untrusted: an entry array placed past the
            // end of any disk just fails to read.
            r.seek(entries_lba.saturating_mul(sector).saturating_add(i * size));
            let e = r.bytes(128)?;
            if e[..16].iter().all(|&b| b == 0) {
                continue;
            }
            let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
            let end = last.checked_add(1).and_then(|n| n.checked_mul(sector));
            if last < first || end.is_none_or(|e| e > disk_len) {
                continue;
            }
            let name: Vec<u16> = e[56..128]
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let name = String::from_utf16_lossy(&name);
            let label = if name.is_empty() {
                gpt_type(&e[..16]).to_string()
            } else {
                name
            };
            out.push(Partition {
                offset: first * sector,
                len: (last - first + 1) * sector,
                label,
            });
        }
        return Some(out);
    }
    None
}

fn mbr_type(typ: u8) -> &'static str {
    match typ {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x0B | 0x0C => "FAT32",
        0x07 => "exFAT/NTFS",
        0xAF => "Mac OS Extended",
        0x83 => "Linux",
        0xA5 | 0xA6 | 0xA9 => "BSD",
        _ => "Partition",
    }
}

fn gpt_type(guid: &[u8]) -> &'static str {
    // GUIDs are stored with their first three fields little-endian.
    const TYPES: &[([u8; 4], &str)] = &[
        ([0xA2, 0xA0, 0xD0, 0xEB], "Basic data"),
        ([0xEF, 0x57, 0x34, 0x7C], "APFS"),
        ([0x00, 0x53, 0x46, 0x48], "Mac OS Extended"),
        ([0x28, 0x73, 0x2A, 0xC1], "EFI System"),
        ([0xAF, 0x3D, 0xC6, 0x0F], "Linux"),
    ];
    TYPES
        .iter()
        .find(|(p, _)| guid[..4] == p[..])
        .map_or("Partition", |(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mbr_and_superfloppy() {
        let mut disk = vec![0u8; 4 << 20];
        disk[510] = 0x55;
        disk[511] = 0xAA;
        let e = &mut disk[446..462];
        e[4] = 0x0C;
        e[8..12].copy_from_slice(&2048u32.to_le_bytes());
        e[12..16].copy_from_slice(&4096u32.to_le_bytes());
        assert_eq!(
            partitions(&disk),
            vec![Partition {
                offset: 1 << 20,
                len: 2 << 20,
                label: "FAT32".into()
            }]
        );
        let blank = vec![0u8; 1 << 20];
        assert_eq!(partitions(&blank)[0].label, "Whole disk");
    }

    /// A GPT disk as Windows leaves it: boot code in the protective MBR,
    /// whose 0xEE entry claims 2 TB on a smaller disk, a Microsoft Reserved
    /// partition, then the data partition.
    #[test]
    fn reads_gpt_behind_an_oversized_protective_mbr() {
        let mut disk = vec![0u8; 8 << 20];
        disk[..8].copy_from_slice(&[0x33, 0xC0, 0x8E, 0xD0, 0xBC, 0x00, 0x7C, 0x8E]);
        disk[510] = 0x55;
        disk[511] = 0xAA;
        let e = &mut disk[446..462];
        e[4] = 0xEE;
        e[8..12].copy_from_slice(&1u32.to_le_bytes());
        e[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let h = &mut disk[512..604];
        h[..8].copy_from_slice(b"EFI PART");
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        let parts: [([u8; 16], u64, u64); 2] = [
            // Microsoft Reserved
            (
                [
                    0x16, 0xE3, 0xC9, 0xE3, 0x5C, 0x0B, 0xB8, 0x4D, 0x81, 0x7D, 0xF9, 0x2D, 0xF0,
                    0x02, 0x15, 0xAE,
                ],
                34,
                2047,
            ),
            // Basic data
            (
                [
                    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7,
                    0x26, 0x99, 0xC7,
                ],
                2048,
                16_000,
            ),
        ];
        for (i, (guid, first, last)) in parts.iter().enumerate() {
            let e = &mut disk[1024 + i * 128..1024 + (i + 1) * 128];
            e[..16].copy_from_slice(guid);
            e[16] = 1;
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
        }
        let found = partitions(&disk);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            (found[1].offset, found[1].len),
            (2048 * 512, (16_000 - 2048 + 1) * 512)
        );
        assert_eq!(found[1].label, "Basic data");
    }
}

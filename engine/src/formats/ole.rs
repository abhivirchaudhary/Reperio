//! OLE2 compound files ("Compound File Binary"): Word, Excel and
//! PowerPoint documents from before 2007, Outlook messages, Windows
//! installers and Visio drawings.
//!
//! A compound file is a small filesystem of fixed-size sectors. Its
//! allocation table (FAT) chains the sectors of every stream, and the file
//! ends with the last sector the table uses: nothing in the header states
//! the length. The table, the directory and every stream's chain are
//! checked before the file is accepted, which random data never survives.
//! The kind of document is told by the names of the root's streams.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Ole;

const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const FREE: u32 = 0xFFFF_FFFF;
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
const FAT_SECT: u32 = 0xFFFF_FFFD;
const DIFAT_SECT: u32 = 0xFFFF_FFFC;
const NO_STREAM: u32 = 0xFFFF_FFFF;
/// Streams shorter than this live in the mini stream, not in the FAT.
const MINI_CUTOFF: u64 = 4096;
/// The root's class ID in Windows Installer packages.
const MSI_CLSID: [u8; 16] = [
    0x84, 0x10, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

impl Format for Ole {
    fn name(&self) -> &'static str {
        "ole"
    }

    fn max_len(&self) -> u64 {
        4 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let major = u16::from_le_bytes([head[26], head[27]]);
        let shift = u16::from_le_bytes([head[30], head[31]]);
        head[..8] == SIGNATURE
            && head[28..30] == [0xFE, 0xFF]
            && matches!((major, shift), (3, 9) | (4, 12))
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let h = r.array::<512>()?;
        let u16_at = |o: usize| u16::from_le_bytes([h[o], h[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
        let major = u16_at(26);
        let shift = u16_at(30);
        let valid_header = self.matches(&h)
            && u16_at(32) == 6 // mini sector shift
            && h[34..40].iter().all(|&b| b == 0)
            && (major == 4 || u32_at(40) == 0) // directory sector count, v4 only
            && u64::from(u32_at(56)) == MINI_CUTOFF;
        if !valid_header {
            return None;
        }
        let ss = 1u64 << shift;
        let per_sector = (ss / 4) as usize;
        // Sectors that fit below the limit; the header takes the first slot.
        let room = ((limit.checked_sub(start)? / ss).checked_sub(1)?).min(u64::from(DIFAT_SECT));
        let in_room = |s: u32| u64::from(s) < room;

        let num_fat = u32_at(44) as usize;
        if num_fat == 0 || num_fat as u64 > room / per_sector as u64 + 2 {
            return None;
        }

        // Where the FAT's own sectors are: 109 in the header, the rest in
        // the DIFAT chain.
        let mut fat_sectors = Vec::with_capacity(num_fat);
        for i in 0..109 {
            let s = u32_at(76 + 4 * i);
            if i < num_fat {
                if !in_room(s) {
                    return None;
                }
                fat_sectors.push(s);
            } else if s != FREE {
                return None;
            }
        }
        let mut difat_sectors = Vec::new();
        let mut next = u32_at(68);
        let num_difat = u32_at(72) as usize;
        if num_difat > num_fat / (per_sector - 1) + 1 {
            return None;
        }
        for _ in 0..num_difat {
            if !in_room(next) || difat_sectors.contains(&next) {
                return None;
            }
            difat_sectors.push(next);
            r.seek(start + (u64::from(next) + 1) * ss);
            let sector = r.bytes(ss as usize)?;
            let entries = words(&sector);
            for &s in &entries[..per_sector - 1] {
                if fat_sectors.len() < num_fat {
                    if !in_room(s) {
                        return None;
                    }
                    fat_sectors.push(s);
                } else if s != FREE {
                    return None;
                }
            }
            next = entries[per_sector - 1];
        }
        if fat_sectors.len() != num_fat || !matches!(next, END_OF_CHAIN | FREE) {
            return None;
        }

        let mut fat = Vec::with_capacity(num_fat * per_sector);
        for &s in &fat_sectors {
            r.seek(start + (u64::from(s) + 1) * ss);
            fat.extend(words(&r.bytes(ss as usize)?));
        }
        let mut last_used = None;
        for (i, &e) in fat.iter().enumerate() {
            match e {
                FREE => continue,
                END_OF_CHAIN | FAT_SECT | DIFAT_SECT => {}
                s if (s as usize) < fat.len() && in_room(s) => {}
                _ => return None,
            }
            last_used = Some(i as u64);
        }
        let last_used = last_used?;
        if last_used >= room
            || fat_sectors.iter().any(|&s| fat[s as usize] != FAT_SECT)
            || difat_sectors.iter().any(|&s| fat[s as usize] != DIFAT_SECT)
        {
            return None;
        }

        let mut chains = Chains {
            fat: &fat,
            steps: 0,
        };
        let dir_chain = chains.walk(u32_at(48))?;
        let mut dir = Vec::with_capacity(dir_chain.len() * ss as usize);
        for &s in &dir_chain {
            r.seek(start + (u64::from(s) + 1) * ss);
            dir.extend(r.bytes(ss as usize)?);
        }
        let entries: Vec<&[u8]> = dir.chunks_exact(128).collect();
        let count = entries.len() as u32;
        let link_ok = |l: u32| l == NO_STREAM || l < count;
        let mut root_size = 0;
        for (i, e) in entries.iter().enumerate() {
            let typ = e[66];
            if typ == 0 {
                continue;
            }
            let name_len = usize::from(u16::from_le_bytes([e[64], e[65]]));
            let field = |o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);
            let mut size = u64::from_le_bytes(e[120..128].try_into().ok()?);
            if major == 3 {
                // Version 3 writers may leave junk in the high half.
                size &= 0xFFFF_FFFF;
            }
            let plausible = matches!(typ, 1 | 2 | 5)
                && (typ == 5) == (i == 0)
                && (2..=64).contains(&name_len)
                && name_len % 2 == 0
                && e[name_len - 2..name_len] == [0, 0]
                && e[67] <= 1
                && link_ok(field(68))
                && link_ok(field(72))
                && link_ok(field(76));
            if !plausible {
                return None;
            }
            match typ {
                5 => {
                    root_size = size;
                    if size > 0 {
                        chains.covers(field(116), size, ss)?;
                    }
                }
                2 if size >= MINI_CUTOFF => chains.covers(field(116), size, ss)?,
                _ => {}
            }
        }
        if entries.first().map(|e| e[66]) != Some(5) {
            return None;
        }
        let mini_fat_sectors = u64::from(u32_at(64));
        if mini_fat_sectors > 0 {
            chains.covers(u32_at(60), mini_fat_sectors * ss, ss)?;
        }
        // Small streams must start inside the mini stream.
        let mini_sectors = root_size.div_ceil(64);
        for e in entries.iter().skip(1) {
            let size = u32::from_le_bytes(e[120..124].try_into().ok()?);
            let first = u32::from_le_bytes(e[116..120].try_into().ok()?);
            let small = e[66] == 2 && size > 0 && u64::from(size) < MINI_CUTOFF;
            if small && u64::from(first) >= mini_sectors {
                return None;
            }
        }

        let end = start + (last_used + 2) * ss;
        if end > limit {
            return None;
        }
        let (ext, kind) = classify(&entries);
        Some(Found {
            len: end - start,
            ext,
            kind,
            taken: None,
        })
    }
}

/// The little-endian 32-bit words of a sector.
fn words(sector: &[u8]) -> Vec<u32> {
    sector
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect()
}

/// Follows sector chains through the FAT, with a shared step budget so a
/// table full of loops cannot keep the scan busy.
struct Chains<'a> {
    fat: &'a [u32],
    steps: usize,
}

impl Chains<'_> {
    /// The sectors of the chain starting at `first`, which must end
    /// properly.
    fn walk(&mut self, first: u32) -> Option<Vec<u32>> {
        let mut out = Vec::new();
        let mut s = first;
        while s != END_OF_CHAIN {
            self.steps += 1;
            if self.steps > 2 * self.fat.len() + 64 {
                return None;
            }
            let next = *self.fat.get(s as usize)?;
            if next != END_OF_CHAIN && next as usize >= self.fat.len() {
                return None;
            }
            out.push(s);
            s = next;
        }
        (!out.is_empty()).then_some(out)
    }

    /// Checks that the chain at `first` is long enough to hold `size` bytes.
    fn covers(&mut self, first: u32, size: u64, ss: u64) -> Option<()> {
        let chain = self.walk(first)?;
        (chain.len() as u64 >= size.div_ceil(ss)).then_some(())
    }
}

/// Names the document by the streams directly inside the root storage.
fn classify(entries: &[&[u8]]) -> (&'static str, Kind) {
    let name = |e: &[u8]| -> String {
        let len = usize::from(u16::from_le_bytes([e[64], e[65]])).min(64);
        let units: Vec<u16> = e[..len.saturating_sub(2)]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    };
    let link = |e: &[u8], o: usize| u32::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3]]);

    // The root's children form a tree linked by left and right siblings.
    let mut names = Vec::new();
    let mut seen = vec![false; entries.len()];
    let mut stack = vec![link(entries[0], 76)];
    while let Some(i) = stack.pop() {
        let Some(e) = entries.get(i as usize) else {
            continue;
        };
        if std::mem::replace(&mut seen[i as usize], true) {
            continue;
        }
        names.push(name(e));
        stack.push(link(e, 68));
        stack.push(link(e, 72));
    }
    let has = |n: &str| names.iter().any(|x| x == n);
    if has("WordDocument") {
        ("doc", Kind::Document)
    } else if has("Workbook") || has("Book") {
        ("xls", Kind::Document)
    } else if has("PowerPoint Document") {
        ("ppt", Kind::Document)
    } else if has("__properties_version1.0") || names.iter().any(|n| n.starts_with("__substg1.0_"))
    {
        ("msg", Kind::Document)
    } else if has("VisioDocument") {
        ("vsd", Kind::Document)
    } else if entries[0][80..96] == MSI_CLSID {
        ("msi", Kind::Other)
    } else {
        ("ole", Kind::Other)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal version 3 compound file with the given root streams, each
    /// `size` bytes, stored in the FAT (sizes of 4096 and over) or the
    /// mini stream.
    pub(crate) fn build(streams: &[(&str, usize)]) -> Vec<u8> {
        const SS: usize = 512;
        let mut sectors: Vec<Vec<u8>> = Vec::new();
        let mut fat: Vec<u32> = Vec::new();
        let alloc = |data: &[u8], fat: &mut Vec<u32>, sectors: &mut Vec<Vec<u8>>| -> u32 {
            let first = sectors.len() as u32;
            let n = data.len().div_ceil(SS).max(1);
            for i in 0..n {
                let mut s = data[(i * SS).min(data.len())..((i + 1) * SS).min(data.len())].to_vec();
                s.resize(SS, 0);
                sectors.push(s);
                fat.push(if i + 1 == n {
                    END_OF_CHAIN
                } else {
                    first + i as u32 + 1
                });
            }
            first
        };
        // Sector 0 is the FAT itself.
        sectors.push(vec![0; SS]);
        fat.push(FAT_SECT);

        let mut mini = Vec::new();
        let mut mini_fat: Vec<u32> = Vec::new();
        let mut entries = Vec::new();
        for (name, size) in streams {
            let data = vec![0x5A; *size];
            let first = if *size >= MINI_CUTOFF as usize {
                alloc(&data, &mut fat, &mut sectors)
            } else {
                let first = (mini.len() / 64) as u32;
                let n = size.div_ceil(64);
                for i in 0..n {
                    mini_fat.push(if i + 1 == n {
                        END_OF_CHAIN
                    } else {
                        first + i as u32 + 1
                    });
                }
                mini.extend(&data);
                mini.resize(mini.len().div_ceil(64) * 64, 0);
                first
            };
            entries.push((name.to_string(), 2u8, first, *size as u64));
        }
        let mini_start = if mini.is_empty() {
            END_OF_CHAIN
        } else {
            alloc(&mini, &mut fat, &mut sectors)
        };
        let mini_fat_bytes: Vec<u8> = mini_fat.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mini_fat_start = if mini_fat.is_empty() {
            END_OF_CHAIN
        } else {
            alloc(&mini_fat_bytes, &mut fat, &mut sectors)
        };

        let entry = |name: &str, typ: u8, child: u32, right: u32, start: u32, size: u64| {
            let mut e = vec![0u8; 128];
            let units: Vec<u16> = name.encode_utf16().chain([0]).collect();
            for (i, u) in units.iter().enumerate() {
                e[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
            }
            e[64..66].copy_from_slice(&(2 * units.len() as u16).to_le_bytes());
            e[66] = typ;
            e[67] = 1;
            e[68..72].copy_from_slice(&NO_STREAM.to_le_bytes());
            e[72..76].copy_from_slice(&right.to_le_bytes());
            e[76..80].copy_from_slice(&child.to_le_bytes());
            e[116..120].copy_from_slice(&start.to_le_bytes());
            e[120..128].copy_from_slice(&size.to_le_bytes());
            e
        };
        let mut dir = entry(
            "Root Entry",
            5,
            if entries.is_empty() { NO_STREAM } else { 1 },
            NO_STREAM,
            mini_start,
            mini.len() as u64,
        );
        for (i, (name, typ, first, size)) in entries.iter().enumerate() {
            let right = if i + 1 < entries.len() {
                i as u32 + 2
            } else {
                NO_STREAM
            };
            dir.extend(entry(name, *typ, NO_STREAM, right, *first, *size));
        }
        let dir_start = alloc(&dir, &mut fat, &mut sectors);

        assert!(fat.len() <= SS / 4);
        fat.resize(SS / 4, FREE);
        sectors[0] = fat.iter().flat_map(|v| v.to_le_bytes()).collect();

        let mut h = vec![0u8; 512];
        h[..8].copy_from_slice(&SIGNATURE);
        h[24..26].copy_from_slice(&0x3Eu16.to_le_bytes());
        h[26..28].copy_from_slice(&3u16.to_le_bytes());
        h[28..30].copy_from_slice(&[0xFE, 0xFF]);
        h[30..32].copy_from_slice(&9u16.to_le_bytes());
        h[32..34].copy_from_slice(&6u16.to_le_bytes());
        h[44..48].copy_from_slice(&1u32.to_le_bytes());
        h[48..52].copy_from_slice(&dir_start.to_le_bytes());
        h[56..60].copy_from_slice(&4096u32.to_le_bytes());
        h[60..64].copy_from_slice(&mini_fat_start.to_le_bytes());
        let mini_fat_count = mini_fat_bytes.len().div_ceil(SS) as u32;
        h[64..68].copy_from_slice(&mini_fat_count.to_le_bytes());
        h[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        for i in 0..109 {
            let v = if i == 0 { 0 } else { FREE };
            h[76 + 4 * i..80 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        let mut out = h;
        for s in sectors {
            out.extend(s);
        }
        out
    }

    fn carve(data: &[u8]) -> Option<Found> {
        let data = data.to_vec();
        Ole.carve(&mut Reader::new(&data, 0), 0, data.len() as u64)
    }

    #[test]
    fn carves_and_names_documents() {
        for (streams, ext) in [
            (vec![("WordDocument", 9000), ("1Table", 700)], "doc"),
            (vec![("Workbook", 5000)], "xls"),
            (vec![("PowerPoint Document", 300)], "ppt"),
            (vec![("__substg1.0_0037001F", 40)], "msg"),
            (vec![("Contents", 100)], "ole"),
        ] {
            let mut data = build(&streams);
            let len = data.len() as u64;
            assert!(Ole.matches(&data));
            data.extend([0xA7; 3000]);
            let f = carve(&data).unwrap();
            assert_eq!((f.len, f.ext), (len, ext));
        }
    }

    #[test]
    fn rejects_broken_tables() {
        let good = build(&[("WordDocument", 9000)]);
        // A FAT entry pointing past the table.
        let mut bad = good.clone();
        bad[512 + 8..512 + 12].copy_from_slice(&0x0012_3456u32.to_le_bytes());
        assert!(carve(&bad).is_none());
        // The FAT's own sector not marked as such.
        let mut bad = good.clone();
        bad[512..516].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        assert!(carve(&bad).is_none());
        // Cut short.
        assert!(carve(&good[..good.len() - 512]).is_none());
    }
}

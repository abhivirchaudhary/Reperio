//! Linux md (software RAID) superblocks, which Synology, QNAP, most other
//! NAS boxes and Linux servers write on each member. They say exactly how
//! the array is laid out and which position each drive holds.
//!
//! Formats, as mdadm writes them (super0.c, super1.c):
//! - 0.90: 4 KiB in the last 64 KiB-aligned 64 KiB of the device, in the
//!   byte order of the machine that wrote it; data starts at 0.
//! - 1.0: 8 to 12 sectors from the end, 4 KiB aligned; 1.1: at 0; 1.2: at
//!   4 KiB. Little endian; the data offset is recorded.
//!
//! Every field is untrusted: a superblock can be damaged or stale, so
//! values are checked before use and the checksum is reported, not
//! required.

use std::sync::Arc;

use super::{Layout, Level, Parity, MAX_CHUNK, MAX_MEMBERS};
use crate::fs::partition;
use crate::source::Source;

pub const MAGIC: u32 = 0xa92b_4efc;
const SB_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V0_90,
    V1_0,
    V1_1,
    V1_2,
}

impl Version {
    pub fn name(self) -> &'static str {
        match self {
            Version::V0_90 => "0.90",
            Version::V1_0 => "1.0",
            Version::V1_1 => "1.1",
            Version::V1_2 => "1.2",
        }
    }
}

/// One member's superblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub version: Version,
    pub uuid: [u8; 16],
    /// The array's name (1.x only), e.g. "DiskStation:2".
    pub name: String,
    /// md level: -1 linear, 0, 1, 4, 5, 6, 10.
    pub level: i32,
    pub layout: u32,
    /// Bytes.
    pub chunk: u64,
    pub raid_disks: u32,
    /// This member's position, `None` for a spare or a failed member.
    pub role: Option<u32>,
    /// Where the member device (the whole drive or a partition) starts on
    /// the drive the superblock was found on.
    pub base: u64,
    /// Bytes from `base` to the array data.
    pub data_offset: u64,
    /// Bytes of array data on this member.
    pub data_size: u64,
    pub events: u64,
    pub checksum_ok: bool,
    /// A reshape (growing or changing level) was under way; the layout
    /// then changes partway through the array.
    pub reshaping: bool,
}

impl Superblock {
    pub fn uuid_string(&self) -> String {
        let h: Vec<String> = self.uuid.iter().map(|b| format!("{b:02x}")).collect();
        // mdadm prints it as four 8-digit groups.
        format!(
            "{}:{}:{}:{}",
            h[0..4].concat(),
            h[4..8].concat(),
            h[8..12].concat(),
            h[12..16].concat()
        )
    }

    /// The array level as a [`Level`], if Reperio can read it.
    pub fn level(&self) -> Option<Level> {
        match self.level {
            -1 => Some(Level::Jbod),
            0 => Some(Level::Raid0),
            1 => Some(Level::Raid1),
            4 => Some(Level::Raid5 {
                parity: Parity::Last,
            }),
            5 => Some(Level::Raid5 {
                parity: match self.layout {
                    0 => Parity::LeftAsymmetric,
                    1 => Parity::RightAsymmetric,
                    2 => Parity::LeftSymmetric,
                    3 => Parity::RightSymmetric,
                    4 => Parity::First,
                    5 => Parity::Last,
                    _ => return None,
                },
            }),
            10 => {
                // Bits 0-7 near copies, 8-15 far copies, 16 offset; 17
                // (far sets) is not supported.
                if self.layout >> 17 != 0 {
                    return None;
                }
                let near = (self.layout & 0xff) as u8;
                let far = ((self.layout >> 8) & 0xff) as u8;
                (near > 0 && far > 0).then_some(Level::Raid10 {
                    near,
                    far,
                    offset: self.layout & 0x1_0000 != 0,
                })
            }
            _ => None,
        }
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn read(src: &dyn Source, at: u64) -> Option<Vec<u8>> {
    let mut b = vec![0u8; SB_BYTES];
    (src.read_at(at, &mut b).ok()? == SB_BYTES).then_some(b)
}

/// Looks for a superblock on the member device `[base, base + len)`.
pub fn read_device(src: &dyn Source, base: u64, len: u64) -> Option<Superblock> {
    let sectors = len / 512;
    // 1.1 and 1.2 first: a 0.90 or 1.0 superblock left at the end by an
    // older array would otherwise win over the current one.
    let v1 = [
        (Version::V1_1, Some(0)),
        (Version::V1_2, Some(8)),
        (
            Version::V1_0,
            sectors.checked_sub(16).map(|s| s & !7).filter(|&s| s > 0),
        ),
    ];
    for (version, sector) in v1 {
        let Some(sector) = sector else { continue };
        let at = base.checked_add(sector * 512)?;
        if let Some(sb) = read(src, at).and_then(|b| parse_v1(&b, version, sector, len)) {
            return Some(Superblock { base, ..sb });
        }
    }
    // 0.90: MD_NEW_SIZE_SECTORS, the last whole 64 KiB less 64 KiB.
    let sb_sector = (sectors & !127).checked_sub(128).filter(|&s| s > 0)?;
    let b = read(src, base.checked_add(sb_sector * 512)?)?;
    parse_v090(&b, sb_sector * 512).map(|sb| Superblock { base, ..sb })
}

/// Parses a 1.x superblock found `sector` sectors into a `len`-byte device.
pub fn parse_v1(b: &[u8], version: Version, sector: u64, len: u64) -> Option<Superblock> {
    if b.len() < SB_BYTES || le32(b, 0) != MAGIC || le32(b, 4) != 1 {
        return None;
    }
    // The superblock records where it is; a copy at the wrong place is
    // from something else (a member image inside a member image).
    if le64(b, 144) != sector {
        return None;
    }
    let max_dev = le32(b, 220) as usize;
    let dev_number = le32(b, 160) as usize;
    if 256 + max_dev * 2 > SB_BYTES {
        return None;
    }
    let feature_map = le32(b, 8);
    let role = (dev_number < max_dev)
        .then(|| u16::from_le_bytes([b[256 + dev_number * 2], b[257 + dev_number * 2]]))
        .and_then(|r| (r < 0xff00).then_some(u32::from(r)));
    let data_offset = le64(b, 128).checked_mul(512)?;
    let data_size = le64(b, 136).checked_mul(512)?;
    let size = le64(b, 80).checked_mul(512)?;
    if data_offset >= len {
        return None;
    }
    let sum_len = 256 + max_dev * 2;
    let mut sum: u64 = 0;
    let mut i = 0;
    while i + 4 <= sum_len {
        if i != 216 {
            sum += u64::from(le32(b, i));
        }
        i += 4;
    }
    if sum_len - i == 2 {
        sum += u64::from(u16::from_le_bytes([b[i], b[i + 1]]));
    }
    let csum = ((sum & 0xffff_ffff) + (sum >> 32)) as u32;
    let name_end = b[32..64].iter().position(|&c| c == 0).unwrap_or(32);
    // Linear arrays and RAID 0 use each member's own size; the others
    // record the size every member contributes.
    let data_size = match size {
        0 => data_size,
        s => s.min(if data_size > 0 { data_size } else { s }),
    };
    Some(Superblock {
        version,
        uuid: b[16..32].try_into().unwrap(),
        name: String::from_utf8_lossy(&b[32..32 + name_end]).into_owned(),
        level: le32(b, 72) as i32,
        layout: le32(b, 76),
        chunk: u64::from(le32(b, 88)) * 512,
        raid_disks: le32(b, 92),
        role,
        base: 0,
        data_offset,
        data_size: data_size.min(len - data_offset),
        events: le64(b, 200),
        checksum_ok: csum == le32(b, 216),
        // MD_FEATURE_RESHAPE_ACTIVE
        reshaping: feature_map & 4 != 0,
    })
}

/// Parses a 0.90 superblock found `at` bytes into its device.
pub fn parse_v090(b: &[u8], at: u64) -> Option<Superblock> {
    if b.len() < SB_BYTES {
        return None;
    }
    let big = if le32(b, 0) == MAGIC {
        false
    } else if u32::from_be_bytes(b[0..4].try_into().unwrap()) == MAGIC {
        true
    } else {
        return None;
    };
    let word = |i: usize| {
        let w: [u8; 4] = b[i * 4..i * 4 + 4].try_into().unwrap();
        if big {
            u32::from_be_bytes(w)
        } else {
            u32::from_le_bytes(w)
        }
    };
    if word(1) != 0 || word(2) != 90 {
        return None;
    }
    let mut sum: u64 = 0;
    for i in 0..SB_BYTES / 4 {
        if i != 38 {
            sum += u64::from(word(i));
        }
    }
    let csum = ((sum & 0xffff_ffff) + (sum >> 32)) as u32;
    // The set UUID is word 5 then words 13-15, each stored as a host
    // integer; mdadm prints them as numbers, so keep their value order.
    let mut uuid = [0u8; 16];
    for (k, w) in [5, 13, 14, 15].into_iter().enumerate() {
        uuid[k * 4..k * 4 + 4].copy_from_slice(&word(w).to_be_bytes());
    }
    // this_disk descriptor at word 992: number, major, minor, raid_disk, state.
    let state = word(996);
    let raid_disk = word(995);
    let faulty = state & 1 != 0;
    let level = word(7) as i32;
    let events = if big {
        (u64::from(word(39)) << 32) | u64::from(word(40))
    } else {
        (u64::from(word(40)) << 32) | u64::from(word(39))
    };
    let chunk = u64::from(word(65));
    let data_size = match level {
        // Linear: each member's own size, the space before the superblock
        // (rounded down to the chunk, as md does).
        -1 if chunk > 0 => at / chunk * chunk,
        -1 => at,
        // RAID 0 likewise uses each member's whole space.
        0 => at,
        _ => (u64::from(word(8)) * 1024).min(at),
    };
    Some(Superblock {
        version: Version::V0_90,
        uuid,
        name: String::new(),
        level,
        layout: word(64),
        chunk,
        raid_disks: word(10),
        role: (!faulty && raid_disk < word(10)).then_some(raid_disk),
        base: 0,
        data_offset: 0,
        data_size,
        events,
        checksum_ok: csum == word(38),
        reshaping: word(44) != 0 || word(45) != 0 || word(47) != 0,
    })
}

/// Superblocks on `drive`: on the whole drive, and on each partition
/// (NAS boxes put one array per partition: system, swap, data).
pub fn find(drive: &dyn Source) -> Vec<Superblock> {
    let mut out: Vec<Superblock> = Vec::new();
    for p in partition::partitions(drive).into_iter().take(16) {
        if p.offset == 0 || p.offset.saturating_add(p.len) > drive.len() {
            continue;
        }
        if let Some(sb) = read_device(drive, p.offset, p.len) {
            out.push(sb);
        }
    }
    // A 0.90 superblock on a last partition that ends where the drive
    // does is also where the whole drive's would be; it belongs to the
    // partition.
    if let Some(sb) = read_device(drive, 0, drive.len()) {
        if !out.iter().any(|o| o.uuid == sb.uuid) {
            out.insert(0, sb);
        }
    }
    out
}

/// An array put together from md superblocks.
#[derive(Debug, Clone)]
pub struct Assembled {
    pub layout: Layout,
    pub uuid: String,
    pub name: String,
    pub version: Version,
    /// Things the user should know: stale or failed members left out, bad
    /// checksums, a reshape in progress.
    pub notes: Vec<String>,
    /// Whether every check passed (checksums, a consistent set).
    pub clean: bool,
}

/// Puts arrays together from the superblocks on `drives`. Arrays with a
/// level Reperio cannot read (RAID 6) are reported as notes with no
/// layout, so they are not silently ignored.
pub fn assemble(drives: &[Arc<dyn Source>]) -> (Vec<Assembled>, Vec<String>) {
    let mut found: Vec<(usize, Superblock)> = Vec::new();
    for (i, d) in drives.iter().enumerate() {
        for sb in find(d.as_ref()) {
            found.push((i, sb));
        }
    }
    let mut uuids: Vec<[u8; 16]> = Vec::new();
    for (_, sb) in &found {
        if !uuids.contains(&sb.uuid) {
            uuids.push(sb.uuid);
        }
    }
    let mut arrays = Vec::new();
    let mut problems = Vec::new();
    for uuid in uuids {
        let set: Vec<&(usize, Superblock)> = found.iter().filter(|(_, s)| s.uuid == uuid).collect();
        // The most recent superblock speaks for the array.
        let Some(head) = set.iter().map(|(_, s)| s).max_by_key(|s| s.events) else {
            continue;
        };
        let id = if head.name.is_empty() {
            head.uuid_string()
        } else {
            format!("\"{}\"", head.name)
        };
        let Some(level) = head.level() else {
            problems.push(format!(
                "Array {id} is md level {} (layout {}), which Reperio cannot combine yet.",
                head.level, head.layout
            ));
            continue;
        };
        let n = head.raid_disks as usize;
        if n == 0 || n > MAX_MEMBERS {
            problems.push(format!(
                "Array {id} says it has {n} drives; its superblock is damaged."
            ));
            continue;
        }
        if level.striped() && (head.chunk < 512 || head.chunk > MAX_CHUNK) {
            problems.push(format!(
                "Array {id} has an impossible chunk size; its superblock is damaged."
            ));
            continue;
        }
        let mut notes = Vec::new();
        let mut clean = true;
        let mut slots: Vec<Option<&Superblock>> = vec![None; n];
        let mut order: Vec<Option<usize>> = vec![None; n];
        for (drive, sb) in &set {
            if !sb.checksum_ok {
                notes.push(format!(
                    "Drive {}: the superblock checksum is wrong.",
                    drive + 1
                ));
                clean = false;
            }
            let Some(role) = sb.role.map(|r| r as usize).filter(|&r| r < n) else {
                notes.push(format!(
                    "Drive {} is a spare or failed member; not used.",
                    drive + 1
                ));
                continue;
            };
            if slots[role].is_none_or(|s| sb.events > s.events) {
                slots[role] = Some(sb);
                order[role] = Some(*drive);
            }
        }
        // A member that dropped out of a redundant array earlier holds
        // old data; md would not use it, and neither do we, as long as
        // the others are enough.
        let newest = slots.iter().flatten().map(|s| s.events).max().unwrap_or(0);
        let stale: Vec<usize> = (0..n)
            .filter(|&r| slots[r].is_some_and(|s| s.events + 1 < newest))
            .collect();
        let present = order.iter().filter(|o| o.is_some()).count();
        let spare = level.spare_members();
        if !stale.is_empty() && n - present + stale.len() <= spare {
            for &r in &stale {
                notes.push(format!(
                    "Drive {} dropped out of the array earlier and holds old data; it is left out.",
                    order[r].unwrap() + 1
                ));
                slots[r] = None;
                order[r] = None;
            }
        } else if !stale.is_empty() {
            notes.push(
                "Some drives dropped out of the array earlier; parts of what they hold may be old."
                    .into(),
            );
            clean = false;
        }
        let missing = order.iter().filter(|o| o.is_none()).count();
        if missing > 0 {
            notes.push(format!("{missing} of {n} drives are missing."));
        }
        if head.reshaping {
            notes.push("The array was being reshaped; part of it may read wrongly.".into());
            clean = false;
        }
        // A missing member's size is that of the others (they match for
        // every level but linear).
        let typical = slots
            .iter()
            .flatten()
            .map(|s| s.data_size)
            .min()
            .unwrap_or(0);
        let layout = Layout {
            level,
            chunk: if level.striped() { head.chunk } else { 0 },
            offsets: slots
                .iter()
                .map(|s| s.map_or(0, |s| s.base.saturating_add(s.data_offset)))
                .collect(),
            sizes: slots
                .iter()
                .map(|s| s.map_or(typical, |s| s.data_size))
                .collect(),
            order,
        };
        arrays.push(Assembled {
            layout,
            uuid: head.uuid_string(),
            name: head.name.clone(),
            version: head.version,
            notes,
            clean,
        });
    }
    (arrays, problems)
}

/// Writes superblocks, for tests (the integration tests use a script that
/// writes them independently; this one is for the unit tests).
#[cfg(test)]
pub(crate) mod write {
    use super::*;

    pub struct Spec {
        pub level: i32,
        pub layout: u32,
        pub chunk: u64,
        pub raid_disks: u32,
        pub role: u32,
        pub data_offset: u64,
        pub data_size: u64,
        pub events: u64,
    }

    /// A 1.x superblock for a device of `len` bytes, at sector `sector`.
    pub fn v1(s: &Spec, sector: u64) -> Vec<u8> {
        let mut b = vec![0u8; SB_BYTES];
        let put32 =
            |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let put64 =
            |b: &mut [u8], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put32(&mut b, 0, MAGIC);
        put32(&mut b, 4, 1);
        b[16..32].copy_from_slice(&[7u8; 16]);
        b[32..38].copy_from_slice(b"nas:md");
        put32(&mut b, 72, s.level as u32);
        put32(&mut b, 76, s.layout);
        put64(&mut b, 80, s.data_size / 512);
        put32(&mut b, 88, (s.chunk / 512) as u32);
        put32(&mut b, 92, s.raid_disks);
        put64(&mut b, 128, s.data_offset / 512);
        put64(&mut b, 136, s.data_size / 512);
        put64(&mut b, 144, sector);
        put32(&mut b, 160, s.role);
        put64(&mut b, 200, s.events);
        put32(&mut b, 220, s.raid_disks);
        for r in 0..s.raid_disks as usize {
            b[256 + r * 2..258 + r * 2].copy_from_slice(&(r as u16).to_le_bytes());
        }
        let size = 256 + s.raid_disks as usize * 2;
        let mut sum = 0u64;
        let mut i = 0;
        while i + 4 <= size {
            sum += u64::from(le32(&b, i));
            i += 4;
        }
        if size - i == 2 {
            sum += u64::from(u16::from_le_bytes([b[i], b[i + 1]]));
        }
        put32(&mut b, 216, ((sum & 0xffff_ffff) + (sum >> 32)) as u32);
        b
    }

    pub fn v090(s: &Spec) -> Vec<u8> {
        let mut w = vec![0u32; SB_BYTES / 4];
        w[0] = MAGIC;
        w[2] = 90;
        w[5] = 0x1234_5678;
        w[7] = s.level as u32;
        w[8] = (s.data_size / 1024) as u32;
        w[9] = s.raid_disks;
        w[10] = s.raid_disks;
        w[13] = 1;
        w[39] = s.events as u32;
        w[64] = s.layout;
        w[65] = s.chunk as u32;
        w[992] = s.role;
        w[995] = s.role;
        w[996] = 6;
        let sum: u64 = w.iter().map(|&x| u64::from(x)).sum();
        w[38] = ((sum & 0xffff_ffff) + (sum >> 32)) as u32;
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::write::*;
    use super::*;
    use crate::raid::{split, Parity};

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn assembles_v12_raid5_with_a_member_missing() {
        let vol = data(1 << 20);
        let chunk = 65536;
        let level = Level::Raid5 {
            parity: Parity::LeftSymmetric,
        };
        let members = split(&vol, level, chunk, 4);
        let per = members[0].len() as u64;
        let off = 1 << 20;
        let mut drives: Vec<Arc<dyn Source>> = Vec::new();
        // Shuffled: role 2, 0, 3 present; role 1 missing.
        for role in [2u32, 0, 3] {
            let mut d = vec![0u8; (off + per) as usize];
            let sb = v1(
                &Spec {
                    level: 5,
                    layout: 2,
                    chunk,
                    raid_disks: 4,
                    role,
                    data_offset: off,
                    data_size: per,
                    events: 10,
                },
                8,
            );
            d[4096..8192].copy_from_slice(&sb);
            d[off as usize..].copy_from_slice(&members[role as usize]);
            drives.push(Arc::new(d));
        }
        let (arrays, problems) = assemble(&drives);
        assert!(problems.is_empty());
        assert_eq!(arrays.len(), 1);
        let a = &arrays[0];
        assert!(a.clean, "{:?}", a.notes);
        assert_eq!(a.version, Version::V1_2);
        assert_eq!(a.layout.order, vec![Some(1), None, Some(0), Some(2)]);
        let src = a.layout.build(&drives).unwrap();
        let mut got = vec![0u8; vol.len()];
        src.read_at(0, &mut got).unwrap();
        assert_eq!(got, vol);
    }

    #[test]
    fn assembles_v090_raid0() {
        let vol = data(512 << 10);
        let members = split(&vol, Level::Raid0, 16384, 2);
        let per = members[0].len();
        let mut drives: Vec<Arc<dyn Source>> = Vec::new();
        for role in [1u32, 0] {
            // Room for the superblock in the last 64 KiB-aligned 64 KiB.
            let len = per + 128 * 1024;
            let mut d = vec![0u8; len];
            d[..per].copy_from_slice(&members[role as usize]);
            let at = ((len / 512) & !127) * 512 - 65536;
            let sb = v090(&Spec {
                level: 0,
                layout: 0,
                chunk: 16384,
                raid_disks: 2,
                role,
                data_offset: 0,
                data_size: per as u64,
                events: 3,
            });
            d[at..at + 4096].copy_from_slice(&sb);
            drives.push(Arc::new(d));
        }
        let (arrays, _) = assemble(&drives);
        assert_eq!(arrays.len(), 1);
        assert!(arrays[0].clean);
        assert_eq!(arrays[0].layout.order, vec![Some(1), Some(0)]);
        let src = arrays[0].layout.build(&drives).unwrap();
        let mut got = vec![0u8; vol.len()];
        src.read_at(0, &mut got).unwrap();
        assert_eq!(got, vol);
    }

    #[test]
    fn damaged_superblocks_do_not_panic() {
        let spec = Spec {
            level: 5,
            layout: 2,
            chunk: 65536,
            raid_disks: 4,
            role: 1,
            data_offset: 1 << 20,
            data_size: 1 << 20,
            events: 1,
        };
        let good1 = v1(&spec, 8);
        let good0 = v090(&spec);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..20000 {
            let mut b = if rnd() % 2 == 0 {
                good1.clone()
            } else {
                good0.clone()
            };
            for _ in 0..1 + rnd() % 8 {
                let at = (rnd() % 1100) as usize;
                let v = rnd();
                match rnd() % 3 {
                    0 => b[at] = v as u8,
                    1 => b[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes()),
                    _ => b[at..at + 4].copy_from_slice(&[0xff; 4]),
                }
            }
            if let Some(sb) = parse_v1(&b, Version::V1_2, 8, 4 << 20) {
                let _ = sb.level();
            }
            if let Some(sb) = parse_v090(&b, 4 << 20) {
                let _ = sb.level();
            }
            // And whole assembly from a drive carrying it.
            let mut d = vec![0u8; 2 << 20];
            d[4096..8192].copy_from_slice(&b);
            let drives: Vec<Arc<dyn Source>> = vec![Arc::new(d)];
            let (arrays, _) = assemble(&drives);
            for a in arrays {
                if let Ok(src) = a.layout.build(&drives) {
                    let mut buf = vec![0u8; 8192];
                    let _ = src.read_at(src.len().saturating_sub(100), &mut buf);
                    let _ = src.read_at(0, &mut buf);
                }
            }
        }
    }
}

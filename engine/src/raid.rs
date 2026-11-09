//! Drives from a RAID set (a dead NAS, a RAID enclosure, a striped
//! Windows or Mac volume) combined back into the one drive they made up,
//! so it can be scanned like any other.
//!
//! [`RaidSource`] presents the member drives as a single [`Source`] for a
//! given [`Layout`]. Layouts come from Linux md superblocks when the
//! drives have them ([`md`]), which is how Synology, QNAP and most Linux
//! NAS boxes build their arrays, or are worked out from the data itself
//! ([`detect`]).
//!
//! Members can be missing. RAID 1 and RAID 10 read any copy that is there;
//! RAID 5 rebuilds a missing or unreadable member's data from the others.
//! Nothing is ever written to any member.

pub mod detect;
pub mod md;

use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::source::Source;

/// Where RAID 5 keeps each stripe's parity, named as Linux md names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Parity {
    LeftAsymmetric,
    RightAsymmetric,
    /// md's default, and what most NAS boxes use.
    LeftSymmetric,
    RightSymmetric,
    /// Parity always on the first member.
    First,
    /// Parity always on the last member (RAID 4).
    Last,
}

impl Parity {
    /// The four rotating layouts md offers, most common first.
    pub const ROTATING: [Parity; 4] = [
        Parity::LeftSymmetric,
        Parity::LeftAsymmetric,
        Parity::RightSymmetric,
        Parity::RightAsymmetric,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Parity::LeftAsymmetric => "left-asymmetric",
            Parity::RightAsymmetric => "right-asymmetric",
            Parity::LeftSymmetric => "left-symmetric",
            Parity::RightSymmetric => "right-symmetric",
            Parity::First => "parity-first",
            Parity::Last => "parity-last",
        }
    }

    pub fn parse(s: &str) -> Option<Parity> {
        let s = s.to_ascii_lowercase();
        [
            Parity::LeftAsymmetric,
            Parity::RightAsymmetric,
            Parity::LeftSymmetric,
            Parity::RightSymmetric,
            Parity::First,
            Parity::Last,
        ]
        .into_iter()
        .find(|p| {
            let name = p.name();
            s == name || s == name.replace('-', "") || {
                // ls, la, rs, ra
                let mut short = String::new();
                for part in name.split('-') {
                    short.push(part.as_bytes()[0] as char);
                }
                s == short
            }
        })
    }

    /// The member holding the parity of `stripe`, and the member holding
    /// data chunk `d` (0-based within the stripe), in an array of `n`
    /// members. Follows raid5_compute_sector in Linux's drivers/md/raid5.c.
    fn place(self, stripe: u64, d: u64, n: u64) -> (u64, u64) {
        let data = n - 1;
        match self {
            Parity::LeftAsymmetric => {
                let p = data - stripe % n;
                (p, if d >= p { d + 1 } else { d })
            }
            Parity::RightAsymmetric => {
                let p = stripe % n;
                (p, if d >= p { d + 1 } else { d })
            }
            Parity::LeftSymmetric => {
                let p = data - stripe % n;
                (p, (p + 1 + d) % n)
            }
            Parity::RightSymmetric => {
                let p = stripe % n;
                (p, (p + 1 + d) % n)
            }
            Parity::First => (0, d + 1),
            Parity::Last => (data, d),
        }
    }
}

/// How the members' data is arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Level {
    /// Members one after another (md "linear", JBOD, spanned volumes).
    Jbod,
    /// Striped, no redundancy.
    Raid0,
    /// Every member a full copy.
    Raid1,
    /// Striped with one rotating parity chunk per stripe (also RAID 4).
    Raid5 { parity: Parity },
    /// Linux md RAID 10: `near` copies of each chunk on neighbouring
    /// members, `far` copies further down the members; `offset` puts the
    /// far copies in the next stripe instead (md's "o" layouts).
    Raid10 { near: u8, far: u8, offset: bool },
}

impl Level {
    pub fn name(self) -> String {
        match self {
            Level::Jbod => "JBOD (spanned)".into(),
            Level::Raid0 => "RAID 0".into(),
            Level::Raid1 => "RAID 1".into(),
            Level::Raid5 {
                parity: Parity::Last,
            } => "RAID 4".into(),
            Level::Raid5 { parity } => format!("RAID 5 ({})", parity.name()),
            Level::Raid10 {
                near,
                far: 1,
                offset: _,
            } => format!("RAID 10 (near {near})"),
            Level::Raid10 {
                near: 1,
                far,
                offset,
            } => format!("RAID 10 ({} {far})", if offset { "offset" } else { "far" }),
            Level::Raid10 { near, far, offset } => format!(
                "RAID 10 (near {near}, {} {far})",
                if offset { "offset" } else { "far" }
            ),
        }
    }

    /// Whether the chunk size matters.
    pub fn striped(self) -> bool {
        !matches!(self, Level::Jbod | Level::Raid1)
    }

    /// How many members can be missing with all the data still there.
    fn spare_members(self) -> usize {
        match self {
            Level::Jbod | Level::Raid0 => 0,
            Level::Raid1 => usize::MAX,
            Level::Raid5 { .. } => 1,
            // At least near*far - 1, if the right ones are missing; which
            // ones are is checked chunk by chunk when reading.
            Level::Raid10 { near, far, .. } => usize::from(near) * usize::from(far) - 1,
        }
    }
}

/// Upper bounds on values that come from the user or from disk metadata.
pub const MAX_MEMBERS: usize = 64;
/// md allows chunks of up to 4 GiB in principle; nobody uses more than a
/// few MiB, and the bound keeps a damaged superblock's chunk size harmless.
pub const MAX_CHUNK: u64 = 256 << 20;

/// A complete description of an array, in terms of the drives given to
/// [`Layout::build`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Layout {
    pub level: Level,
    /// Stripe unit in bytes; ignored by JBOD and RAID 1.
    pub chunk: u64,
    /// For each position in the array, the index of the drive there among
    /// the drives given, or `None` where a member is missing.
    pub order: Vec<Option<usize>>,
    /// For each position, where its data starts on its drive.
    pub offsets: Vec<u64>,
    /// For each position, bytes of array data on its drive; 0 means up to
    /// the end of the drive.
    pub sizes: Vec<u64>,
}

impl Layout {
    /// One line for people: "RAID 5 (left-symmetric), 4 drives (1 missing), 512 KiB chunks".
    pub fn describe(&self) -> String {
        let missing = self.order.iter().filter(|o| o.is_none()).count();
        let mut s = format!("{}, {} drives", self.level.name(), self.order.len());
        if missing > 0 {
            s.push_str(&format!(" ({missing} missing)"));
        }
        if self.level.striped() {
            s.push_str(&format!(", {} chunks", size_name(self.chunk)));
        }
        s
    }

    /// Opens the array over `drives` (indexed by `order`).
    pub fn build(&self, drives: &[Arc<dyn Source>]) -> Result<RaidSource, String> {
        let members = self
            .order
            .iter()
            .map(|o| match o {
                Some(i) => drives
                    .get(*i)
                    .cloned()
                    .map(Some)
                    .ok_or_else(|| format!("there is no drive {}", i + 1)),
                None => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()?;
        RaidSource::new(
            self.level,
            self.chunk,
            members,
            self.offsets.clone(),
            self.sizes.clone(),
        )
    }
}

/// Reads "64K", "512 KiB", "1M", "4096" as bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: u64 = s[..digits].parse().ok()?;
    let mult = match s[digits..].trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Reads an array order such as "2,1,-,3": drive numbers counted from 1 in
/// the order the drives were given, `-` or `x` for a missing member.
pub fn parse_order(s: &str, drives: usize) -> Result<Vec<Option<usize>>, String> {
    let mut seen = vec![false; drives];
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part == "-" || part.eq_ignore_ascii_case("x") || part == "?" {
            out.push(None);
            continue;
        }
        let n: usize = part
            .parse()
            .map_err(|_| format!("\"{part}\" is not a drive number"))?;
        if n == 0 || n > drives {
            return Err(format!("there is no drive {n} (give 1 to {drives})"));
        }
        if std::mem::replace(&mut seen[n - 1], true) {
            return Err(format!("drive {n} is named twice"));
        }
        out.push(Some(n - 1));
    }
    if out.len() > MAX_MEMBERS {
        return Err("too many drives".into());
    }
    Ok(out)
}

/// "512 KiB", "4 MiB".
pub fn size_name(n: u64) -> String {
    if n >= 1 << 20 && n.is_multiple_of(1 << 20) {
        format!("{} MiB", n >> 20)
    } else if n >= 1 << 10 && n.is_multiple_of(1 << 10) {
        format!("{} KiB", n >> 10)
    } else {
        format!("{n} bytes")
    }
}

/// An array of member drives read as one drive.
pub struct RaidSource {
    level: Level,
    chunk: u64,
    members: Vec<Option<Arc<dyn Source>>>,
    offsets: Vec<u64>,
    /// Bytes of array data on each member.
    sizes: Vec<u64>,
    /// JBOD: where each member begins in the array.
    starts: Vec<u64>,
    /// RAID 10 far layouts: distance between copies on a member.
    stride: u64,
    len: u64,
}

impl RaidSource {
    /// `members` are in array order, `None` where one is missing.
    /// `offsets` give where each member's data starts, `sizes` how much
    /// there is (0: to the end of the member).
    pub fn new(
        level: Level,
        chunk: u64,
        members: Vec<Option<Arc<dyn Source>>>,
        offsets: Vec<u64>,
        sizes: Vec<u64>,
    ) -> Result<Self, String> {
        let n = members.len();
        if n == 0 || n > MAX_MEMBERS {
            return Err(format!("an array needs 1 to {MAX_MEMBERS} drives"));
        }
        if offsets.len() != n || sizes.len() != n {
            return Err("give one data offset and size per drive".into());
        }
        let missing = members.iter().filter(|m| m.is_none()).count();
        if missing == n {
            return Err("none of the drives is there".into());
        }
        if missing > level.spare_members() {
            return Err(format!(
                "{} cannot be read with {missing} of its drives missing",
                level.name()
            ));
        }
        match level {
            Level::Raid5 { .. } if n < 2 => return Err("RAID 5 needs at least 2 drives".into()),
            Level::Raid10 { near, far, .. } => {
                let copies = usize::from(near) * usize::from(far);
                if near == 0 || far == 0 || copies > n {
                    return Err("RAID 10 needs at least as many drives as copies".into());
                }
            }
            _ => {}
        }
        if level.striped() && (!(512..=MAX_CHUNK).contains(&chunk) || !chunk.is_multiple_of(512)) {
            return Err(format!(
                "the chunk size must be a multiple of 512 bytes, up to {}",
                size_name(MAX_CHUNK)
            ));
        }

        // What each present member can actually hold.
        let mut sizes: Vec<u64> = (0..n)
            .map(|i| {
                let room = members[i]
                    .as_ref()
                    .map(|m| m.len().saturating_sub(offsets[i]));
                match (sizes[i], room) {
                    (0, Some(room)) => room,
                    (s, Some(room)) => s.min(room),
                    (s, None) => s,
                }
            })
            .collect();

        let mut stride = 0;
        let mut starts = Vec::new();
        let len = if level == Level::Jbod {
            if sizes.contains(&0) {
                return Err("the size of a missing drive must be known to span past it".into());
            }
            let mut at = 0u64;
            for &s in &sizes {
                starts.push(at);
                at = at.saturating_add(s);
            }
            at
        } else {
            // Striped and mirrored members all hold the same amount: the
            // least any present member holds.
            let per = (0..n)
                .filter(|&i| members[i].is_some())
                .map(|i| sizes[i])
                .min()
                .unwrap_or(0);
            let per = if level.striped() {
                per / chunk * chunk
            } else {
                per
            };
            sizes.iter_mut().for_each(|s| *s = per);
            let n = n as u64;
            match level {
                Level::Raid0 => per.saturating_mul(n),
                Level::Raid1 => per,
                Level::Raid5 { .. } => per.saturating_mul(n - 1),
                Level::Raid10 { near, far, offset } => {
                    // As raid10_size in drivers/md/raid10.c.
                    let (near, far) = (u64::from(near), u64::from(far));
                    let chunks = per / chunk / far;
                    stride = if offset { chunk } else { chunks * chunk };
                    (chunks * n / near).saturating_mul(chunk)
                }
                Level::Jbod => unreachable!(),
            }
        };
        Ok(Self {
            level,
            chunk,
            members,
            offsets,
            sizes,
            starts,
            stride,
            len,
        })
    }

    pub fn level(&self) -> Level {
        self.level
    }

    /// Reads exactly `buf.len()` bytes of member `m`'s data at `at`.
    fn member(&self, m: usize, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let Some(src) = &self.members[m] else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("drive {} of the array is missing", m + 1),
            ));
        };
        let got = src.read_at(self.offsets[m].saturating_add(at), buf)?;
        if got < buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("drive {} of the array ends early", m + 1),
            ));
        }
        Ok(())
    }

    /// Reads the first copy that can be read.
    fn any_copy(&self, copies: &[(u64, u64)], buf: &mut [u8]) -> io::Result<()> {
        let mut last = None;
        for &(m, at) in copies {
            match self.member(m as usize, at, buf) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no copy of this part of the array")))
    }

    /// Rebuilds member `lost`'s data at `at` from every other member.
    fn rebuild(&self, lost: u64, at: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        let mut other = vec![0u8; buf.len()];
        for m in 0..self.members.len() {
            if m as u64 == lost {
                continue;
            }
            self.member(m, at, &mut other)?;
            buf.iter_mut().zip(&other).for_each(|(b, o)| *b ^= o);
        }
        Ok(())
    }

    /// Reads one piece that lies within a single chunk (or member).
    /// Returns how many bytes of `buf` it covered.
    fn read_piece(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.members.len() as u64;
        let chunk = self.chunk;
        match self.level {
            Level::Jbod => {
                let i = self.starts.partition_point(|&s| s <= pos) - 1;
                let within = pos - self.starts[i];
                let k = (self.sizes[i] - within).min(buf.len() as u64) as usize;
                self.member(i, within, &mut buf[..k])?;
                Ok(k)
            }
            Level::Raid1 => {
                let k = buf.len().min(1 << 20);
                let copies: Vec<(u64, u64)> = (0..n).map(|m| (m, pos)).collect();
                self.any_copy(&copies, &mut buf[..k])?;
                Ok(k)
            }
            Level::Raid0 => {
                let (c, within) = (pos / chunk, pos % chunk);
                let k = (chunk - within).min(buf.len() as u64) as usize;
                self.member((c % n) as usize, (c / n) * chunk + within, &mut buf[..k])?;
                Ok(k)
            }
            Level::Raid5 { parity } => {
                let (c, within) = (pos / chunk, pos % chunk);
                let k = (chunk - within).min(buf.len() as u64) as usize;
                let stripe = c / (n - 1);
                let (_, m) = parity.place(stripe, c % (n - 1), n);
                let at = stripe * chunk + within;
                let buf = &mut buf[..k];
                if self.member(m as usize, at, buf).is_err() {
                    self.rebuild(m, at, buf)?;
                }
                Ok(k)
            }
            Level::Raid10 { near, far, .. } => {
                let (c, within) = (pos / chunk, pos % chunk);
                let k = (chunk - within).min(buf.len() as u64) as usize;
                let copies = self.raid10_copies(c, within, near, far);
                self.any_copy(&copies, &mut buf[..k])?;
                Ok(k)
            }
        }
    }

    /// Where the copies of array chunk `c` are, as raid10_find_phys in
    /// drivers/md/raid10.c works them out.
    fn raid10_copies(&self, c: u64, within: u64, near: u8, far: u8) -> Vec<(u64, u64)> {
        let n = self.members.len() as u64;
        let (near, far) = (u64::from(near), u64::from(far));
        let offset = matches!(self.level, Level::Raid10 { offset: true, .. });
        let first = c * near;
        let mut dev = first % n;
        let mut stripe = first / n;
        if offset {
            stripe *= far;
        }
        let mut at = stripe * self.chunk + within;
        let mut out = Vec::with_capacity((near * far) as usize);
        for _ in 0..near {
            let (mut d, mut s) = (dev, at);
            out.push((d, s));
            for _ in 1..far {
                d = (d + near) % n;
                s += self.stride;
                out.push((d, s));
            }
            dev += 1;
            if dev >= n {
                dev = 0;
                at += self.chunk;
            }
        }
        out
    }
}

impl Source for RaidSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = buf.len().min((self.len - offset) as usize);
        let mut done = 0;
        while done < want {
            done += self.read_piece(offset + done as u64, &mut buf[done..want])?;
        }
        Ok(want)
    }
}

/// Lays `volume` out over `n` members as `level` would, for tests: the
/// inverse of [`RaidSource`]. Members are returned in array order, each
/// `per` bytes long (data only, no offsets).
#[cfg(test)]
pub(crate) fn split(volume: &[u8], level: Level, chunk: u64, n: usize) -> Vec<Vec<u8>> {
    let per = match level {
        Level::Jbod | Level::Raid0 => volume.len().div_ceil(n),
        Level::Raid1 => volume.len(),
        Level::Raid5 { .. } => volume.len().div_ceil(n - 1),
        Level::Raid10 { near, far, .. } => {
            let rows = (volume.len() as u64)
                .div_ceil(chunk)
                .saturating_mul(u64::from(near))
                .div_ceil(n as u64);
            (rows * u64::from(far) * chunk) as usize
        }
    };
    let per = (per as u64).div_ceil(chunk.max(1)) * chunk.max(1);
    let mut members = vec![vec![0u8; per as usize]; n];
    // Write through the same layout by building a source over empty
    // members and asking it where each byte goes would be circular; so
    // place bytes with an independent, chunk-by-chunk rendition of the
    // md layouts.
    let nn = n as u64;
    let chunks = (volume.len() as u64).div_ceil(chunk.max(1));
    let get = |c: u64| {
        let s = (c * chunk) as usize;
        let e = (s + chunk as usize).min(volume.len());
        let mut v = vec![0u8; chunk as usize];
        if s < volume.len() {
            v[..e - s].copy_from_slice(&volume[s..e]);
        }
        v
    };
    let mut put = |m: u64, at: u64, data: &[u8]| {
        let at = at as usize;
        members[m as usize][at..at + data.len()].copy_from_slice(data);
    };
    match level {
        Level::Jbod => {
            for (i, piece) in volume.chunks(per as usize).enumerate() {
                put(i as u64, 0, piece);
            }
        }
        Level::Raid1 => {
            for m in 0..nn {
                put(m, 0, volume);
            }
        }
        Level::Raid0 => {
            for c in 0..chunks {
                put(c % nn, c / nn * chunk, &get(c));
            }
        }
        Level::Raid5 { parity } => {
            let stripes = chunks.div_ceil(nn - 1);
            for s in 0..stripes {
                let mut p = vec![0u8; chunk as usize];
                for d in 0..nn - 1 {
                    let data = get(s * (nn - 1) + d);
                    p.iter_mut().zip(&data).for_each(|(a, b)| *a ^= b);
                    let (_, m) = parity.place(s, d, nn);
                    put(m, s * chunk, &data);
                }
                let (pm, _) = parity.place(s, 0, nn);
                put(pm, s * chunk, &p);
            }
        }
        Level::Raid10 { near, far, offset } => {
            // near copies on consecutive devices, wrapping to the next row.
            let (near, far) = (u64::from(near), u64::from(far));
            let rows = per / chunk / far;
            let stride = if offset { chunk } else { rows * chunk };
            for c in 0..chunks {
                let data = get(c);
                for k in 0..near {
                    let slot = c * near + k;
                    let (d, row) = (slot % nn, slot / nn);
                    let row = if offset {
                        // far copies take the following rows in offset mode
                        (c * near / nn) * far + (row - c * near / nn)
                    } else {
                        row
                    };
                    put(d, row * chunk, &data);
                    for f in 1..far {
                        put((d + f * near) % nn, row * chunk + f * stride, &data);
                    }
                }
            }
        }
    }
    members
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn open(members: Vec<Option<Vec<u8>>>, level: Level, chunk: u64) -> RaidSource {
        let n = members.len();
        let members = members
            .into_iter()
            .map(|m| m.map(|m| Arc::new(m) as Arc<dyn Source>))
            .collect();
        RaidSource::new(level, chunk, members, vec![0; n], vec![0; n]).unwrap()
    }

    fn read_all(src: &dyn Source, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        // Odd-sized reads, so pieces straddle chunk boundaries.
        let mut at = 0;
        while at < len {
            let end = (at + 7777).min(len);
            assert_eq!(src.read_at(at as u64, &mut out[at..end]).unwrap(), end - at);
            at = end;
        }
        out
    }

    #[test]
    fn layouts_survive_json() {
        // The app passes a detected layout back to open it.
        for level in [
            Level::Jbod,
            Level::Raid5 {
                parity: Parity::RightSymmetric,
            },
            Level::Raid10 {
                near: 2,
                far: 1,
                offset: false,
            },
        ] {
            let l = Layout {
                level,
                chunk: 65536,
                order: vec![Some(1), None, Some(0)],
                offsets: vec![0, 0, 1 << 20],
                sizes: vec![0; 3],
            };
            let back: Layout = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
            assert_eq!(back, l);
        }
    }

    #[test]
    fn parses_sizes_and_orders() {
        assert_eq!(parse_size("64K"), Some(65536));
        assert_eq!(parse_size("1 MiB"), Some(1 << 20));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("99999999999999999999"), None);
        assert_eq!(
            parse_order("2,1,-,3", 3).unwrap(),
            vec![Some(1), Some(0), None, Some(2)]
        );
        assert!(parse_order("1,1", 2).is_err());
        assert!(parse_order("4", 3).is_err());
        assert!(parse_order("0", 3).is_err());
    }

    #[test]
    fn known_raid5_placement() {
        // The left-symmetric diagram from the md documentation, 3 drives.
        let p = Parity::LeftSymmetric;
        assert_eq!(p.place(0, 0, 3), (2, 0));
        assert_eq!(p.place(0, 1, 3), (2, 1));
        assert_eq!(p.place(1, 0, 3), (1, 2));
        assert_eq!(p.place(1, 1, 3), (1, 0));
        assert_eq!(p.place(2, 0, 3), (0, 1));
        // Left-asymmetric: data skips the parity drive.
        let p = Parity::LeftAsymmetric;
        assert_eq!(p.place(1, 0, 3), (1, 0));
        assert_eq!(p.place(1, 1, 3), (1, 2));
        // Right-symmetric starts with parity on the first drive.
        assert_eq!(Parity::RightSymmetric.place(0, 0, 3), (0, 1));
        assert_eq!(Parity::RightAsymmetric.place(1, 0, 3), (1, 0));
    }

    #[test]
    fn every_level_reads_back_what_was_striped() {
        let data = volume(3 << 20, 7);
        let mut cases = vec![(Level::Jbod, 4096, 3), (Level::Raid1, 4096, 2)];
        for chunk in [4096u64, 65536, 512 << 10] {
            for n in [2, 3, 4] {
                cases.push((Level::Raid0, chunk, n));
            }
            for parity in Parity::ROTATING.into_iter().chain([Parity::Last]) {
                cases.push((Level::Raid5 { parity }, chunk, 4));
                cases.push((Level::Raid5 { parity }, chunk, 3));
            }
            for (near, far, offset) in [(2, 1, false), (1, 2, false), (1, 2, true), (2, 2, false)] {
                cases.push((Level::Raid10 { near, far, offset }, chunk, 4));
            }
            cases.push((
                Level::Raid10 {
                    near: 2,
                    far: 1,
                    offset: false,
                },
                chunk,
                3,
            ));
        }
        for (level, chunk, n) in cases {
            let members = split(&data, level, chunk, n);
            let src = open(members.iter().cloned().map(Some).collect(), level, chunk);
            assert!(src.len() >= data.len() as u64, "{level:?} {chunk} {n}");
            assert_eq!(read_all(&src, data.len()), data, "{level:?} {chunk} {n}");

            // Every redundant level still reads with any one drive gone.
            if level.spare_members() > 0 {
                for gone in 0..n {
                    let mut m: Vec<Option<Vec<u8>>> = members.iter().cloned().map(Some).collect();
                    m[gone] = None;
                    let src = open(m, level, chunk);
                    assert_eq!(
                        read_all(&src, data.len()),
                        data,
                        "{level:?} {chunk} {n} without {gone}"
                    );
                }
            }
        }
    }

    /// A member that fails every read in one range, like a bad patch.
    struct Bad(Vec<u8>, std::ops::Range<u64>);

    impl Source for Bad {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let end = offset + buf.len() as u64;
            if offset < self.1.end && end > self.1.start {
                return Err(io::Error::other("bad sector"));
            }
            self.0.read_at(offset, buf)
        }
    }

    #[test]
    fn raid5_rebuilds_unreadable_sectors() {
        let data = volume(1 << 20, 3);
        let level = Level::Raid5 {
            parity: Parity::LeftSymmetric,
        };
        let members = split(&data, level, 16384, 3);
        let mut srcs: Vec<Option<Arc<dyn Source>>> = members
            .iter()
            .map(|m| Some(Arc::new(m.clone()) as Arc<dyn Source>))
            .collect();
        srcs[1] = Some(Arc::new(Bad(members[1].clone(), 40000..90000)));
        let src = RaidSource::new(level, 16384, srcs, vec![0; 3], vec![0; 3]).unwrap();
        assert_eq!(read_all(&src, data.len()), data);
    }

    #[test]
    fn offsets_and_missing_members() {
        let data = volume(256 << 10, 9);
        let members = split(&data, Level::Raid0, 4096, 2);
        // Each member's data starts at a different place on its drive.
        let a: Vec<u8> = [vec![0xAA; 1024], members[0].clone()].concat();
        let b: Vec<u8> = [vec![0xBB; 8192], members[1].clone()].concat();
        let src = RaidSource::new(
            Level::Raid0,
            4096,
            vec![Some(Arc::new(a)), Some(Arc::new(b))],
            vec![1024, 8192],
            vec![0, 0],
        )
        .unwrap();
        assert_eq!(read_all(&src, data.len()), data);

        // RAID 0 cannot do without a member, and says so on the reads that
        // need it rather than returning zeros.
        assert!(RaidSource::new(
            Level::Raid0,
            4096,
            vec![Some(Arc::new(members[0].clone())), None],
            vec![0, 0],
            vec![0, 0],
        )
        .is_err());
        assert!(RaidSource::new(Level::Raid0, 1000, vec![None], vec![0], vec![0]).is_err());
        assert!(RaidSource::new(Level::Raid0, 4096, vec![], vec![], vec![]).is_err());
    }

    #[test]
    fn unequal_jbod_members() {
        let data = volume(300_000, 5);
        let src = RaidSource::new(
            Level::Jbod,
            0,
            vec![
                Some(Arc::new(data[..100_000].to_vec())),
                Some(Arc::new(data[100_000..].to_vec())),
            ],
            vec![0, 0],
            vec![0, 0],
        )
        .unwrap();
        assert_eq!(src.len(), 300_000);
        assert_eq!(read_all(&src, data.len()), data);
    }
}

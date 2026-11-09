//! Surface test: reads a whole drive (or part of it) block by block,
//! timing every read, to show where it is healthy, where it is slow and
//! where it can't be read at all.
//!
//! A drive that is wearing out retries hard-to-read sectors internally
//! before it gives up on them, so slow areas show up before unreadable
//! ones do. What counts as slow depends on the drive: each block's time is
//! compared with the drive's own median, so a slow USB stick and a fast
//! SSD are judged by the same rule.
//!
//! Like everything in the engine, the test only reads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::source::{read_skipping, Source, ALIGN};

/// Bytes read and timed as one block, unless the range is so large that
/// [`MAX_BLOCKS`] needs bigger ones.
pub const DEFAULT_BLOCK: u64 = 1 << 20;
/// Most blocks a map holds, which bounds its memory (a few bytes each).
pub const MAX_BLOCKS: u64 = 1 << 22;
/// Largest single read; bigger blocks are read in pieces of this size.
const PIECE: usize = 8 << 20;
/// Unreadable blocks in a row after which the rest of the damaged area is
/// skipped over: every failed read can take seconds on a failing drive.
const DAMAGE_RUN: u32 = 4;
/// Readable blocks timed before slow ones are told apart; until then the
/// drive's normal speed is not known.
const WARM_UP: u64 = 16;
/// Read times are counted in buckets an eighth of an octave wide, which
/// gives the median to within about 5% in a fixed, small table.
const BUCKETS: usize = 8 * 33;

/// How a block read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Not read yet.
    Untested,
    /// At about the drive's normal speed.
    Fast,
    /// Several times slower than normal: the drive had to retry.
    Slow,
    /// Many times slower: the area is struggling.
    VerySlow,
    /// Not read: it lies in a damaged area that was skipped over, so it is
    /// most likely unreadable too.
    Skipped,
    /// At least one sector could not be read.
    Unreadable,
}

impl Class {
    /// One character per class, as maps are stored and drawn.
    pub fn symbol(self) -> char {
        match self {
            Class::Untested => '_',
            Class::Fast => '.',
            Class::Slow => 's',
            Class::VerySlow => 'S',
            Class::Skipped => '?',
            Class::Unreadable => 'X',
        }
    }

    pub fn from_symbol(c: char) -> Option<Class> {
        Some(match c {
            '_' => Class::Untested,
            '.' => Class::Fast,
            's' => Class::Slow,
            'S' => Class::VerySlow,
            '?' => Class::Skipped,
            'X' => Class::Unreadable,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SurfaceOptions {
    /// First byte to test.
    pub start: u64,
    /// End of the range to test; `None` for the end of the drive.
    pub end: Option<u64>,
    /// Bytes per block (rounded up to [`ALIGN`]).
    pub block: u64,
}

impl Default for SurfaceOptions {
    fn default() -> Self {
        Self {
            start: 0,
            end: None,
            block: DEFAULT_BLOCK,
        }
    }
}

/// The state of a surface test: how every block read so far. It can be
/// saved (as JSON) and given back to [`run`] to continue a stopped test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceMap {
    /// Size of the drive tested, so a map is never continued on another.
    pub source_len: u64,
    pub start: u64,
    pub end: u64,
    pub block: u64,
    /// Blocks before this one have been read (or skipped).
    pub next: u64,
    /// One per block, stored as a string of [`Class::symbol`]s.
    #[serde(with = "symbols")]
    pub classes: Vec<Class>,
    pub bad_sectors: u64,
    /// How many readable blocks took each time; see [`BUCKETS`].
    hist: Vec<u64>,
    /// Microseconds each block took, for blocks read in this session
    /// (`u32::MAX` for the others), so they can be judged again as the
    /// drive's median becomes known.
    #[serde(skip)]
    times: Vec<u32>,
    /// The median the blocks were last judged against.
    #[serde(skip)]
    judged_by: u64,
}

impl SurfaceMap {
    pub fn new(source_len: u64, opts: &SurfaceOptions) -> Self {
        let start = opts.start.min(source_len) / ALIGN * ALIGN;
        let end = opts.end.unwrap_or(source_len).min(source_len).max(start);
        let span = end - start;
        let block = opts
            .block
            .max(span.div_ceil(MAX_BLOCKS))
            .max(ALIGN)
            .div_ceil(ALIGN)
            .saturating_mul(ALIGN);
        let blocks = span.div_ceil(block) as usize;
        Self {
            source_len,
            start,
            end,
            block,
            next: 0,
            classes: vec![Class::Untested; blocks],
            bad_sectors: 0,
            hist: vec![0; BUCKETS],
            times: vec![u32::MAX; blocks],
            judged_by: 0,
        }
    }

    /// Reads a saved map. It is refused unless it describes a test of a
    /// drive of `source_len` bytes and is consistent in itself, since it
    /// may have been edited or damaged.
    pub fn from_json(json: &str, source_len: u64) -> Result<Self, String> {
        let mut m: SurfaceMap =
            serde_json::from_str(json).map_err(|e| format!("not a surface map: {e}"))?;
        let span = m.end.checked_sub(m.start).ok_or("its range is backwards")?;
        if m.source_len != source_len {
            return Err("it is from a drive of another size".into());
        }
        if m.end > source_len
            || m.block < ALIGN
            || !m.block.is_multiple_of(ALIGN)
            || span.div_ceil(m.block) != m.classes.len() as u64
            || m.classes.len() as u64 > MAX_BLOCKS
            || m.next > m.classes.len() as u64
            || m.hist.len() != BUCKETS
        {
            return Err("it is damaged".into());
        }
        m.times = vec![u32::MAX; m.classes.len()];
        Ok(m)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn blocks(&self) -> u64 {
        self.classes.len() as u64
    }

    /// Where block `i` starts and how long it is.
    pub fn block_range(&self, i: u64) -> (u64, u64) {
        let at = self.start + i * self.block;
        (at, self.block.min(self.end - at))
    }

    /// Bytes read (or skipped) so far, and in all.
    pub fn done(&self) -> u64 {
        (self.next * self.block).min(self.end - self.start)
    }

    pub fn total(&self) -> u64 {
        self.end - self.start
    }

    /// Whether every block has been read.
    pub fn finished(&self) -> bool {
        self.next >= self.blocks()
    }

    /// The median time to read one readable block, in microseconds.
    pub fn median_micros(&self) -> Option<u64> {
        let total: u64 = self.hist.iter().sum();
        if total == 0 {
            return None;
        }
        let mut seen = 0;
        for (b, &n) in self.hist.iter().enumerate() {
            seen += n;
            if seen * 2 >= total {
                return Some(2f64.powf((b as f64 + 0.5) / 8.0).round() as u64);
            }
        }
        None
    }

    fn readable_timed(&self) -> u64 {
        self.hist.iter().sum()
    }

    /// The map at a coarser scale: `cells` characters, each the worst
    /// class among its blocks (untested only if none was read).
    pub fn coarse(&self, cells: usize) -> String {
        let n = self.classes.len();
        let cells = cells.clamp(1, 1 << 16).min(n);
        (0..cells)
            .map(|c| {
                let from = c * n / cells;
                let to = ((c + 1) * n / cells).max(from + 1);
                self.classes[from..to]
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(Class::Untested)
                    .symbol()
            })
            .collect()
    }

    /// Judges a read time against the drive's median. Both a ratio and a
    /// floor must be passed: on a fast SSD a block that waited 30 ms for
    /// a busy computer is many times the median but says nothing about
    /// the drive, while a drive retrying a weak sector takes hundreds of
    /// milliseconds. Floors scale with the block size.
    fn judge(&self, micros: u32, median: u64) -> Class {
        if median == 0 {
            return Class::Fast;
        }
        let scale = (self.block as f64 / (1u64 << 20) as f64).max(0.25);
        let t = f64::from(micros);
        let m = median as f64;
        if t >= 15.0 * m && t >= 300_000.0 * scale {
            Class::VerySlow
        } else if t >= 5.0 * m && t >= 60_000.0 * scale {
            Class::Slow
        } else {
            Class::Fast
        }
    }

    /// Judges every block timed in this session again against the median
    /// as now known.
    fn rejudge(&mut self) {
        let median = self.median_micros().unwrap_or(0);
        self.judged_by = median;
        for i in 0..self.classes.len() {
            let t = self.times[i];
            if t != u32::MAX {
                self.classes[i] = self.judge(t, median);
            }
        }
    }

    fn record_time(&mut self, i: usize, micros: u32) {
        self.times[i] = micros;
        let b = ((f64::from(micros.max(1))).log2() * 8.0) as usize;
        self.hist[b.min(BUCKETS - 1)] += 1;
        let timed = self.readable_timed();
        let median = self.median_micros().unwrap_or(0);
        // Judge everything again once the drive's speed is known, and
        // whenever it has moved by more than a quarter since.
        let drifted = self.judged_by == 0
            || median * 4 > self.judged_by * 5
            || median * 5 < self.judged_by * 4;
        if timed == WARM_UP || (timed > WARM_UP && drifted) {
            self.rejudge();
        } else {
            self.classes[i] = if timed < WARM_UP {
                Class::Fast
            } else {
                self.judge(micros, self.judged_by)
            };
        }
    }

    /// Counts and figures for showing the result.
    pub fn summary(&self) -> Summary {
        let mut s = Summary {
            total: self.total(),
            tested: self.done(),
            block: self.block,
            bad_sectors: self.bad_sectors,
            ..Default::default()
        };
        for c in &self.classes {
            match c {
                Class::Untested => {}
                Class::Fast => s.fast += 1,
                Class::Slow => s.slow += 1,
                Class::VerySlow => s.very_slow += 1,
                Class::Skipped => s.skipped += 1,
                Class::Unreadable => s.unreadable += 1,
            }
        }
        s.median_rate = self
            .median_micros()
            .map(|us| self.block as f64 * 1e6 / us.max(1) as f64);
        let mut slowest: Option<(u64, u32)> = None;
        for (i, &t) in self.times.iter().enumerate() {
            if t != u32::MAX && slowest.is_none_or(|(_, s)| t > s) {
                slowest = Some((i as u64, t));
            }
        }
        s.slowest = slowest.map(|(i, t)| (self.block_range(i).0, u64::from(t)));
        // Unreadable and skipped blocks, joined into areas.
        let bad = |c: &Class| matches!(c, Class::Unreadable | Class::Skipped);
        let mut i = 0;
        while i < self.classes.len() {
            if !bad(&self.classes[i]) {
                i += 1;
                continue;
            }
            let from = i;
            while i < self.classes.len() && bad(&self.classes[i]) {
                i += 1;
            }
            s.damaged_areas += 1;
            if s.unreadable_ranges.len() < MAX_RANGES {
                let (at, _) = self.block_range(from as u64);
                let (last, len) = self.block_range(i as u64 - 1);
                s.unreadable_ranges.push((at, last + len - at));
            }
        }
        s
    }
}

/// Unreadable areas listed in a [`Summary`]; [`Summary::damaged_areas`]
/// counts them all.
const MAX_RANGES: usize = 100;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    /// Bytes in the range tested, and how many of them were read so far.
    pub total: u64,
    pub tested: u64,
    pub block: u64,
    /// Blocks in each class.
    pub fast: u64,
    pub slow: u64,
    pub very_slow: u64,
    pub unreadable: u64,
    pub skipped: u64,
    pub bad_sectors: u64,
    /// The drive's usual speed, in bytes a second.
    pub median_rate: Option<f64>,
    /// Where the slowest readable block is, and how many microseconds it
    /// took.
    pub slowest: Option<(u64, u64)>,
    /// `[offset, length]` of the first unreadable areas.
    pub unreadable_ranges: Vec<(u64, u64)>,
    pub damaged_areas: u64,
}

/// Tests the blocks of `map` not yet read, in order. `on_progress` is told
/// after every block (callers decide how often to show it). Returns
/// whether the test reached the end; when `cancel` is set it stops after
/// the current block, and `map` can be passed in again to continue.
pub fn run(
    src: &dyn Source,
    map: &mut SurfaceMap,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(&SurfaceMap),
) -> bool {
    let n = map.blocks();
    if map.times.len() != map.classes.len() {
        map.times = vec![u32::MAX; map.classes.len()];
    }
    let mut buf = vec![0u8; (map.block as usize).min(PIECE)];
    let mut bad_run = 0;
    while map.next < n {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let i = map.next;
        let (at, len) = map.block_range(i);
        let began = Instant::now();
        let mut bad = 0;
        let mut pos = 0;
        while pos < len {
            let want = buf.len().min((len - pos) as usize);
            let (got, b) = read_skipping(src, at + pos, &mut buf[..want]);
            // A source that ends early has lost the rest.
            bad += b + (want - got.min(want)).div_ceil(512) as u64;
            pos += want as u64;
        }
        let micros = began.elapsed().as_micros().min(u128::from(u32::MAX - 1)) as u32;
        map.next = i + 1;
        if bad > 0 {
            map.classes[i as usize] = Class::Unreadable;
            map.bad_sectors += bad;
            bad_run += 1;
            if bad_run >= DAMAGE_RUN {
                let resume = end_of_damage(src, map, i);
                for c in &mut map.classes[(i + 1) as usize..resume as usize] {
                    *c = Class::Skipped;
                }
                map.next = resume;
                bad_run = 0;
            }
        } else {
            bad_run = 0;
            map.record_time(i as usize, micros);
        }
        on_progress(map);
    }
    map.rejudge();
    on_progress(map);
    true
}

/// The first block after the damaged area holding block `bad` whose
/// start can be read: single-sector reads further and further ahead, then
/// halving back, so a damaged area costs a few dozen failed reads however
/// long it is.
fn end_of_damage(src: &dyn Source, map: &SurfaceMap, bad: u64) -> u64 {
    let n = map.blocks();
    let readable = |i: u64| {
        let mut sector = [0u8; ALIGN as usize];
        src.read_at(map.block_range(i).0, &mut sector).is_ok()
    };
    let (mut lo, mut stride) = (bad, 8u64);
    let max_stride = ((1u64 << 30) / map.block).max(1);
    let mut hi = loop {
        let at = lo.saturating_add(stride);
        if at >= n {
            break n;
        }
        if readable(at) {
            break at;
        }
        lo = at;
        stride = (stride * 2).min(max_stride);
    };
    // Damage starts at `lo` (or before) and is over by `hi`.
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if readable(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi.max(bad + 1)
}

/// Classes stored as a string of symbols: a map of a big drive has
/// millions of blocks, and a character each keeps it small.
mod symbols {
    use super::{Class, MAX_BLOCKS};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(classes: &[Class], s: S) -> Result<S::Ok, S::Error> {
        let text: String = classes.iter().map(|c| c.symbol()).collect();
        s.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Class>, D::Error> {
        let text = String::deserialize(d)?;
        if text.len() as u64 > MAX_BLOCKS {
            return Err(serde::de::Error::custom("too many blocks"));
        }
        text.chars()
            .map(|c| {
                Class::from_symbol(c).ok_or_else(|| serde::de::Error::custom("unknown block class"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    /// A drive with a slow area, a very slow area and unreadable areas.
    /// It holds no data: only how it reads matters here.
    #[derive(Default)]
    struct Patchy {
        len: u64,
        slow: Range<u64>,
        very_slow: Range<u64>,
        bad: Range<u64>,
        failed: AtomicU64,
        lowest: AtomicU64,
        highest: AtomicU64,
    }

    impl Patchy {
        fn new(len: u64) -> Self {
            Self {
                len,
                lowest: AtomicU64::new(u64::MAX),
                ..Default::default()
            }
        }
    }

    impl Source for Patchy {
        fn len(&self) -> u64 {
            self.len
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let end = offset + buf.len() as u64;
            self.lowest.fetch_min(offset, Ordering::Relaxed);
            self.highest.fetch_max(end, Ordering::Relaxed);
            let hits = |r: &Range<u64>| offset < r.end && r.start < end;
            if hits(&self.bad) {
                self.failed.fetch_add(1, Ordering::Relaxed);
                return Err(std::io::Error::other("unreadable"));
            }
            if hits(&self.very_slow) {
                std::thread::sleep(Duration::from_millis(700));
            } else if hits(&self.slow) {
                std::thread::sleep(Duration::from_millis(150));
            }
            let n = buf.len().min(self.len.saturating_sub(offset) as usize);
            buf[..n].fill(0);
            Ok(n)
        }
    }

    const KB64: u64 = 64 << 10;
    const MB: u64 = 1 << 20;

    fn opts() -> SurfaceOptions {
        SurfaceOptions {
            block: KB64,
            ..Default::default()
        }
    }

    #[test]
    fn finds_slow_and_unreadable_areas() {
        // Delays well past the floors, so a busy test machine can't blur
        // them; 1 MiB blocks, as on a real drive.
        let src = Patchy {
            slow: 20 * MB..24 * MB,
            very_slow: 40 * MB..42 * MB,
            bad: 52 * MB + 8192..52 * MB + 12288,
            ..Patchy::new(64 * MB)
        };
        let mut map = SurfaceMap::new(src.len(), &SurfaceOptions::default());
        assert_eq!(map.blocks(), 64);
        let mut updates = 0;
        assert!(run(&src, &mut map, &AtomicBool::new(false), |_| updates += 1));
        assert!(updates >= 64);
        let coarse = map.coarse(64);
        assert_eq!(&coarse[20..24], "ssss", "{coarse}");
        assert_eq!(&coarse[40..42], "SS", "{coarse}");
        assert_eq!(&coarse[52..53], "X", "{coarse}");
        let s = map.summary();
        assert_eq!((s.unreadable, s.skipped), (1, 0));
        // The rest reads normally (allowing a stall or two on a busy
        // machine).
        assert!(s.fast >= 55, "{coarse}");
        // The one bad 4K block, as 512-byte sectors.
        assert_eq!(s.bad_sectors, 8);
        assert_eq!(s.unreadable_ranges, vec![(52 * MB, MB)]);
        let slowest = s.slowest.map(|(at, _)| at / MB).unwrap();
        assert!((40..42).contains(&slowest));
        assert!(s.median_rate.is_some_and(|r| r > 0.0));
        assert_eq!(s.tested, 64 * MB);
        // Coarser maps show the worst of each cell.
        let quarter = map.coarse(16);
        assert_eq!(&quarter[5..6], "s");
        assert_eq!(&quarter[10..11], "S");
        assert_eq!(&quarter[13..14], "X");
    }

    #[test]
    fn skips_over_a_large_damaged_area() {
        let src = Patchy {
            bad: 2 * MB..50 * MB,
            ..Patchy::new(64 * MB)
        };
        let mut map = SurfaceMap::new(src.len(), &opts());
        assert!(run(&src, &mut map, &AtomicBool::new(false), |_| {}));
        // Every block in the area is unreadable or skipped, and the first
        // block after it is found exactly.
        let first_good = (50 * MB / KB64) as usize;
        for (i, c) in map.classes.iter().enumerate() {
            let in_area = (2 * MB / KB64) as usize <= i && i < first_good;
            assert_eq!(
                in_area,
                matches!(c, Class::Unreadable | Class::Skipped),
                "block {i}: {c:?}"
            );
        }
        assert_eq!(map.classes[first_good], Class::Fast);
        let s = map.summary();
        assert_eq!(s.damaged_areas, 1);
        assert_eq!(s.unreadable_ranges, vec![(2 * MB, 48 * MB)]);
        assert!(s.skipped > 600);
        // Not 768 blocks' worth of failed reads.
        let failed = src.failed.load(Ordering::Relaxed);
        assert!(failed < 200, "{failed} failed reads");
    }

    #[test]
    fn stops_and_continues() {
        let src = Patchy {
            bad: 3 * MB..3 * MB + 512,
            ..Patchy::new(8 * MB)
        };
        let mut whole = SurfaceMap::new(src.len(), &opts());
        run(&src, &mut whole, &AtomicBool::new(false), |_| {});

        let cancel = AtomicBool::new(false);
        let mut map = SurfaceMap::new(src.len(), &opts());
        let finished = run(&src, &mut map, &cancel, |m| {
            if m.next >= 40 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(!finished);
        assert_eq!(map.next, 40);
        assert!(!map.finished());
        assert_eq!(map.classes[60], Class::Untested);

        let saved = map.to_json();
        let mut again = SurfaceMap::from_json(&saved, src.len()).unwrap();
        assert!(run(&src, &mut again, &AtomicBool::new(false), |_| {}));
        // Read times differ run to run; where it could not read must not.
        let unreadable = |m: &SurfaceMap| -> Vec<bool> {
            m.classes.iter().map(|&c| c == Class::Unreadable).collect()
        };
        assert_eq!(unreadable(&again), unreadable(&whole));
        assert!(again.classes.iter().all(|&c| c != Class::Untested));
        assert_eq!(again.bad_sectors, whole.bad_sectors);
        // Its 4K block: devices are read a block at a time.
        assert_eq!(again.bad_sectors, 8);

        // Not on a drive of another size.
        assert!(SurfaceMap::from_json(&saved, src.len() + 4096).is_err());
    }

    #[test]
    fn reads_only_the_range_asked() {
        let src = Patchy::new(32 * MB);
        let o = SurfaceOptions {
            start: 5 * MB + 100,
            end: Some(9 * MB),
            block: KB64,
        };
        let mut map = SurfaceMap::new(src.len(), &o);
        assert_eq!(map.start, 5 * MB);
        assert!(run(&src, &mut map, &AtomicBool::new(false), |_| {}));
        assert_eq!(src.lowest.load(Ordering::Relaxed), 5 * MB);
        assert_eq!(src.highest.load(Ordering::Relaxed), 9 * MB);
        assert_eq!(map.summary().fast, 64);

        // A range past the end, or backwards, is empty, not a panic.
        for (start, end) in [(40 * MB, None), (9 * MB, Some(5 * MB)), (0, Some(0))] {
            let o = SurfaceOptions {
                start,
                end,
                block: KB64,
            };
            let mut m = SurfaceMap::new(src.len(), &o);
            assert_eq!(m.blocks(), 0);
            assert!(run(&src, &mut m, &AtomicBool::new(false), |_| {}));
            assert_eq!(m.coarse(10), "");
        }
    }

    #[test]
    fn sizes_blocks_to_fit() {
        // A 16 TB drive gets bigger blocks rather than a huge map.
        let m = SurfaceMap::new(16 << 40, &SurfaceOptions::default());
        assert!(m.blocks() <= MAX_BLOCKS);
        assert!(m.block.is_multiple_of(ALIGN));
        // Odd sizes asked for are rounded to whole sectors.
        let m = SurfaceMap::new(
            10 * MB,
            &SurfaceOptions {
                block: 5000,
                ..Default::default()
            },
        );
        assert_eq!(m.block, 8192);
        let m = SurfaceMap::new(
            10 * MB,
            &SurfaceOptions {
                block: u64::MAX,
                ..Default::default()
            },
        );
        assert_eq!(m.blocks(), 1);
    }

    #[test]
    fn refuses_damaged_maps() {
        let good = SurfaceMap::new(8 * MB, &opts()).to_json();
        assert!(SurfaceMap::from_json(&good, 8 * MB).is_ok());
        let v: serde_json::Value = serde_json::from_str(&good).unwrap();
        let edit = |k: &str, x: serde_json::Value| {
            let mut v = v.clone();
            v[k] = x;
            v.to_string()
        };
        for bad in [
            edit("block", 1000.into()),
            edit("block", 0.into()),
            edit("next", 10_000.into()),
            edit("end", (64 * MB).into()),
            edit("start", (9 * MB).into()),
            edit("classes", "..Z".into()),
            edit("classes", "...".into()),
            edit("hist", serde_json::json!([1, 2])),
            "{}".to_string(),
            "[".to_string(),
        ] {
            assert!(SurfaceMap::from_json(&bad, 8 * MB).is_err(), "{bad:.200}");
        }
    }
}

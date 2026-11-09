//! Copying a failing card to an image file, the way GNU ddrescue does.
//!
//! A dying card often gets worse with every read, and a sector that fails can
//! take seconds to give up. So the healthy areas are copied first in large
//! reads, skipping ahead past trouble. Only then does the imager go back to
//! the edges of each failed area one sector at a time ("trimming"), then to
//! what is left in the middle ("scraping"), and finally retry the sectors
//! that still failed. Progress is kept in a map file in ddrescue's format, so
//! an interrupted copy resumes where it stopped, and the map can be read by
//! ddrescue's own tools (ddrescuelog) or handed to ddrescue to continue.
//!
//! The source is only ever read. The image is written at the same offsets as
//! the source; areas that could not be read stay zero. It can be one file or
//! a set of numbered segments (`.001`, `.002`, ...) of a fixed size, for
//! destinations such as FAT32 that cannot hold large files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::source::Source;

/// What is known about a stretch of the source, with ddrescue's map letters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// `?`: not read yet.
    NonTried,
    /// `*`: a large read here failed; its edges have not been read singly.
    NonTrimmed,
    /// `/`: the middle of a failed area, between sectors that failed.
    NonScraped,
    /// `-`: a sector that failed when read on its own.
    BadSector,
    /// `+`: copied to the image.
    Finished,
}

impl Status {
    pub fn as_char(self) -> char {
        match self {
            Status::NonTried => '?',
            Status::NonTrimmed => '*',
            Status::NonScraped => '/',
            Status::BadSector => '-',
            Status::Finished => '+',
        }
    }

    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '?' => Status::NonTried,
            '*' => Status::NonTrimmed,
            '/' => Status::NonScraped,
            '-' => Status::BadSector,
            '+' => Status::Finished,
            _ => return None,
        })
    }
}

/// Which pass the imager is on; the map file's "current status".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Pass {
    Copy,
    Trim,
    Scrape,
    Retry,
    Finished,
}

impl Pass {
    pub fn as_char(self) -> char {
        match self {
            Pass::Copy => '?',
            Pass::Trim => '*',
            Pass::Scrape => '/',
            Pass::Retry => '-',
            Pass::Finished => '+',
        }
    }

    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            // ddrescue's fill ('F') and generate ('G') modes leave maps whose
            // blocks are still meaningful; carry on copying from them.
            '?' | 'F' | 'G' => Pass::Copy,
            '*' => Pass::Trim,
            '/' => Pass::Scrape,
            '-' => Pass::Retry,
            '+' => Pass::Finished,
            _ => return None,
        })
    }
}

/// A ddrescue map: the source divided into contiguous blocks, each with a
/// [`Status`], plus where the copy was and in which pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapfile {
    pub pos: u64,
    pub pass: Pass,
    /// The pass's repetition: copy sub-pass or retry number, counted from 1.
    pub pass_number: u32,
    len: u64,
    /// Block start -> (size, status). Adjacent blocks never share a status.
    blocks: BTreeMap<u64, (u64, Status)>,
    /// Bytes in each status, kept up to date so progress is cheap.
    totals: [u64; 5],
}

impl Mapfile {
    /// A map of a source of `len` bytes that has not been read at all.
    pub fn new(len: u64) -> Self {
        let mut map = Self {
            pos: 0,
            pass: Pass::Copy,
            pass_number: 1,
            len: 0,
            blocks: BTreeMap::new(),
            totals: [0; 5],
        };
        map.fit(len);
        map
    }

    /// The number of bytes the map covers.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes currently in `status`.
    pub fn total(&self, status: Status) -> u64 {
        self.totals[status as usize]
    }

    /// Every block as `(pos, size, status)`, in order.
    pub fn blocks(&self) -> impl Iterator<Item = (u64, u64, Status)> + '_ {
        self.blocks.iter().map(|(&pos, &(size, s))| (pos, size, s))
    }

    /// The `(pos, size)` of every block in `status`.
    pub fn ranges(&self, status: Status) -> Vec<(u64, u64)> {
        self.blocks()
            .filter(|b| b.2 == status)
            .map(|(pos, size, _)| (pos, size))
            .collect()
    }

    /// Marks `size` bytes at `pos` with `status`.
    pub fn set(&mut self, pos: u64, size: u64, status: Status) {
        let end = pos.saturating_add(size).min(self.len);
        if pos >= end {
            return;
        }
        self.split(pos);
        self.split(end);
        let inside: Vec<u64> = self.blocks.range(pos..end).map(|(&k, _)| k).collect();
        for k in inside {
            if let Some((size, s)) = self.blocks.remove(&k) {
                self.totals[s as usize] -= size;
            }
        }
        self.blocks.insert(pos, (end - pos, status));
        self.totals[status as usize] += end - pos;
        self.merge_next(pos);
        if let Some((&prev, _)) = self.blocks.range(..pos).next_back() {
            self.merge_next(prev);
        }
    }

    /// The first stretch in `status` at or after `from`, as `(start, end)`,
    /// starting no earlier than `from`.
    pub fn next(&self, status: Status, from: u64) -> Option<(u64, u64)> {
        if let Some((&s, &(size, st))) = self.blocks.range(..=from).next_back() {
            if st == status && s + size > from {
                return Some((from, s + size));
            }
        }
        self.blocks
            .range((Bound::Excluded(from), Bound::Unbounded))
            .find(|(_, &(_, st))| st == status)
            .map(|(&s, &(size, _))| (s, s + size))
    }

    /// The last stretch in `status` that starts before `before`, as
    /// `(start, end)`, ending no later than `before`.
    pub fn prev(&self, status: Status, before: u64) -> Option<(u64, u64)> {
        self.blocks
            .range(..before)
            .rev()
            .find(|(_, &(_, st))| st == status)
            .map(|(&s, &(size, _))| (s, (s + size).min(before)))
    }

    /// Splits the block containing `at` so that a block starts there.
    fn split(&mut self, at: u64) {
        if let Some((&s, &(size, st))) = self.blocks.range(..at).next_back() {
            if s + size > at {
                self.blocks.insert(s, (at - s, st));
                self.blocks.insert(at, (s + size - at, st));
            }
        }
    }

    /// Joins the block at `start` with the next one if they share a status.
    fn merge_next(&mut self, start: u64) {
        let Some(&(size, st)) = self.blocks.get(&start) else {
            return;
        };
        if let Some(&(next, next_st)) = self.blocks.get(&(start + size)) {
            if next_st == st {
                self.blocks.remove(&(start + size));
                self.blocks.insert(start, (size + next, st));
            }
        }
    }

    /// Makes the map cover exactly `len` bytes by adding an unread tail.
    /// ddrescue maps of a partial copy can stop short of the device's end.
    fn fit(&mut self, len: u64) {
        if len > self.len {
            let tail = self.len;
            self.blocks.insert(tail, (len - tail, Status::NonTried));
            self.totals[Status::NonTried as usize] += len - tail;
            self.len = len;
            if let Some((&prev, _)) = self.blocks.range(..tail).next_back() {
                self.merge_next(prev);
            }
        }
    }

    /// Parses a GNU ddrescue map file.
    pub fn parse(text: &str) -> io::Result<Self> {
        let invalid = |line: usize, what: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("map file line {line}: {what}"),
            )
        };
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(n, l)| (n + 1, l.trim()))
            .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'));
        let Some((n, first)) = lines.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the map file is empty",
            ));
        };
        let f: Vec<&str> = first.split_whitespace().collect();
        if !(2..=3).contains(&f.len()) {
            return Err(invalid(n, "expected the current position and status"));
        }
        let pos = number(f[0]).ok_or_else(|| invalid(n, "bad position"))?;
        let pass = single_char(f[1])
            .and_then(Pass::from_char)
            .ok_or_else(|| invalid(n, "unknown status"))?;
        let pass_number = match f.get(2) {
            Some(p) => number(p)
                .and_then(|p| u32::try_from(p).ok())
                .ok_or_else(|| invalid(n, "bad pass number"))?,
            None => 1,
        };

        let mut map = Self {
            pos,
            pass,
            pass_number: pass_number.max(1),
            len: 0,
            blocks: BTreeMap::new(),
            totals: [0; 5],
        };
        let mut last: Option<u64> = None;
        for (n, line) in lines {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() != 3 {
                return Err(invalid(n, "expected position, size and status"));
            }
            let start = number(f[0]).ok_or_else(|| invalid(n, "bad position"))?;
            let size = number(f[1]).ok_or_else(|| invalid(n, "bad size"))?;
            let status = single_char(f[2])
                .and_then(Status::from_char)
                .ok_or_else(|| invalid(n, "unknown status"))?;
            if start != map.len {
                return Err(invalid(n, "block does not follow the one before"));
            }
            let end = start
                .checked_add(size)
                .ok_or_else(|| invalid(n, "block runs past the largest size"))?;
            if size == 0 {
                continue;
            }
            map.totals[status as usize] += size;
            map.len = end;
            match last.and_then(|l| map.blocks.get_mut(&l)) {
                Some((prev_size, prev)) if *prev == status => *prev_size += size,
                _ => {
                    map.blocks.insert(start, (size, status));
                    last = Some(start);
                }
            }
        }
        Ok(map)
    }

    /// Writes the map in GNU ddrescue's format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# Mapfile. Created by Reperio {}",
            env!("CARGO_PKG_VERSION")
        );
        let _ = writeln!(out, "# current_pos  current_status  current_pass");
        let _ = writeln!(
            out,
            "{:#010X}     {}               {}",
            self.pos,
            self.pass.as_char(),
            self.pass_number
        );
        let _ = writeln!(out, "#      pos        size  status");
        for (pos, size, status) in self.blocks() {
            let _ = writeln!(out, "{pos:#010X}  {size:#010X}  {}", status.as_char());
        }
        out
    }

    /// Loads a map file for a source of `len` bytes.
    pub fn load(path: &Path, len: u64) -> io::Result<Self> {
        let mut map = Self::parse(&std::fs::read_to_string(path)?)?;
        if map.len > len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} describes a larger disk ({} bytes) than this one ({len} bytes)",
                    path.display(),
                    map.len
                ),
            ));
        }
        map.fit(len);
        Ok(map)
    }

    /// Saves the map so that a crash midway leaves the old map or the new
    /// one, never half of each.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = File::create(&tmp)?;
        f.write_all(self.render().as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    }
}

/// Parses a number as ddrescue writes it: `0x`-prefixed hex or decimal.
fn number(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

#[derive(Debug, Clone)]
pub struct ImageOptions {
    /// Read size for the first pass. Large reads keep a healthy card fast.
    pub cluster: u64,
    /// The unit that trimming, scraping and retrying read in.
    pub sector: u64,
    /// Read the middle of failed areas sector by sector. Turning this off
    /// gets the easy data out sooner; a later run can scrape.
    pub scrape: bool,
    /// How many times to go back over sectors that still failed.
    pub retries: u32,
    /// Retry from the end towards the start. Some drives read a stubborn
    /// sector when approached from the other side.
    pub reverse_retries: bool,
    /// How often the map is saved during a long copy.
    pub save_every: Duration,
    /// Write the image as numbered segments of this many bytes
    /// (`OUTPUT.001`, `OUTPUT.002`, ...) instead of one file.
    pub split: Option<u64>,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            cluster: 1 << 20,
            sector: 512,
            scrape: true,
            retries: 0,
            reverse_retries: false,
            save_every: Duration::from_secs(10),
            split: None,
        }
    }
}

/// Where a copy stands, for showing progress.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ImageProgress {
    pub pass: Pass,
    pub pass_number: u32,
    /// Where on the source the imager is reading.
    pub pos: u64,
    pub total: u64,
    /// Bytes copied to the image.
    pub finished: u64,
    /// Bytes in sectors that failed when read on their own.
    pub bad: u64,
    /// Bytes not read yet.
    pub non_tried: u64,
    /// Bytes in failed areas not yet trimmed or scraped.
    pub pending: u64,
    /// Bytes read per second, over the last second or so.
    pub rate: f64,
    /// Failed reads so far in this run.
    pub errors: u64,
}

impl ImageProgress {
    /// Bytes whose fate is settled: copied, or failed when read singly.
    pub fn done(&self) -> u64 {
        self.finished + self.bad
    }
}

#[derive(Debug, Clone)]
pub struct ImageOutcome {
    pub cancelled: bool,
    pub finished: u64,
    pub bad: u64,
    /// Bytes neither copied nor found bad: left for a later run, after a
    /// cancel or when scraping was turned off.
    pub unread: u64,
    pub errors: u64,
    /// `(pos, size)` of each unreadable area.
    pub bad_ranges: Vec<(u64, u64)>,
    /// `(pos, size)` of each area not read yet (see `unread`).
    pub unread_ranges: Vec<(u64, u64)>,
    /// What each pass of this run did, in order.
    pub passes: Vec<PassRecord>,
    /// Whether this run carried on from an existing map.
    pub resumed: bool,
}

/// One pass of a run, for an acquisition log.
#[derive(Debug, Clone)]
pub struct PassRecord {
    pub pass: Pass,
    pub number: u32,
    pub started: SystemTime,
    pub ended: SystemTime,
    /// Bytes copied to the image during the pass.
    pub copied: u64,
    /// Failed reads during the pass.
    pub errors: u64,
    /// Bytes in unreadable sectors when the pass ended.
    pub bad_after: u64,
}

/// The files an image of `len` bytes is written to: `dest` itself, or with
/// `split`, `dest.001`, `dest.002`, ... each `split` bytes but the last.
pub fn segment_paths(dest: &Path, len: u64, split: Option<u64>) -> Vec<PathBuf> {
    match split {
        None => vec![dest.to_path_buf()],
        Some(size) => {
            let count = len.div_ceil(size.max(1)).max(1);
            // Three digits as forensic tools expect, more only if needed.
            let width = count.to_string().len().max(3);
            (1..=count)
                .map(|n| {
                    let mut p = dest.as_os_str().to_owned();
                    p.push(format!(".{n:0width$}"));
                    PathBuf::from(p)
                })
                .collect()
        }
    }
}

/// The image being written: one file, or segments of `seg` bytes each.
struct Dest {
    files: Vec<File>,
    seg: u64,
}

impl Dest {
    fn open(dest: &Path, len: u64, split: Option<u64>, resuming: bool) -> io::Result<Self> {
        let paths = segment_paths(dest, len, split);
        let seg = split.map_or(u64::MAX, |s| s.max(1));
        let mut files = Vec::new();
        for (n, path) in paths.iter().enumerate() {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            // Pre-size the image so it is sparse and never-read areas read
            // as zero. A destination that is itself a disk is left as it is.
            if file.metadata()?.is_file() {
                let start = n as u64 * seg.min(len);
                let size = if split.is_some() {
                    (len - start.min(len)).min(seg)
                } else {
                    len
                };
                if !resuming {
                    file.set_len(0)?;
                }
                if file.metadata()?.len() < size {
                    file.set_len(size)?;
                }
            }
            files.push(file);
        }
        Ok(Self { files, seg })
    }

    fn write_at(&mut self, mut pos: u64, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = (pos / self.seg) as usize;
            let within = pos % self.seg;
            let take = (data.len() as u64).min(self.seg - within) as usize;
            let file = self
                .files
                .get_mut(n)
                .ok_or_else(|| io::Error::other("write past the end of the last image segment"))?;
            file.seek(SeekFrom::Start(within))?;
            file.write_all(&data[..take])?;
            pos += take as u64;
            data = &data[take..];
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        for f in &self.files {
            f.sync_data()?;
        }
        Ok(())
    }
}

/// Copies `src` into the image at `dest`, keeping track in `map_path`.
///
/// If the map file exists, the copy resumes from it and `dest` must be the
/// image it describes; otherwise the copy starts afresh and `dest` is
/// emptied. With [`ImageOptions::split`], `dest` names the segments (see
/// [`segment_paths`]). Set `cancel` to stop at the next read; the map is saved either
/// way, so running again with the same paths carries on.
pub fn image(
    src: &dyn Source,
    dest: &Path,
    map_path: &Path,
    opts: &ImageOptions,
    cancel: &AtomicBool,
    on_progress: impl FnMut(&ImageProgress),
) -> io::Result<ImageOutcome> {
    let len = src.len();
    let resuming = map_path.exists();
    let map = if resuming {
        Mapfile::load(map_path, len)?
    } else {
        Mapfile::new(len)
    };
    let file = Dest::open(dest, len, opts.split, resuming)?;

    let sector = opts.sector.max(1);
    let cluster = opts.cluster.max(sector) / sector * sector;
    let now = Instant::now();
    let mut run = Run {
        src,
        file,
        map,
        map_path,
        opts,
        sector,
        cluster,
        cancel,
        on_progress,
        errors: 0,
        read: 0,
        rate: 0.0,
        rate_mark: (now, 0),
        last_emit: now,
        last_save: now,
        buf: vec![0; cluster as usize],
        passes: Vec::new(),
        current: None,
        mark: (0, 0),
    };
    run.mark = (run.map.total(Status::Finished), 0);
    run.emit();
    let result = run.all();
    run.close_pass();
    // Leave a map that matches the image even after an error.
    let saved = run.save();
    let cancelled = result?;
    saved?;
    run.emit();
    let map = &run.map;
    Ok(ImageOutcome {
        cancelled,
        finished: map.total(Status::Finished),
        bad: map.total(Status::BadSector),
        unread: map.len() - map.total(Status::Finished) - map.total(Status::BadSector),
        errors: run.errors,
        bad_ranges: map.ranges(Status::BadSector),
        unread_ranges: map
            .blocks()
            .filter(|b| !matches!(b.2, Status::Finished | Status::BadSector))
            .map(|(pos, size, _)| (pos, size))
            .collect(),
        passes: std::mem::take(&mut run.passes),
        resumed: resuming,
    })
}

/// Smallest and largest jump past a failed read in the first pass. The jump
/// doubles with each failure in a row, as in ddrescue, so a dead stretch is
/// crossed in a few reads instead of thousands.
const MIN_SKIP: u64 = 64 << 10;
const MAX_SKIP: u64 = 1 << 30;

struct Run<'a, F> {
    src: &'a dyn Source,
    file: Dest,
    map: Mapfile,
    map_path: &'a Path,
    opts: &'a ImageOptions,
    sector: u64,
    cluster: u64,
    cancel: &'a AtomicBool,
    on_progress: F,
    errors: u64,
    /// Bytes read successfully in this run, for the rate.
    read: u64,
    rate: f64,
    rate_mark: (Instant, u64),
    last_emit: Instant,
    last_save: Instant,
    buf: Vec<u8>,
    passes: Vec<PassRecord>,
    /// The pass under way.
    current: Option<PassRecord>,
    /// Bytes copied and failed reads when the last pass ended.
    mark: (u64, u64),
}

impl<F: FnMut(&ImageProgress)> Run<'_, F> {
    /// Runs every pass that still has work. Returns true if cancelled.
    fn all(&mut self) -> io::Result<bool> {
        let (pass, number, pos) = (self.map.pass, self.map.pass_number, self.map.pos);
        if self.map.total(Status::NonTried) > 0 {
            // A forward pass that skips past trouble, then one that reads
            // whatever was skipped.
            let resume_sweep = pass == Pass::Copy && number >= 2;
            if !resume_sweep {
                let from = if pass == Pass::Copy { pos } else { 0 };
                if self.copy_pass(from, true)? {
                    return Ok(true);
                }
            }
            if self.copy_pass(0, false)? {
                return Ok(true);
            }
        }
        if self.map.total(Status::NonTrimmed) > 0 && self.trim()? {
            return Ok(true);
        }
        if self.opts.scrape && self.map.total(Status::NonScraped) > 0 && self.scrape()? {
            return Ok(true);
        }
        let first = if pass == Pass::Retry { number } else { 1 };
        for n in first..=self.opts.retries {
            if self.map.total(Status::BadSector) == 0 {
                break;
            }
            if self.retry(n)? {
                return Ok(true);
            }
        }
        self.map.pass = Pass::Finished;
        self.map.pass_number = 1;
        Ok(false)
    }

    fn copy_pass(&mut self, from: u64, skipping: bool) -> io::Result<bool> {
        let number = if skipping { 1 } else { 2 };
        self.begin(Pass::Copy, number);
        let mut skip = MIN_SKIP;
        // At most 1% of the source, in whole steps so reads stay aligned.
        let max_skip = (self.map.len() / 100).clamp(MIN_SKIP, MAX_SKIP) / MIN_SKIP * MIN_SKIP;
        let mut pos = from;
        while let Some((start, end)) = self.map.next(Status::NonTried, pos) {
            pos = start;
            while pos < end {
                // End each read on a cluster boundary so reads stay aligned.
                let want = (self.cluster - pos % self.cluster).min(end - pos);
                let ok = self.copy(pos, want, Status::NonTrimmed)?;
                pos += want;
                if ok {
                    skip = MIN_SKIP;
                } else if skipping {
                    pos = pos.saturating_add(skip);
                    skip = (skip * 2).min(max_skip);
                }
                if self.tick(Pass::Copy, number, pos.min(self.map.len()))? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Reads each failed area's edges sector by sector until a sector
    /// fails, from the front and then from the back.
    fn trim(&mut self) -> io::Result<bool> {
        self.begin(Pass::Trim, 1);
        let mut pos = 0;
        while let Some((start, end)) = self.map.next(Status::NonTrimmed, pos) {
            let (mut lo, mut hi) = (start, end);
            while lo < hi {
                let n = (self.sector - lo % self.sector).min(hi - lo);
                let ok = self.copy(lo, n, Status::BadSector)?;
                lo += n;
                if self.tick(Pass::Trim, 1, lo)? {
                    return Ok(true);
                }
                if !ok {
                    break;
                }
            }
            while lo < hi {
                let s = ((hi - 1) / self.sector * self.sector).max(lo);
                let ok = self.copy(s, hi - s, Status::BadSector)?;
                hi = s;
                if self.tick(Pass::Trim, 1, hi)? {
                    return Ok(true);
                }
                if !ok {
                    break;
                }
            }
            self.map.set(lo, hi - lo, Status::NonScraped);
            pos = end;
        }
        Ok(false)
    }

    fn scrape(&mut self) -> io::Result<bool> {
        self.begin(Pass::Scrape, 1);
        let mut pos = 0;
        while let Some((start, end)) = self.map.next(Status::NonScraped, pos) {
            pos = start;
            while pos < end {
                let n = (self.sector - pos % self.sector).min(end - pos);
                self.copy(pos, n, Status::BadSector)?;
                pos += n;
                if self.tick(Pass::Scrape, 1, pos)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn retry(&mut self, number: u32) -> io::Result<bool> {
        self.begin(Pass::Retry, number);
        if self.opts.reverse_retries {
            let mut before = self.map.len();
            while let Some((start, end)) = self.map.prev(Status::BadSector, before) {
                let mut hi = end;
                while hi > start {
                    let s = ((hi - 1) / self.sector * self.sector).max(start);
                    self.copy(s, hi - s, Status::BadSector)?;
                    hi = s;
                    if self.tick(Pass::Retry, number, hi)? {
                        return Ok(true);
                    }
                }
                before = start;
            }
        } else {
            let mut pos = 0;
            while let Some((start, end)) = self.map.next(Status::BadSector, pos) {
                pos = start;
                while pos < end {
                    let n = (self.sector - pos % self.sector).min(end - pos);
                    self.copy(pos, n, Status::BadSector)?;
                    pos += n;
                    if self.tick(Pass::Retry, number, pos)? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Reads `len` bytes at `pos` into the image. On failure the area is
    /// marked `on_fail`. Returns whether the whole read succeeded.
    fn copy(&mut self, pos: u64, len: u64, on_fail: Status) -> io::Result<bool> {
        let buf = &mut self.buf[..len as usize];
        // A short read before the end of the source is a failure too; the
        // part that did arrive is kept.
        let got = match self.src.read_at(pos, buf) {
            Ok(n) => n.min(buf.len()),
            Err(_) => 0,
        };
        if got > 0 {
            self.file.write_at(pos, &self.buf[..got])?;
            self.map.set(pos, got as u64, Status::Finished);
            self.read += got as u64;
        }
        if got as u64 == len {
            return Ok(true);
        }
        self.map.set(pos + got as u64, len - got as u64, on_fail);
        self.errors += 1;
        Ok(false)
    }

    /// Records where the copy is, shows progress now and then, saves the
    /// map now and then. Returns true if the copy should stop.
    fn tick(&mut self, pass: Pass, number: u32, pos: u64) -> io::Result<bool> {
        self.map.pass = pass;
        self.map.pass_number = number;
        self.map.pos = pos;
        let now = Instant::now();
        let dt = now.duration_since(self.rate_mark.0);
        if dt >= Duration::from_secs(1) {
            self.rate = (self.read - self.rate_mark.1) as f64 / dt.as_secs_f64();
            self.rate_mark = (now, self.read);
        }
        if now.duration_since(self.last_emit) >= Duration::from_millis(100) {
            self.emit();
        }
        if now.duration_since(self.last_save) >= self.opts.save_every {
            self.save()?;
        }
        Ok(self.cancel.load(Ordering::Relaxed))
    }

    /// Starts logging a pass, ending the one before.
    fn begin(&mut self, pass: Pass, number: u32) {
        self.close_pass();
        let now = SystemTime::now();
        self.current = Some(PassRecord {
            pass,
            number,
            started: now,
            ended: now,
            copied: 0,
            errors: 0,
            bad_after: 0,
        });
    }

    /// Ends the pass under way, if any, and logs what it did.
    fn close_pass(&mut self) {
        let (finished, errors) = (self.map.total(Status::Finished), self.errors);
        if let Some(mut p) = self.current.take() {
            p.ended = SystemTime::now();
            p.copied = finished - self.mark.0;
            p.errors = errors - self.mark.1;
            p.bad_after = self.map.total(Status::BadSector);
            self.passes.push(p);
        }
        self.mark = (finished, errors);
    }

    fn emit(&mut self) {
        self.last_emit = Instant::now();
        let m = &self.map;
        let p = ImageProgress {
            pass: m.pass,
            pass_number: m.pass_number,
            pos: m.pos,
            total: m.len(),
            finished: m.total(Status::Finished),
            bad: m.total(Status::BadSector),
            non_tried: m.total(Status::NonTried),
            pending: m.total(Status::NonTrimmed) + m.total(Status::NonScraped),
            rate: self.rate,
            errors: self.errors,
        };
        (self.on_progress)(&p);
    }

    /// Saves the map, after making sure the image holds everything the map
    /// says was copied.
    fn save(&mut self) -> io::Result<()> {
        self.last_save = Instant::now();
        self.file.sync()?;
        self.map.save(self.map_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// A card that fails reads touching chosen byte ranges, optionally only
    /// the first few times, slowly, or stopping the copy after some reads.
    struct FakeCard {
        data: Vec<u8>,
        bad: Vec<(u64, u64)>,
        /// Sector offset -> failures left.
        flaky: Mutex<HashMap<u64, u32>>,
        delay: Duration,
        reads: AtomicUsize,
        offsets: Mutex<Vec<(u64, usize)>>,
        stop_after: Option<(usize, Arc<AtomicBool>)>,
    }

    impl FakeCard {
        fn new(data: Vec<u8>, bad: &[(u64, u64)]) -> Self {
            Self {
                data,
                bad: bad.to_vec(),
                flaky: Mutex::new(HashMap::new()),
                delay: Duration::ZERO,
                reads: AtomicUsize::new(0),
                offsets: Mutex::new(Vec::new()),
                stop_after: None,
            }
        }
    }

    impl Source for FakeCard {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some((after, flag)) = &self.stop_after {
                if n.is_multiple_of(*after) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            self.offsets.lock().unwrap().push((offset, buf.len()));
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let end = (offset + buf.len() as u64).min(self.len());
            if offset >= end {
                return Ok(0);
            }
            if self.bad.iter().any(|&(s, e)| s < end && offset < e) {
                return Err(io::Error::other("bad sector"));
            }
            let mut flaky = self.flaky.lock().unwrap();
            let mut failed = false;
            for (&s, left) in flaky.iter_mut() {
                if s < end && offset < s + 512 && *left > 0 {
                    *left -= 1;
                    failed = true;
                }
            }
            if failed {
                return Err(io::Error::other("flaky sector"));
            }
            let n = (end - offset) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(n)
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8 | 1)
            .collect()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reperio-img-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn small_opts() -> ImageOptions {
        ImageOptions {
            cluster: 64 << 10,
            ..ImageOptions::default()
        }
    }

    /// What the image should hold: the source with bad ranges zeroed.
    fn expected(data: &[u8], bad: &[(u64, u64)]) -> Vec<u8> {
        let mut want = data.to_vec();
        for &(s, e) in bad {
            want[s as usize..e as usize].fill(0);
        }
        want
    }

    fn as_sizes(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
        ranges.iter().map(|&(s, e)| (s, e - s)).collect()
    }

    /// Ends in a short sector, as an odd-sized image can.
    const LEN: usize = (3 << 20) + 1536 + 100;
    /// Bad byte ranges: a run of sectors, a lone sector, one spanning a
    /// cluster boundary, and the last full sector with the short one.
    const BAD: [(u64, u64); 4] = [
        (1_000_448, 1_000_448 + 3 * 512),
        (2 << 20, (2 << 20) + 512),
        ((1 << 20) - 1024, (1 << 20) + 2048),
        (LEN as u64 - 612, LEN as u64),
    ];

    fn bad_sorted() -> Vec<(u64, u64)> {
        let mut b = BAD.to_vec();
        b.sort();
        b
    }

    #[test]
    fn copies_good_data_and_records_bad_ranges() {
        let dir = tmp_dir("copy");
        let data = pattern(LEN);
        let card = FakeCard::new(data.clone(), &BAD);
        let (img, map) = (dir.join("card.img"), dir.join("card.img.map"));
        let cancel = AtomicBool::new(false);
        let mut passes = Vec::new();
        let out = image(&card, &img, &map, &small_opts(), &cancel, |p| {
            if passes.last() != Some(&p.pass) {
                passes.push(p.pass);
            }
        })
        .unwrap();

        assert!(!out.cancelled);
        assert_eq!(std::fs::read(&img).unwrap(), expected(&data, &BAD));
        assert_eq!(out.bad_ranges, as_sizes(&bad_sorted()));
        let bad_bytes: u64 = BAD.iter().map(|(s, e)| e - s).sum();
        assert_eq!(
            (out.bad, out.finished, out.unread),
            (bad_bytes, LEN as u64 - bad_bytes, 0)
        );
        assert_eq!(passes.first(), Some(&Pass::Copy));
        assert_eq!(passes.last(), Some(&Pass::Finished));

        let saved = Mapfile::load(&map, LEN as u64).unwrap();
        assert_eq!(saved.pass, Pass::Finished);
        assert_eq!(saved.ranges(Status::BadSector), out.bad_ranges);
        assert_eq!(saved.total(Status::Finished), out.finished);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A healthy area right after a bad one is copied in the first pass,
    /// before any sector-by-sector work, and the dead stretch is crossed in
    /// a few large reads.
    #[test]
    fn first_pass_skips_past_errors() {
        let dir = tmp_dir("skip");
        let len = 16 << 20;
        let data = pattern(len);
        let dead = (4 << 20, 8 << 20);
        let card = FakeCard::new(data.clone(), &[dead]);
        let (img, map) = (dir.join("a.img"), dir.join("a.map"));
        let cancel = AtomicBool::new(false);
        let mut after_copy = None;
        image(&card, &img, &map, &small_opts(), &cancel, |p| {
            if p.pass != Pass::Copy && after_copy.is_none() {
                after_copy = Some(*p);
            }
        })
        .unwrap();
        let p = after_copy.unwrap();
        assert_eq!(p.finished, (len - (4 << 20)) as u64);
        assert_eq!(p.non_tried, 0);
        // The skipping pass reads far fewer clusters than the 64 the dead
        // stretch holds; the sweep then reads each skipped one once.
        let failed_large = card
            .offsets
            .lock()
            .unwrap()
            .iter()
            .filter(|&&(o, l)| l > 512 && o >= dead.0 && o < dead.1)
            .count();
        assert!(
            failed_large <= 64 + 8,
            "{failed_large} large reads in the dead area"
        );
        assert_eq!(std::fs::read(&img).unwrap(), expected(&data, &[dead]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn map_round_trips() {
        let mut map = Mapfile::new(10_000);
        map.set(512, 1024, Status::Finished);
        map.set(4096, 512, Status::BadSector);
        map.set(5000, 100, Status::NonScraped);
        map.set(9000, 1000, Status::NonTrimmed);
        map.set(1536, 100, Status::Finished); // joins the block before
        map.pos = 4608;
        map.pass = Pass::Scrape;
        let text = map.render();
        assert!(
            text.contains("\n0x00001200     /               1\n"),
            "{text}"
        );
        assert!(text.contains("\n0x00000200  0x00000464  +\n"), "{text}");
        let back = Mapfile::parse(&text).unwrap();
        assert_eq!(back, map);
        assert_eq!(back.total(Status::Finished), 1124);
        assert_eq!(
            back.blocks().map(|b| b.0 + b.1).last(),
            Some(10_000),
            "blocks cover the whole source"
        );
    }

    #[test]
    fn parses_ddrescue_maps() {
        let text = "# Mapfile. Created by GNU ddrescue version 1.27\n\
                    # Command line: ddrescue /dev/sdb card.img card.map\n\
                    # current_pos  current_status  current_pass\n\
                    0x00010000     *               1\n\
                    #      pos        size  status\n\
                    0x00000000  0x00010000  +\r\n\
                    65536  4096  *\n\
                    0x00011000  0x00001000  +\n";
        let map = Mapfile::parse(text).unwrap();
        assert_eq!(
            (map.pos, map.pass, map.len()),
            (0x10000, Pass::Trim, 0x12000)
        );
        assert_eq!(map.ranges(Status::NonTrimmed), vec![(65536, 4096)]);
        // Older ddrescue leaves out the pass number.
        let old = Mapfile::parse("0 ?\n0 100 ?\n").unwrap();
        assert_eq!((old.pass_number, old.len()), (1, 100));

        for broken in ["", "0 ?\n0 10 +\n20 10 +\n", "0 x\n", "0 ?\n0 10 q\n"] {
            assert!(Mapfile::parse(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn refuses_a_map_of_a_larger_disk() {
        let dir = tmp_dir("larger");
        let map = dir.join("m");
        Mapfile::new(2048).save(&map).unwrap();
        let card = FakeCard::new(pattern(1024), &[]);
        let cancel = AtomicBool::new(false);
        let err = image(&card, &dir.join("i"), &map, &small_opts(), &cancel, |_| {});
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Stopping at many points through every pass and resuming each time
    /// ends with the same image and map as one uninterrupted copy.
    #[test]
    fn cancelled_copies_resume_to_the_same_result() {
        let dir = tmp_dir("resume");
        let data = pattern(LEN);
        for every in [3, 17, 200] {
            let flag = Arc::new(AtomicBool::new(false));
            let mut card = FakeCard::new(data.clone(), &BAD);
            card.stop_after = Some((every, flag.clone()));
            let (img, map) = (dir.join("r.img"), dir.join("r.map"));
            let _ = std::fs::remove_file(&img);
            let _ = std::fs::remove_file(&map);
            let mut runs = 0;
            let out = loop {
                runs += 1;
                assert!(runs < 10_000);
                flag.store(false, Ordering::Relaxed);
                let out = image(&card, &img, &map, &small_opts(), &flag, |_| {}).unwrap();
                // Every stop leaves a map that parses and covers the source.
                let saved = Mapfile::load(&map, LEN as u64).unwrap();
                assert_eq!(saved.blocks().map(|b| b.1).sum::<u64>(), LEN as u64);
                if !out.cancelled {
                    break out;
                }
                assert_ne!(saved.pass, Pass::Finished);
            };
            assert!(runs > 2, "stopping every {every} reads should interrupt");
            assert_eq!(out.bad_ranges, as_sizes(&bad_sorted()));
            assert_eq!(std::fs::read(&img).unwrap(), expected(&data, &BAD));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A slow card stopped from another thread leaves a map to resume from.
    #[test]
    fn stops_from_another_thread() {
        let dir = tmp_dir("thread");
        let data = pattern(1 << 20);
        let mut card = FakeCard::new(data.clone(), &[(300_000, 300_100)]);
        card.delay = Duration::from_millis(2);
        let (img, map) = (dir.join("t.img"), dir.join("t.map"));
        let cancel = AtomicBool::new(false);
        let opts = ImageOptions {
            cluster: 4096,
            ..ImageOptions::default()
        };
        let out = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(60));
                cancel.store(true, Ordering::Relaxed);
            });
            image(&card, &img, &map, &opts, &cancel, |_| {}).unwrap()
        });
        assert!(out.cancelled);
        let saved = Mapfile::load(&map, 1 << 20).unwrap();
        assert_eq!(saved.pass, Pass::Copy);
        assert!(saved.total(Status::NonTried) > 0);
        assert_eq!(saved.total(Status::Finished), out.finished);

        card.delay = Duration::ZERO;
        cancel.store(false, Ordering::Relaxed);
        let reads_before = card.offsets.lock().unwrap().len();
        let out = image(&card, &img, &map, &opts, &cancel, |_| {}).unwrap();
        assert!(!out.cancelled);
        assert_eq!(out.bad_ranges, vec![(299_520, 1024)]);
        assert_eq!(
            std::fs::read(&img).unwrap(),
            expected(&data, &[(299_520, 300_544)])
        );
        // The resumed run did not read again what was already copied.
        let copied = saved.ranges(Status::Finished);
        let reread = card.offsets.lock().unwrap()[reads_before..]
            .iter()
            .any(|&(o, _)| copied.iter().any(|&(p, size)| p <= o && o < p + size));
        assert!(!reread);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Sectors that fail a few times then read are rescued by retries, in
    /// either direction.
    #[test]
    fn retries_rescue_intermittent_sectors() {
        let data = pattern(512 << 10);
        let flaky_at = [10_240u64, 10_752, 200_704, 524_288 - 512];
        for (reverse, retries, expect_bad) in
            [(false, 0, true), (false, 3, false), (true, 3, false)]
        {
            let dir = tmp_dir(&format!("flaky-{reverse}-{retries}"));
            let card = FakeCard::new(data.clone(), &[]);
            card.flaky
                .lock()
                .unwrap()
                .extend(flaky_at.iter().map(|&s| (s, 3)));
            let opts = ImageOptions {
                retries,
                reverse_retries: reverse,
                ..small_opts()
            };
            let (img, map) = (dir.join("f.img"), dir.join("f.map"));
            let cancel = AtomicBool::new(false);
            let out = image(&card, &img, &map, &opts, &cancel, |_| {}).unwrap();
            if expect_bad {
                assert!(out.bad > 0);
            } else {
                assert_eq!((out.bad, out.finished), (0, data.len() as u64));
                assert_eq!(std::fs::read(&img).unwrap(), data);
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Without scraping, the middles of failed areas are left for a later
    /// run, which finishes them.
    #[test]
    fn scraping_can_wait_for_a_later_run() {
        let dir = tmp_dir("noscrape");
        let data = pattern(LEN);
        let card = FakeCard::new(data.clone(), &BAD);
        let (img, map) = (dir.join("n.img"), dir.join("n.map"));
        let cancel = AtomicBool::new(false);
        let opts = ImageOptions {
            scrape: false,
            ..small_opts()
        };
        let out = image(&card, &img, &map, &opts, &cancel, |_| {}).unwrap();
        assert!(out.unread > 0);
        assert!(
            Mapfile::load(&map, LEN as u64)
                .unwrap()
                .total(Status::NonScraped)
                > 0
        );
        let out = image(&card, &img, &map, &small_opts(), &cancel, |_| {}).unwrap();
        assert_eq!(out.unread, 0);
        assert_eq!(out.bad_ranges, as_sizes(&bad_sorted()));
        assert_eq!(std::fs::read(&img).unwrap(), expected(&data, &BAD));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A map saying part of the source is already copied (from ddrescue or
    /// an earlier run) keeps the imager away from that part.
    #[test]
    fn resumes_from_a_written_map_without_rereading() {
        let dir = tmp_dir("given");
        let data = pattern(256 << 10);
        let card = FakeCard::new(data.clone(), &[]);
        let (img, map) = (dir.join("g.img"), dir.join("g.map"));
        let mut half = data.clone();
        half[128 << 10..].fill(0);
        std::fs::write(&img, &half).unwrap();
        std::fs::write(&map, "0x00020000 ? 1\n0x0 0x20000 +\n0x20000 0x20000 ?\n").unwrap();
        let cancel = AtomicBool::new(false);
        image(&card, &img, &map, &small_opts(), &cancel, |_| {}).unwrap();
        assert!(card
            .offsets
            .lock()
            .unwrap()
            .iter()
            .all(|&(o, _)| o >= 128 << 10));
        assert_eq!(std::fs::read(&img).unwrap(), data);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_source() {
        let dir = tmp_dir("empty");
        let cancel = AtomicBool::new(false);
        let out = image(
            &Vec::<u8>::new(),
            &dir.join("e"),
            &dir.join("e.map"),
            &small_opts(),
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!((out.finished, out.bad, out.cancelled), (0, 0, false));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

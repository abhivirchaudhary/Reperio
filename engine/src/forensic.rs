//! Forensic acquisition: an image of a drive that can stand as evidence.
//!
//! On top of the ddrescue-style copy in [`crate::imaging`], this hashes the
//! finished image (MD5, SHA-1 and SHA-256, the hashes courts and forensic
//! tools expect), can read the source again to show the image matches it,
//! and writes an acquisition log: who made the copy and for which case,
//! what the drive is, how the copy went pass by pass, which sectors could
//! not be read, and the hashes.
//!
//! The copy writes the image out of order (healthy areas first, damaged
//! ones later), so the hashes cannot be computed on the fly; the finished
//! image is read once more from start to end instead. Verification reads
//! the source a second time alongside it.

use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use md5::Md5;
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::health;
use crate::imaging::{self, ImageOptions, ImageOutcome, ImageProgress, Pass};
use crate::source::Source;

/// Which hashes to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Algorithms {
    pub md5: bool,
    pub sha1: bool,
    pub sha256: bool,
}

impl Default for Algorithms {
    fn default() -> Self {
        Self::ALL
    }
}

impl Algorithms {
    pub const ALL: Self = Self {
        md5: true,
        sha1: true,
        sha256: true,
    };
    pub const NONE: Self = Self {
        md5: false,
        sha1: false,
        sha256: false,
    };

    /// Parses a list such as `md5,sha1,sha256` (also `sha-1`, `all`).
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut a = Self::NONE;
        for name in list.split([',', ' ']).filter(|s| !s.is_empty()) {
            match name.to_ascii_lowercase().replace('-', "").as_str() {
                "md5" => a.md5 = true,
                "sha1" => a.sha1 = true,
                "sha256" => a.sha256 = true,
                "all" => a = Self::ALL,
                other => {
                    return Err(format!(
                        "unknown hash \"{other}\"; choose from md5, sha1, sha256"
                    ))
                }
            }
        }
        if a == Self::NONE {
            return Err("name at least one hash: md5, sha1 or sha256".into());
        }
        Ok(a)
    }

    pub fn any(self) -> bool {
        self != Self::NONE
    }
}

/// Hashes as lowercase hex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Hashes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl Hashes {
    /// `(name, hex)` of each hash computed.
    pub fn list(&self) -> Vec<(&'static str, &str)> {
        [
            ("MD5", &self.md5),
            ("SHA-1", &self.sha1),
            ("SHA-256", &self.sha256),
        ]
        .into_iter()
        .filter_map(|(n, h)| h.as_deref().map(|h| (n, h)))
        .collect()
    }
}

/// Every hash asked for, fed the same bytes.
pub struct Hasher {
    md5: Option<Md5>,
    sha1: Option<Sha1>,
    sha256: Option<Sha256>,
}

impl Hasher {
    pub fn new(a: Algorithms) -> Self {
        Self {
            md5: a.md5.then(Md5::new),
            sha1: a.sha1.then(Sha1::new),
            sha256: a.sha256.then(Sha256::new),
        }
    }

    /// Feeds `data` to each hash, each on its own thread: hashing a
    /// terabyte three ways on one core would take far longer than reading
    /// it.
    pub fn update(&mut self, data: &[u8]) {
        Self::update_all(&mut [self], data);
    }

    /// Feeds `data` to several hashers at once, every hash on its own thread.
    pub fn update_all(hashers: &mut [&mut Hasher], data: &[u8]) {
        // Small pieces are not worth a thread.
        if data.len() < 64 << 10 {
            for h in hashers.iter_mut() {
                if let Some(d) = h.md5.as_mut() {
                    d.update(data);
                }
                if let Some(d) = h.sha1.as_mut() {
                    d.update(data);
                }
                if let Some(d) = h.sha256.as_mut() {
                    d.update(data);
                }
            }
            return;
        }
        std::thread::scope(|s| {
            for h in hashers.iter_mut() {
                if let Some(d) = h.md5.as_mut() {
                    s.spawn(move || d.update(data));
                }
                if let Some(d) = h.sha1.as_mut() {
                    s.spawn(move || d.update(data));
                }
                if let Some(d) = h.sha256.as_mut() {
                    s.spawn(move || d.update(data));
                }
            }
        });
    }

    pub fn finish(self) -> Hashes {
        Hashes {
            md5: self.md5.map(|d| hex(&d.finalize())),
            sha1: self.sha1.map(|d| hex(&d.finalize())),
            sha256: self.sha256.map(|d| hex(&d.finalize())),
        }
    }
}

/// Hashes `data` in one go.
pub fn hash_bytes(data: &[u8], a: Algorithms) -> Hashes {
    let mut h = Hasher::new(a);
    h.update(data);
    h.finish()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// An image read back as one source, whether it is one file or segments.
pub struct ImageFiles {
    /// `(file, start offset, length)` of each segment.
    parts: Vec<(File, u64, u64)>,
    len: u64,
}

impl ImageFiles {
    pub fn open(paths: &[PathBuf]) -> io::Result<Self> {
        let mut parts = Vec::new();
        let mut len = 0;
        for p in paths {
            let f = File::open(p)?;
            let n = f.metadata()?.len();
            parts.push((f, len, n));
            len += n;
        }
        Ok(Self { parts, len })
    }

    /// Where each segment starts and ends, as `(start, end)`.
    pub fn bounds(&self) -> Vec<(u64, u64)> {
        self.parts.iter().map(|&(_, s, n)| (s, s + n)).collect()
    }
}

impl Source for ImageFiles {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        for (f, start, n) in &self.parts {
            let at = offset + done as u64;
            if done == buf.len() || at >= self.len {
                break;
            }
            if at < *start || at >= start + n {
                continue;
            }
            let want = (buf.len() - done).min((start + n - at) as usize);
            read_exact_at(f, &mut buf[done..done + want], at - start)?;
            done += want;
        }
        Ok(done)
    }
}

fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(f, buf, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            let n = std::os::windows::fs::FileExt::seek_read(
                f,
                &mut buf[done..],
                offset + done as u64,
            )?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
}

/// Which check is running, for progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Reading the image back to hash it.
    Hashing,
    /// Reading the image and the source side by side.
    Verifying,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CheckProgress {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
}

/// What reading the source again showed.
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    /// The source's hashes as read now, unreadable sectors counted as zeros
    /// (as they are in the image).
    pub source: Hashes,
    /// The source hashes equal the image hashes.
    pub matched: bool,
    /// `(offset, length)` of areas whose bytes differ from the image.
    pub differing: Vec<(u64, u64)>,
    /// `(offset, length)` of areas the copy read but that failed now.
    pub unreadable: Vec<(u64, u64)>,
    /// Bytes not read again because the copy already found them unreadable:
    /// a failing drive is not made to fight over them twice.
    pub skipped_bad: u64,
}

/// The result of [`check`].
#[derive(Debug, Clone, Serialize)]
pub struct Checked {
    pub image: Hashes,
    /// Each segment's own hashes, for split images.
    pub segments: Vec<Hashes>,
    pub verification: Option<Verification>,
}

/// The source to verify against, and the `(offset, length)` areas the copy
/// found unreadable there.
pub type Verify<'a> = (&'a dyn Source, &'a [(u64, u64)]);

/// Bytes read at a time when hashing.
const CHUNK: u64 = 4 << 20;

/// Hashes the image and, given `verify`, reads the source again alongside
/// it. `verify` holds the source and the `(offset, length)` areas the copy
/// found unreadable, which are taken as zeros instead of read again; `sector`
/// is the unit differences are reported in. Returns `None` if cancelled.
pub fn check(
    image: &ImageFiles,
    algorithms: Algorithms,
    verify: Option<Verify>,
    sector: u64,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(&CheckProgress),
) -> io::Result<Option<Checked>> {
    let sector = sector.max(1);
    let len = image.len();
    let stage = if verify.is_some() {
        Stage::Verifying
    } else {
        Stage::Hashing
    };
    let bounds = image.bounds();
    let split = bounds.len() > 1;
    let mut whole = Hasher::new(algorithms);
    let mut segs: Vec<Hasher> = if split {
        bounds.iter().map(|_| Hasher::new(algorithms)).collect()
    } else {
        Vec::new()
    };
    let mut from_source = verify.map(|_| Hasher::new(algorithms));
    let mut differing = Ranges::default();
    let mut unreadable = Ranges::default();
    let mut skipped_bad = 0;

    let mut img = vec![0u8; CHUNK as usize];
    let mut src_buf = vec![0u8; CHUNK as usize];
    let mut pos = 0;
    let mut seg = 0;
    let mut last = std::time::Instant::now();
    on_progress(&CheckProgress {
        stage,
        done: 0,
        total: len,
    });
    while pos < len {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        while bounds[seg].1 <= pos {
            seg += 1;
        }
        // Never cross a segment's end, so each segment hashes on its own.
        let end = (pos + CHUNK).min(bounds[seg].1);
        let n = (end - pos) as usize;
        let got = image.read_at(pos, &mut img[..n])?;
        if got < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the image is shorter than it should be",
            ));
        }
        let img = &img[..n];
        let mut hashers: Vec<&mut Hasher> = vec![&mut whole];
        if split {
            hashers.push(&mut segs[seg]);
        }
        if let (Some((src, bad)), Some(h)) = (verify, from_source.as_mut()) {
            let buf = &mut src_buf[..n];
            skipped_bad += read_source(src, bad, pos, buf, sector, &mut unreadable);
            for (i, (a, b)) in img
                .chunks(sector as usize)
                .zip(buf.chunks(sector as usize))
                .enumerate()
            {
                if a != b {
                    differing.add(pos + i as u64 * sector, a.len() as u64);
                }
            }
            Hasher::update_all(&mut hashers, img);
            h.update(buf);
        } else {
            Hasher::update_all(&mut hashers, img);
        }
        pos = end;
        if last.elapsed().as_millis() >= 100 || pos == len {
            last = std::time::Instant::now();
            on_progress(&CheckProgress {
                stage,
                done: pos,
                total: len,
            });
        }
    }
    let image_hashes = whole.finish();
    let verification = from_source.map(|h| {
        let source = h.finish();
        Verification {
            matched: source == image_hashes,
            source,
            differing: differing.0,
            unreadable: unreadable.0,
            skipped_bad,
        }
    });
    Ok(Some(Checked {
        image: image_hashes,
        segments: segs.into_iter().map(Hasher::finish).collect(),
        verification,
    }))
}

/// Reads `buf.len()` bytes of the source at `pos` for verification. Areas
/// in `bad` are left as zeros without reading; sectors that fail now are
/// zeros too and noted in `unreadable`. Returns the bytes skipped.
fn read_source(
    src: &dyn Source,
    bad: &[(u64, u64)],
    pos: u64,
    buf: &mut [u8],
    sector: u64,
    unreadable: &mut Ranges,
) -> u64 {
    buf.fill(0);
    let end = pos + buf.len() as u64;
    let mut skipped = 0;
    let mut at = pos;
    // The pieces of [pos, end) outside the known-bad areas.
    let mut pieces = Vec::new();
    for &(b, n) in bad.iter().filter(|&&(b, n)| b < end && b + n > pos) {
        if b > at {
            pieces.push((at, b));
        }
        let stop = (b + n).min(end);
        skipped += stop - b.max(at);
        at = at.max(stop);
    }
    if at < end {
        pieces.push((at, end));
    }
    for (s, e) in pieces {
        let part = &mut buf[(s - pos) as usize..(e - pos) as usize];
        if matches!(src.read_at(s, part), Ok(n) if n == part.len()) {
            continue;
        }
        // Sector by sector, to find exactly what fails now.
        part.fill(0);
        let mut p = s;
        while p < e {
            let q = ((p / sector + 1) * sector).min(e);
            let piece = &mut part[(p - s) as usize..(q - s) as usize];
            match src.read_at(p, piece) {
                Ok(n) if n == piece.len() => {}
                _ => {
                    piece.fill(0);
                    unreadable.add(p, q - p);
                }
            }
            p = q;
        }
    }
    skipped
}

/// `(offset, length)` areas, joined where they touch.
#[derive(Default)]
struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    fn add(&mut self, pos: u64, len: u64) {
        if let Some(last) = self.0.last_mut() {
            if last.0 + last.1 == pos {
                last.1 += len;
                return;
            }
        }
        self.0.push((pos, len));
    }
}

/// Details of the case, all optional, as the examiner gives them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CaseInfo {
    pub case_number: Option<String>,
    pub examiner: Option<String>,
    pub evidence: Option<String>,
    pub notes: Option<String>,
}

/// What to do beyond the copy itself.
#[derive(Debug, Clone, Default)]
pub struct ForensicOptions {
    pub case: CaseInfo,
    pub algorithms: Algorithms,
    /// Read the source again and compare it with the image.
    pub verify: bool,
    /// Where to write the report, without extension: `.json` and `.txt`
    /// are added. Defaults to the image's path with `.report`.
    pub report: Option<PathBuf>,
}

/// Where the reports for an image at `dest` go by default, without
/// extension.
pub fn default_report_base(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".report");
    PathBuf::from(p)
}

/// The `.json` and `.txt` paths for a report base. A base already ending
/// in `.json` or `.txt` has that dropped first.
pub fn report_paths(base: &Path) -> (PathBuf, PathBuf) {
    let base = match base.extension().and_then(|e| e.to_str()) {
        Some("json" | "txt") => base.with_extension(""),
        _ => base.to_path_buf(),
    };
    let with = |ext: &str| {
        let mut p = base.as_os_str().to_owned();
        p.push(ext);
        PathBuf::from(p)
    };
    (with(".json"), with(".txt"))
}

/// What a drive says about itself, for the report. Anything the system
/// does not know is left empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    /// Logical sector size in bytes.
    pub sector_size: Option<u64>,
    pub physical_sector_size: Option<u64>,
    /// How it is attached: "SATA", "NVMe", "USB (SATA inside)", ...
    pub transport: Option<String>,
}

/// Asks the system what the drive at `device` is, with the same sources as
/// the drive health check (sysfs or diskutil, and smartctl where installed).
pub fn identify(device: &Path) -> DeviceInfo {
    let r = health::report(device);
    let (sector_size, physical_sector_size) = sector_sizes(device);
    DeviceInfo {
        model: r.model,
        serial: r.serial,
        firmware: r.firmware,
        sector_size,
        physical_sector_size,
        transport: r.interface,
    }
}

/// Logical and physical sector sizes from sysfs (Linux only).
fn sector_sizes(device: &Path) -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let Some(name) = device
            .canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        else {
            return (None, None);
        };
        let mut dir = Path::new("/sys/class/block").join(name);
        // A partition keeps these on its parent disk.
        if dir.join("partition").exists() {
            if let Some(parent) = dir
                .canonicalize()
                .ok()
                .and_then(|r| r.parent().map(Path::to_path_buf))
            {
                dir = parent;
            }
        }
        let read = |rel: &str| {
            std::fs::read_to_string(dir.join(rel))
                .ok()
                .and_then(|s| s.trim().parse().ok())
        };
        (
            read("queue/logical_block_size"),
            read("queue/physical_block_size"),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        (None, None)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    pub path: String,
    /// "device" or "file".
    pub kind: &'static str,
    pub size: u64,
    #[serde(flatten)]
    pub device: DeviceInfo,
    /// The source was opened for reading only and never written.
    pub opened_read_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentReport {
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PassReport {
    pub pass: Pass,
    pub number: u32,
    pub started: String,
    pub ended: String,
    pub copied: u64,
    pub read_errors: u64,
    pub unreadable_after: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Area {
    pub offset: u64,
    pub length: u64,
    pub first_sector: u64,
    pub last_sector: u64,
}

impl Area {
    fn new((offset, length): (u64, u64), sector: u64) -> Self {
        Self {
            offset,
            length,
            first_sector: offset / sector,
            last_sector: (offset + length).div_ceil(sector).saturating_sub(1),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub matched: bool,
    pub source_hashes: Hashes,
    pub differing: Vec<Area>,
    pub unreadable_now: Vec<Area>,
    pub skipped_known_bad: u64,
    pub started: String,
    pub ended: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub copied: u64,
    pub unreadable: u64,
    pub not_read: u64,
    pub read_errors: u64,
    /// Every byte was either copied or found unreadable sector by sector.
    pub complete: bool,
}

/// The acquisition log, written as JSON and as text.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub tool: String,
    pub tool_version: String,
    pub case: CaseInfo,
    pub source: SourceReport,
    pub image_format: &'static str,
    pub image: Vec<SegmentReport>,
    pub image_size: u64,
    pub map_file: String,
    /// When this run began and when the report was finished (UTC).
    pub started: String,
    pub finished: String,
    /// This run continued an earlier, stopped copy.
    pub resumed: bool,
    pub sector_size: u64,
    pub read_size: u64,
    pub scrape: bool,
    pub retries: u32,
    pub passes: Vec<PassReport>,
    pub totals: Totals,
    pub unreadable: Vec<Area>,
    pub not_read: Vec<Area>,
    pub hashes: Hashes,
    pub hashed: String,
    pub verification: Option<VerificationReport>,
}

impl Report {
    /// Writes the report as JSON and text; returns their paths.
    pub fn write(&self, base: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let (json, txt) = report_paths(base);
        let body = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&json, body + "\n")?;
        std::fs::write(&txt, self.to_text())?;
        Ok((json, txt))
    }

    pub fn to_text(&self) -> String {
        let mut o = String::new();
        let line = |o: &mut String, k: &str, v: &str| {
            let _ = writeln!(o, "  {:<22} {v}", format!("{k}:"));
        };
        let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "(not given)".into());
        let unknown = |v: &Option<String>| v.clone().unwrap_or_else(|| "(not reported)".into());
        let _ = writeln!(o, "FORENSIC ACQUISITION REPORT");
        let _ = writeln!(o, "{} {}", self.tool, self.tool_version);
        let _ = writeln!(o);
        let _ = writeln!(o, "Case");
        line(&mut o, "Case number", &opt(&self.case.case_number));
        line(&mut o, "Examiner", &opt(&self.case.examiner));
        line(&mut o, "Evidence", &opt(&self.case.evidence));
        line(&mut o, "Notes", &opt(&self.case.notes));
        let _ = writeln!(o);
        let s = &self.source;
        let d = &s.device;
        let _ = writeln!(o, "Source");
        line(&mut o, "Path", &s.path);
        line(&mut o, "Kind", s.kind);
        line(&mut o, "Size", &format!("{} bytes", s.size));
        if s.kind == "device" {
            line(&mut o, "Model", &unknown(&d.model));
            line(&mut o, "Serial number", &unknown(&d.serial));
            line(&mut o, "Firmware", &unknown(&d.firmware));
            line(&mut o, "Connection", &unknown(&d.transport));
            let ss = |v: Option<u64>| v.map_or("(not reported)".into(), |v| format!("{v} bytes"));
            line(&mut o, "Logical sector size", &ss(d.sector_size));
            line(&mut o, "Physical sector size", &ss(d.physical_sector_size));
        }
        line(
            &mut o,
            "Access",
            if s.opened_read_only {
                "opened read-only; nothing was written to the source"
            } else {
                "unknown"
            },
        );
        let _ = writeln!(o);
        let _ = writeln!(o, "Image");
        line(&mut o, "Format", self.image_format);
        line(&mut o, "Size", &format!("{} bytes", self.image_size));
        for (i, seg) in self.image.iter().enumerate() {
            let k = if self.image.len() > 1 {
                format!("Segment {}", i + 1)
            } else {
                "File".to_string()
            };
            line(&mut o, &k, &format!("{} ({} bytes)", seg.path, seg.size));
            if let Some(h) = &seg.hashes {
                for (n, v) in h.list() {
                    line(&mut o, &format!("  {n}"), v);
                }
            }
        }
        line(&mut o, "Map file", &self.map_file);
        let _ = writeln!(o);
        let _ = writeln!(o, "Acquisition");
        line(&mut o, "Started (UTC)", &self.started);
        line(&mut o, "Finished (UTC)", &self.finished);
        line(
            &mut o,
            "Resumed earlier copy",
            if self.resumed { "yes" } else { "no" },
        );
        line(
            &mut o,
            "Sector size used",
            &format!("{} bytes", self.sector_size),
        );
        line(
            &mut o,
            "First-pass read size",
            &format!("{} bytes", self.read_size),
        );
        line(
            &mut o,
            "Scrape damaged areas",
            if self.scrape { "yes" } else { "no" },
        );
        line(&mut o, "Retry passes", &self.retries.to_string());
        let _ = writeln!(o);
        let _ = writeln!(o, "Passes");
        if self.passes.is_empty() {
            let _ = writeln!(
                o,
                "  (none needed; the map showed the copy already finished)"
            );
        }
        for p in &self.passes {
            let _ = writeln!(
                o,
                "  {:<8} #{:<2} {} -> {}  copied {} bytes, {} read errors, {} bytes unreadable after",
                format!("{:?}", p.pass).to_lowercase(),
                p.number,
                p.started,
                p.ended,
                p.copied,
                p.read_errors,
                p.unreadable_after
            );
        }
        let _ = writeln!(o);
        let t = &self.totals;
        let _ = writeln!(o, "Result");
        line(&mut o, "Copied", &format!("{} bytes", t.copied));
        line(
            &mut o,
            "Unreadable",
            &format!("{} bytes (zeros in the image)", t.unreadable),
        );
        line(&mut o, "Not read", &format!("{} bytes", t.not_read));
        line(&mut o, "Read errors", &t.read_errors.to_string());
        line(&mut o, "Complete", if t.complete { "yes" } else { "no" });
        let _ = writeln!(o);
        let areas = |o: &mut String, title: &str, list: &[Area]| {
            let _ = writeln!(o, "{title} ({} areas)", list.len());
            if list.is_empty() {
                let _ = writeln!(o, "  none");
            }
            for a in list {
                let _ = writeln!(
                    o,
                    "  sectors {}-{}  bytes {}+{}",
                    a.first_sector, a.last_sector, a.offset, a.length
                );
            }
            let _ = writeln!(o);
        };
        areas(&mut o, "Unreadable sectors", &self.unreadable);
        if !self.not_read.is_empty() {
            areas(&mut o, "Areas not read", &self.not_read);
        }
        let _ = writeln!(o, "Image hashes (computed {})", self.hashed);
        for (n, v) in self.hashes.list() {
            line(&mut o, n, v);
        }
        let _ = writeln!(o);
        let _ = writeln!(o, "Verification");
        match &self.verification {
            None => {
                let _ = writeln!(o, "  not performed");
            }
            Some(v) => {
                line(
                    &mut o,
                    "Read again",
                    &format!("{} -> {}", v.started, v.ended),
                );
                for (n, h) in v.source_hashes.list() {
                    line(&mut o, &format!("Source {n}"), h);
                }
                line(
                    &mut o,
                    "Result",
                    if v.matched {
                        "MATCH: the source hashes equal the image hashes"
                    } else {
                        "MISMATCH: see the areas below"
                    },
                );
                if v.skipped_known_bad > 0 {
                    line(
                        &mut o,
                        "Not read again",
                        &format!(
                            "{} bytes already found unreadable, taken as zeros",
                            v.skipped_known_bad
                        ),
                    );
                }
                if !v.differing.is_empty() {
                    areas(&mut o, "  Differing areas", &v.differing);
                }
                if !v.unreadable_now.is_empty() {
                    areas(
                        &mut o,
                        "  Unreadable during verification",
                        &v.unreadable_now,
                    );
                }
            }
        }
        o
    }
}

/// Progress of a whole acquisition.
pub enum Progress<'a> {
    Imaging(&'a ImageProgress),
    Checking(&'a CheckProgress),
}

/// What [`acquire`] did.
pub struct Acquisition {
    pub outcome: ImageOutcome,
    /// The image's segments (one path when not split).
    pub segments: Vec<PathBuf>,
    /// Hashes, verification and the report, unless the copy or the
    /// check was cancelled.
    pub report: Option<Report>,
    /// `(json, text)` report paths, when written.
    pub report_paths: Option<(PathBuf, PathBuf)>,
}

/// Images `src` (found at `source_path`) to `dest` like
/// [`imaging::image`], then hashes the image, verifies it if asked and
/// writes the report.
#[allow(clippy::too_many_arguments)]
pub fn acquire(
    src: &dyn Source,
    source_path: &Path,
    dest: &Path,
    map: &Path,
    opts: &ImageOptions,
    forensic: &ForensicOptions,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(Progress),
) -> io::Result<Acquisition> {
    let started = SystemTime::now();
    let outcome = imaging::image(src, dest, map, opts, cancel, |p| {
        on_progress(Progress::Imaging(p))
    })?;
    let segments = imaging::segment_paths(dest, src.len(), opts.split);
    let mut acq = Acquisition {
        outcome,
        segments,
        report: None,
        report_paths: None,
    };
    if acq.outcome.cancelled {
        return Ok(acq);
    }
    let algorithms = if forensic.algorithms.any() {
        forensic.algorithms
    } else {
        Algorithms::ALL
    };
    let image = ImageFiles::open(&acq.segments)?;
    let check_started = SystemTime::now();
    let verify = forensic
        .verify
        .then_some((src, acq.outcome.bad_ranges.as_slice()));
    let Some(checked) = check(&image, algorithms, verify, opts.sector, cancel, |p| {
        on_progress(Progress::Checking(p))
    })?
    else {
        return Ok(acq);
    };
    let ended = SystemTime::now();
    let report = build_report(
        src,
        source_path,
        &acq,
        map,
        opts,
        forensic,
        checked,
        started,
        check_started,
        ended,
    );
    let base = forensic
        .report
        .clone()
        .unwrap_or_else(|| default_report_base(dest));
    acq.report_paths = Some(report.write(&base)?);
    acq.report = Some(report);
    Ok(acq)
}

#[allow(clippy::too_many_arguments)]
fn build_report(
    src: &dyn Source,
    source_path: &Path,
    acq: &Acquisition,
    map: &Path,
    opts: &ImageOptions,
    forensic: &ForensicOptions,
    checked: Checked,
    started: SystemTime,
    check_started: SystemTime,
    ended: SystemTime,
) -> Report {
    let is_file = std::fs::metadata(source_path).is_ok_and(|m| m.is_file());
    let device = if is_file {
        DeviceInfo::default()
    } else {
        identify(source_path)
    };
    let sector = opts.sector.max(1);
    let out = &acq.outcome;
    let image = acq
        .segments
        .iter()
        .enumerate()
        .map(|(i, p)| SegmentReport {
            path: p.display().to_string(),
            size: std::fs::metadata(p).map_or(0, |m| m.len()),
            hashes: checked.segments.get(i).cloned(),
        })
        .collect();
    let verification = checked.verification.map(|v| VerificationReport {
        matched: v.matched,
        source_hashes: v.source,
        differing: v
            .differing
            .into_iter()
            .map(|r| Area::new(r, sector))
            .collect(),
        unreadable_now: v
            .unreadable
            .into_iter()
            .map(|r| Area::new(r, sector))
            .collect(),
        skipped_known_bad: v.skipped_bad,
        started: utc(check_started),
        ended: utc(ended),
    });
    Report {
        tool: "Reperio".into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        case: forensic.case.clone(),
        source: SourceReport {
            path: source_path.display().to_string(),
            kind: if is_file { "file" } else { "device" },
            size: src.len(),
            device,
            opened_read_only: true,
        },
        image_format: "raw (dd), unreadable sectors filled with zeros",
        image,
        image_size: src.len(),
        map_file: map.display().to_string(),
        started: utc(started),
        finished: utc(ended),
        resumed: out.resumed,
        sector_size: sector,
        read_size: opts.cluster,
        scrape: opts.scrape,
        retries: opts.retries,
        passes: out
            .passes
            .iter()
            .map(|p| PassReport {
                pass: p.pass,
                number: p.number,
                started: utc(p.started),
                ended: utc(p.ended),
                copied: p.copied,
                read_errors: p.errors,
                unreadable_after: p.bad_after,
            })
            .collect(),
        totals: Totals {
            copied: out.finished,
            unreadable: out.bad,
            not_read: out.unread,
            read_errors: out.errors,
            complete: out.unread == 0,
        },
        unreadable: out
            .bad_ranges
            .iter()
            .map(|&r| Area::new(r, sector))
            .collect(),
        not_read: out
            .unread_ranges
            .iter()
            .map(|&r| Area::new(r, sector))
            .collect(),
        hashes: checked.image,
        hashed: utc(check_started),
        verification,
    }
}

/// A time as `YYYY-MM-DDTHH:MM:SSZ`, in UTC.
pub fn utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's days-to-civil conversion.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hash_test_vectors() {
        // FIPS 180 and RFC 1321 test vectors.
        let cases: [(&[u8], &str, &str, &str); 3] = [
            (
                b"",
                "d41d8cd98f00b204e9800998ecf8427e",
                "da39a3ee5e6b4b0d3255bfef95601890afd80709",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "900150983cd24fb0d6963f7d28e17f72",
                "a9993e364706816aba3e25717850c26c9cd0d89d",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "8215ef0796a20bcaaae116d3876c664a",
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (data, md5, sha1, sha256) in cases {
            let h = hash_bytes(data, Algorithms::ALL);
            assert_eq!(h.md5.as_deref(), Some(md5));
            assert_eq!(h.sha1.as_deref(), Some(sha1));
            assert_eq!(h.sha256.as_deref(), Some(sha256));
        }
        // A million 'a's, fed in uneven pieces, some large enough to be
        // hashed on threads.
        let mut h = Hasher::new(Algorithms::ALL);
        let million = vec![b'a'; 1_000_000];
        for piece in [&million[..1], &million[1..70_001], &million[70_001..]] {
            h.update(piece);
        }
        let h = h.finish();
        assert_eq!(h.md5.as_deref(), Some("7707d6ae4e027c70eea2a935c2296f21"));
        assert_eq!(
            h.sha1.as_deref(),
            Some("34aa973cd4c4daa4f61eeb2bdbad27316534016f")
        );
        assert_eq!(
            h.sha256.as_deref(),
            Some("cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0")
        );
        let only = hash_bytes(b"abc", Algorithms::parse("sha1").unwrap());
        assert_eq!((only.md5.is_none(), only.sha256.is_none()), (true, true));
    }

    #[test]
    fn parses_hash_lists() {
        assert_eq!(Algorithms::parse("md5,sha1,sha256"), Ok(Algorithms::ALL));
        assert_eq!(Algorithms::parse("all"), Ok(Algorithms::ALL));
        let a = Algorithms::parse("SHA-256").unwrap();
        assert!(a.sha256 && !a.md5 && !a.sha1);
        assert!(Algorithms::parse("crc32").is_err());
        assert!(Algorithms::parse("").is_err());
    }

    #[test]
    fn formats_utc_times() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400 + 3661); // 2000-02-29
        assert_eq!(utc(t), "2000-02-29T01:01:01Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        assert_eq!(utc(t), "2026-09-21T14:13:20Z");
    }

    #[test]
    fn report_paths_drop_a_given_extension() {
        let (j, t) = report_paths(Path::new("/x/case42.txt"));
        assert_eq!(
            (j.to_str(), t.to_str()),
            (Some("/x/case42.json"), Some("/x/case42.txt"))
        );
        let (j, _) = report_paths(&default_report_base(Path::new("/x/a.img")));
        assert_eq!(j.to_str(), Some("/x/a.img.report.json"));
    }

    /// A source with unreadable byte ranges, and optionally different bytes
    /// on a second reading.
    struct Card {
        data: Vec<u8>,
        bad: Vec<(u64, u64)>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl Source for Card {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let end = (offset + buf.len() as u64).min(self.len());
            if offset >= end {
                return Ok(0);
            }
            if self.bad.iter().any(|&(s, e)| s < end && offset < e) {
                return Err(io::Error::other("bad sector"));
            }
            let n = (end - offset) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(n)
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reperio-forensic-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Images a card with a bad region into split segments, then checks
    /// the report lists the region, the hashes match the expected bytes,
    /// each segment is hashed, and verification matches.
    #[test]
    fn acquires_with_report_hashes_and_verification() {
        let dir = tmp_dir("acquire");
        let len = (5 << 20) + 3 * 512;
        let data = pattern(len);
        let bad = (2_000_384u64, 2_000_384 + 8 * 512);
        let card = Card {
            data: data.clone(),
            bad: vec![bad],
            reads: Default::default(),
        };
        let dest = dir.join("ev.img");
        let map = dir.join("ev.img.map");
        let opts = ImageOptions {
            cluster: 64 << 10,
            split: Some(2 << 20),
            ..ImageOptions::default()
        };
        let forensic = ForensicOptions {
            case: CaseInfo {
                case_number: Some("2026-0042".into()),
                examiner: Some("A. Examiner".into()),
                evidence: Some("Item 3: SD card".into()),
                notes: Some("Seized at scene".into()),
            },
            verify: true,
            ..ForensicOptions::default()
        };
        let cancel = AtomicBool::new(false);
        let mut stages = Vec::new();
        let acq = acquire(
            &card,
            &dir.join("card"),
            &dest,
            &map,
            &opts,
            &forensic,
            &cancel,
            |p| {
                if let Progress::Checking(c) = p {
                    stages.push(c.stage);
                }
            },
        )
        .unwrap();
        assert_eq!(stages.first(), Some(&Stage::Verifying));
        assert_eq!(acq.segments.len(), 3);
        assert_eq!(acq.segments[0].file_name().unwrap(), "ev.img.001");
        let mut joined = Vec::new();
        for s in &acq.segments {
            joined.extend(std::fs::read(s).unwrap());
        }
        let mut want = data.clone();
        want[bad.0 as usize..bad.1 as usize].fill(0);
        assert_eq!(joined, want);

        let r = acq.report.as_ref().unwrap();
        assert_eq!(r.hashes, hash_bytes(&want, Algorithms::ALL));
        assert_eq!(
            r.image[1].hashes,
            Some(hash_bytes(&want[2 << 20..4 << 20], Algorithms::ALL))
        );
        assert_eq!(r.unreadable.len(), 1);
        assert_eq!(
            (
                r.unreadable[0].offset,
                r.unreadable[0].length,
                r.unreadable[0].first_sector
            ),
            (bad.0, bad.1 - bad.0, bad.0 / 512)
        );
        assert!(r.totals.complete);
        assert!(r.passes.iter().any(|p| p.pass == Pass::Copy));
        assert_eq!(
            r.passes.iter().map(|p| p.copied).sum::<u64>(),
            r.totals.copied
        );
        let v = r.verification.as_ref().unwrap();
        assert!(v.matched, "{v:?}");
        assert_eq!(v.skipped_known_bad, bad.1 - bad.0);

        let (json, txt) = acq.report_paths.clone().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(parsed["case"]["case_number"], "2026-0042");
        assert_eq!(parsed["source"]["opened_read_only"], true);
        assert_eq!(parsed["hashes"]["sha256"], r.hashes.sha256.clone().unwrap());
        let text = std::fs::read_to_string(&txt).unwrap();
        assert!(text.contains("MATCH"), "{text}");
        assert!(
            text.contains(&format!("sectors {}-{}", bad.0 / 512, bad.1 / 512 - 1)),
            "{text}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Verification names the sectors that changed and the ones that no
    /// longer read.
    #[test]
    fn verification_finds_differences() {
        let dir = tmp_dir("verify");
        let data = pattern(1 << 20);
        let img = dir.join("i.img");
        let mut on_disk = data.clone();
        on_disk[5000] ^= 0xff; // differs in sector 9
        std::fs::write(&img, &on_disk).unwrap();
        let image = ImageFiles::open(std::slice::from_ref(&img)).unwrap();
        let card = Card {
            data,
            bad: vec![(700_000, 700_001)],
            reads: Default::default(),
        };
        let cancel = AtomicBool::new(false);
        let c = check(
            &image,
            Algorithms::ALL,
            Some((&card, &[])),
            512,
            &cancel,
            |_| {},
        )
        .unwrap()
        .unwrap();
        let v = c.verification.unwrap();
        assert!(!v.matched);
        assert_eq!(v.differing[0], (4608, 512));
        let bad_sector = 700_000 / 512 * 512;
        assert_eq!(v.unreadable, vec![(bad_sector, 512)]);
        assert!(v.differing.contains(&(bad_sector, 512)));
        assert_eq!(c.image, hash_bytes(&on_disk, Algorithms::ALL));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_copy_writes_no_report() {
        let dir = tmp_dir("cancel");
        let card = Card {
            data: pattern(1 << 20),
            bad: vec![],
            reads: Default::default(),
        };
        let cancel = AtomicBool::new(true);
        let dest = dir.join("c.img");
        let acq = acquire(
            &card,
            Path::new("card"),
            &dest,
            &dir.join("c.map"),
            &ImageOptions::default(),
            &ForensicOptions::default(),
            &cancel,
            |_| {},
        )
        .unwrap();
        assert!(acq.outcome.cancelled && acq.report.is_none());
        assert!(!default_report_base(&dest)
            .with_extension("report.json")
            .exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

//! The scan loop: reads the source in large sequential chunks, tries every
//! format at each sector boundary, and hands back what it finds.

use std::io::{self, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::formats::{self, DateTime, Format, Kind, HEAD_LEN};
use crate::reader::Reader;
use crate::source::{read_skipping, read_tolerant, Source, ALIGN};

pub(crate) const CHUNK: usize = 8 << 20;

/// A chunk read that takes this long (under 2 MB/s) without an error means
/// the card is retrying internally: an early sign that it is failing.
const SLOW_READ: Duration = Duration::from_secs(4);

/// A recoverable file.
#[derive(Debug, Clone)]
pub struct Hit {
    pub offset: u64,
    pub len: u64,
    pub format: &'static str,
    pub ext: &'static str,
    pub kind: Kind,
    pub taken: Option<DateTime>,
}

/// What a scan saw, besides the files it reported.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub bad_sectors: u64,
    /// Chunk reads that succeeded but took longer than [`SLOW_READ`].
    pub slow_reads: u64,
    /// Headers of photos and videos whose data broke off, with their
    /// format name: candidates for [`crate::repair`].
    pub broken: Vec<(u64, &'static str)>,
    /// Where `moov` boxes (video indexes) start, noted while reading so
    /// video repair need not read the source again.
    pub indexes: Vec<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub pos: u64,
    pub end: u64,
    pub bad_sectors: u64,
}

pub struct Scanner {
    formats: Vec<Box<dyn Format>>,
    /// Distance between candidate offsets. Files on FAT, exFAT, HFS+ and
    /// APFS always start on a sector boundary, so 512 finds everything a
    /// filesystem wrote while skipping 511 of every 512 false starts.
    pub step: u64,
    pub start: u64,
    pub end: Option<u64>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            formats: formats::all(),
            step: 512,
            start: 0,
            end: None,
        }
    }
}

impl Scanner {
    /// Restricts the scan to formats whose name is in `names`.
    pub fn only(mut self, names: &[&str]) -> Self {
        self.formats.retain(|f| names.contains(&f.name()));
        self
    }

    pub fn format_names() -> Vec<&'static str> {
        formats::all().iter().map(|f| f.name()).collect()
    }

    /// Scans `src`. Either callback returns false to stop the scan early.
    pub fn scan(
        &self,
        src: &dyn Source,
        on_hit: impl FnMut(&Hit) -> bool,
        on_progress: impl FnMut(Progress) -> bool,
    ) -> Outcome {
        self.scan_with(src, on_hit, on_progress, |_, _| {})
    }

    /// Like [`Scanner::scan`], also showing `on_chunk` each stretch of data
    /// read (its position and bytes, which run a little past the next
    /// stretch's start), for finders that look for more than files.
    pub fn scan_with(
        &self,
        src: &dyn Source,
        mut on_hit: impl FnMut(&Hit) -> bool,
        mut on_progress: impl FnMut(Progress) -> bool,
        mut on_chunk: impl FnMut(u64, &[u8]),
    ) -> Outcome {
        let step = self.step.max(1);
        let end = self.end.unwrap_or(u64::MAX).min(src.len());
        let mut progress = Progress {
            pos: self.start,
            end,
            bad_sectors: 0,
        };
        let mut pos = self.start.div_ceil(step) * step;
        let mut broken = Vec::new();
        let mut indexes = Vec::new();
        let mut slow_reads = 0;
        let repairable: Vec<&dyn Format> = self
            .formats
            .iter()
            .filter(|f| matches!(f.name(), "jpeg" | "mp4"))
            .map(|f| f.as_ref())
            .collect();

        std::thread::scope(|scope| {
            // The next stretches are read on another thread while this one
            // is searched, so the disk never waits for the search.
            let (seek_tx, seek_rx) = mpsc::channel::<u64>();
            let (chunk_tx, chunk_rx) = mpsc::sync_channel::<Chunk>(1);
            let (spare_tx, spare_rx) = mpsc::channel::<Vec<u8>>();
            scope.spawn(move || read_ahead(src, end, seek_rx, chunk_tx, spare_rx));
            let _ = seek_tx.send(pos);

            'chunks: while pos < end {
                // The stretch holding `pos`. Stretches read ahead that the
                // search has jumped past are dropped.
                let chunk = loop {
                    let Ok(c) = chunk_rx.recv() else {
                        break 'chunks;
                    };
                    if c.pos + CHUNK as u64 > pos && c.pos <= pos {
                        break c;
                    }
                    let _ = spare_tx.send(c.buf);
                };
                let (base, buf, n) = (chunk.pos, &chunk.buf, chunk.n);
                slow_reads += u64::from(chunk.slow);
                progress.bad_sectors += chunk.bad;
                on_chunk(base, &buf[..n]);
                // Search past the stretch's end too, so an index straddling
                // the boundary is seen; each position counts in one only.
                for i in memchr::memmem::find_iter(&buf[..n], b"moov") {
                    if (4..CHUNK + 4).contains(&i) {
                        indexes.push(base + i as u64 - 4);
                    }
                }

                let mut off = pos - base;
                let mut jumped = false;
                while off < CHUNK as u64 && base + off < end {
                    let head = &buf[off as usize..off as usize + HEAD_LEN];
                    if let Some(hit) = self.try_formats(src, base + off, head) {
                        let next = (hit.offset + hit.len).div_ceil(step) * step;
                        if !on_hit(&hit) {
                            let _ = spare_tx.send(chunk.buf);
                            break 'chunks;
                        }
                        if next >= base + CHUNK as u64 {
                            // Beyond what has been read ahead: send the
                            // reader there instead.
                            if next >= base + 3 * CHUNK as u64 {
                                let _ = seek_tx.send(next);
                            }
                            pos = next;
                            jumped = true;
                            break;
                        }
                        off = next - base;
                    } else {
                        if let Some(f) = repairable.iter().find(|f| f.matches(head)) {
                            broken.push((base + off, f.name()));
                        }
                        off += step;
                    }
                }
                let _ = spare_tx.send(chunk.buf);
                if !jumped {
                    pos = base + CHUNK as u64;
                }
                progress.pos = pos.min(end);
                if !on_progress(progress) {
                    break;
                }
            }
            // Dropping the request channel stops the reader.
            drop(seek_tx);
            drop(chunk_rx);
        });
        indexes.sort_unstable();
        indexes.dedup();
        Outcome {
            bad_sectors: progress.bad_sectors,
            slow_reads,
            broken,
            indexes,
        }
    }

    fn try_formats(&self, src: &dyn Source, offset: u64, head: &[u8]) -> Option<Hit> {
        self.formats
            .iter()
            .filter(|f| f.matches(head))
            .find_map(|f| {
                let limit = offset.saturating_add(f.max_len()).min(src.len());
                let found = f.carve(&mut Reader::budgeted(src, offset), offset, limit)?;
                (found.len > 0).then(|| Hit {
                    offset,
                    len: found.len,
                    format: f.name(),
                    ext: found.ext,
                    kind: found.kind,
                    taken: found.taken,
                })
            })
    }
}

/// A stretch of the source read ahead of the search.
struct Chunk {
    pos: u64,
    /// `CHUNK + ALIGN` bytes, zero past `n`.
    buf: Vec<u8>,
    n: usize,
    bad: u64,
    slow: bool,
}

/// Reads stretches one after another from where it was last sent, handing
/// each over as soon as it is read.
fn read_ahead(
    src: &dyn Source,
    end: u64,
    seek: mpsc::Receiver<u64>,
    out: mpsc::SyncSender<Chunk>,
    spare: mpsc::Receiver<Vec<u8>>,
) {
    let Ok(mut pos) = seek.recv() else { return };
    // After a stretch that was mostly unreadable, the stretches up to where
    // the damage ends are not read: every failed read on a failing drive
    // can take seconds, and a damaged area can be gigabytes long. What is
    // skipped counts as unreadable; backing the drive up first gets
    // everything that can still be read.
    let mut skip_until = 0u64;
    loop {
        while let Ok(p) = seek.try_recv() {
            pos = p;
        }
        if pos >= end {
            match seek.recv() {
                Ok(p) => {
                    pos = p;
                    continue;
                }
                Err(_) => return,
            }
        }
        let mut buf = spare
            .try_recv()
            .unwrap_or_else(|_| vec![0u8; CHUNK + ALIGN as usize]);
        let began = Instant::now();
        let (n, bad) = if pos < skip_until {
            let n = CHUNK.min((end - pos) as usize);
            buf.fill(0);
            (n, n.div_ceil(512) as u64)
        } else {
            let (n, bad) = read_skipping(src, pos, &mut buf);
            if bad * 512 * 2 > n as u64 {
                skip_until = end_of_damage(src, pos, end);
            }
            (n, bad)
        };
        let slow = bad == 0 && began.elapsed() > SLOW_READ;
        buf[n..].fill(0);
        if out
            .send(Chunk {
                pos,
                buf,
                n,
                bad,
                slow,
            })
            .is_err()
        {
            return;
        }
        pos += CHUNK as u64;
    }
}

/// Where the damaged area holding the stretch at `bad` ends, to the
/// stretch: the first stretch after it whose start can be read, or the one
/// whose start can't but whose rest may. Found with single-block reads
/// further and further ahead, then halving back, so a damaged area costs
/// a few dozen failed reads however long it is.
fn end_of_damage(src: &dyn Source, bad: u64, end: u64) -> u64 {
    let chunk = CHUNK as u64;
    let readable = |at: u64| {
        let mut block = [0u8; ALIGN as usize];
        src.read_at(at, &mut block).is_ok()
    };
    let (mut lo, mut stride) = (bad, chunk);
    let mut hi = loop {
        let at = lo + stride;
        if at >= end {
            break end.div_ceil(chunk) * chunk;
        }
        if readable(at) {
            break at;
        }
        lo = at;
        stride = (stride * 2).min(1 << 30);
    };
    // Damage begins at `lo` (or before) and is over by `hi`.
    while hi - lo > chunk {
        let mid = lo + (hi - lo) / chunk / 2 * chunk;
        if readable(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    // The stretch at `lo` starts damaged but may end readable: read it.
    lo.max(bad + chunk)
}

/// Copies a recovered file out of the source. Returns the number of
/// unreadable sectors, which were written as zeros.
pub fn extract(src: &dyn Source, hit: &Hit, out: &mut impl Write) -> io::Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut bad = 0;
    let mut done = 0;
    while done < hit.len {
        let want = buf.len().min((hit.len - done) as usize);
        let (n, b) = read_tolerant(src, hit.offset + done, &mut buf[..want]);
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source ended early",
            ));
        }
        out.write_all(&buf[..n])?;
        bad += b;
        done += n as u64;
    }
    Ok(bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{bmff, jpeg, tiff};

    /// Deleted files sit at sector boundaries between leftover junk.
    #[test]
    fn recovers_files_between_junk() {
        let files = [
            jpeg::tests::sample(40_000),
            bmff::tests::sample(70_000),
            tiff::tests::sample("SONY", 30_000),
        ];
        let mut card: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let mut offsets = Vec::new();
        for f in &files {
            let at = card.len().div_ceil(512) * 512 + 4096;
            card.resize(at, 0x33);
            offsets.push(at as u64);
            card.extend(f);
            card.extend([0x99; 777]);
        }

        let mut hits = Vec::new();
        Scanner::default().scan(
            &card,
            |h| {
                hits.push(h.clone());
                true
            },
            |_| true,
        );

        let got: Vec<(u64, u64, &str)> = hits.iter().map(|h| (h.offset, h.len, h.ext)).collect();
        let want: Vec<(u64, u64, &str)> = offsets
            .iter()
            .zip(&files)
            .zip(["jpg", "mov", "arw"])
            .map(|((&o, f), e)| (o, f.len() as u64, e))
            .collect();
        assert_eq!(got, want);

        let mut out = Vec::new();
        extract(&card, &hits[1], &mut out).unwrap();
        assert_eq!(out, files[1]);
    }

    /// A drive with an unreadable area, where each failed read is slow.
    struct Failing {
        data: Vec<u8>,
        bad: std::ops::Range<u64>,
        failed: std::sync::atomic::AtomicU64,
    }

    impl Source for Failing {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let end = offset + buf.len() as u64;
            if offset < self.bad.end && self.bad.start < end {
                self.failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
                return Err(std::io::Error::other("unreadable"));
            }
            let end = end.min(self.len());
            let n = end.saturating_sub(offset) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(n)
        }
    }

    #[test]
    fn gets_past_unreadable_areas_quickly() {
        let photo = jpeg::tests::sample(50_000);
        for (bad, around) in [
            // One bad block: the rest of its stretch is still read.
            (5_000_000..5_004_096, true),
            // 100 MB that can't be read: skipped, not fought over.
            (2_000_000..102_000_000, false),
        ] {
            let mut data = vec![0u8; 112 << 20];
            let before = 4_096_000;
            let after = 108 << 20;
            data[before..before + photo.len()].copy_from_slice(&photo);
            data[after..after + photo.len()].copy_from_slice(&photo);
            // Right behind the bad block, in the same stretch.
            let behind = 5_100_000 / 512 * 512;
            data[behind..behind + photo.len()].copy_from_slice(&photo);
            let src = Failing {
                data,
                bad: bad.clone(),
                failed: Default::default(),
            };
            let mut found = Vec::new();
            let began = Instant::now();
            let outcome = Scanner::default().scan(
                &src,
                |h| {
                    found.push(h.offset);
                    true
                },
                |_| true,
            );
            let failed = src.failed.into_inner();
            assert!(found.contains(&(after as u64)), "{bad:?}: {found:?}");
            assert_eq!(
                found.contains(&(behind as u64)),
                around,
                "{bad:?}: {found:?}"
            );
            assert!(outcome.bad_sectors > 0);
            assert!(failed < 600, "{bad:?}: {failed} failed reads");
            assert!(began.elapsed() < Duration::from_secs(60));
        }
    }
}

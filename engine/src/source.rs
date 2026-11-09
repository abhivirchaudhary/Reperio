//! Read-only access to whatever we are recovering from: a raw device
//! (`/dev/rdisk4`), a block device (`/dev/sdb`) or a disk image file.
//!
//! Nothing in this module ever opens a source for writing.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::path::Path;

/// Raw devices on macOS reject reads that are not sector aligned, so every
/// read is widened to this granularity. 4096 is a multiple of every sector
/// size in use (512 and 4K-native).
pub const ALIGN: u64 = 4096;

/// A random-access, read-only byte source.
pub trait Source: Send + Sync {
    /// Total size in bytes.
    fn len(&self) -> u64;

    /// Reads up to `buf.len()` bytes at `offset`. Returns the number of bytes
    /// read, which is only short at the end of the source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A device or image file opened read-only.
pub struct DeviceSource {
    file: File,
    len: u64,
}

impl DeviceSource {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let len = device_len(&mut file)?;
        Ok(Self { file, len })
    }

    fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match pread(&self.file, &mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }
}

impl Source for DeviceSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min((self.len - offset) as usize);
        let buf = &mut buf[..want];

        if offset.is_multiple_of(ALIGN) && (want as u64).is_multiple_of(ALIGN) {
            return self.read_aligned(offset, buf);
        }

        // Bounce through an aligned buffer. The aligned end may run past the
        // end of the source; read_aligned handles the short read.
        let start = offset - offset % ALIGN;
        let end = (offset + want as u64).div_ceil(ALIGN) * ALIGN;
        let mut tmp = vec![0u8; (end - start) as usize];
        let got = self.read_aligned(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        let n = got.saturating_sub(skip).min(want);
        buf[..n].copy_from_slice(&tmp[skip..skip + n]);
        Ok(n)
    }
}

/// A shared source is a source, so a device of any kind (a disk, or one
/// served by a plug-in) can be passed around and read the same way.
impl Source for std::sync::Arc<dyn Source> {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

/// An in-memory source, used by tests.
impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let data = self.as_slice();
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let start = offset as usize;
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }
}

/// A read cache for parsers that read a source in many small pieces, such
/// as a file system's tables a node or block at a time. On a raw USB
/// device every read is a round trip, so a million small reads take
/// minutes where a few hundred large ones take seconds.
///
/// Each miss loads a window from the miss onwards. While reads keep going
/// forwards past the window, the window doubles (up to [`Cached::MAX`]);
/// a read elsewhere drops it back to [`Cached::MIN`], so parsers that jump
/// around do not pay for reading ahead.
pub struct Cached<'a> {
    src: &'a dyn Source,
    window: std::sync::Mutex<Window>,
}

#[derive(Default)]
struct Window {
    start: u64,
    data: Vec<u8>,
    size: usize,
}

impl<'a> Cached<'a> {
    pub const MIN: usize = 64 << 10;
    pub const MAX: usize = 4 << 20;

    pub fn new(src: &'a dyn Source) -> Self {
        Self {
            src,
            window: Default::default(),
        }
    }
}

impl Source for Cached<'_> {
    fn len(&self) -> u64 {
        self.src.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        // Big reads gain nothing; reads past the end (offsets from damaged
        // tables can be anything) are the source's to refuse.
        if buf.len() >= Self::MAX || offset >= self.src.len() {
            return self.src.read_at(offset, buf);
        }
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let end = w.start + w.data.len() as u64;
        let held = offset >= w.start && offset + buf.len() as u64 <= end;
        if !held {
            // Going on from (or just past) the window: read further ahead.
            let onwards =
                !w.data.is_empty() && offset >= w.start && offset <= end + Self::MAX as u64;
            w.size = if onwards {
                (w.size * 2).clamp(Self::MIN, Self::MAX)
            } else {
                Self::MIN
            };
            let start = offset - offset % ALIGN;
            let want = (w.size as u64)
                .max(offset + buf.len() as u64 - start)
                .min(self.src.len().saturating_sub(start)) as usize;
            let mut data = std::mem::take(&mut w.data);
            data.resize(want, 0);
            match self.src.read_at(start, &mut data) {
                Ok(n) => data.truncate(n),
                Err(_) => {
                    // Fall back to the read as asked, so one bad sector in
                    // the window does not fail the reads around it.
                    w.data = Vec::new();
                    drop(w);
                    return self.src.read_at(offset, buf);
                }
            }
            w.start = start;
            w.data = data;
        }
        let from = (offset - w.start) as usize;
        let n = buf.len().min(w.data.len().saturating_sub(from));
        buf[..n].copy_from_slice(&w.data[from..from + n]);
        Ok(n)
    }
}

/// Reads `buf` at `offset`, tolerating unreadable sectors on a failing card.
///
/// On an I/O error the range is retried one device block at a time, and
/// blocks that still fail are filled with zeros. Returns `(bytes_read,
/// bad_sectors)`, counting 512-byte sectors. For copying files out, where
/// every readable block counts; the deep scan uses [`read_skipping`].
pub fn read_tolerant(src: &dyn Source, offset: u64, buf: &mut [u8]) -> (usize, u64) {
    read_around_errors(src, offset, buf, false)
}

/// Like [`read_tolerant`], but gets past damaged areas quickly, as a deep
/// scan must: each failed read can take seconds on a failing drive, so
/// after a bad block the following ones are skipped (filled with zeros)
/// in growing steps, and a stretch that is still being fought over after
/// [`STRETCH_BUDGET`] is given up on.
pub fn read_skipping(src: &dyn Source, offset: u64, buf: &mut [u8]) -> (usize, u64) {
    read_around_errors(src, offset, buf, true)
}

/// Longest [`read_skipping`] spends on one stretch with errors.
pub const STRETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

fn read_around_errors(src: &dyn Source, offset: u64, buf: &mut [u8], skip: bool) -> (usize, u64) {
    if let Ok(n) = src.read_at(offset, buf) {
        return (n, 0);
    }
    const SECTOR: usize = 512;
    // Pieces first, so one bad block costs a few reads, not a whole
    // stretch read block by block.
    const PIECE: usize = 256 << 10;
    let block = ALIGN as usize;
    let want = buf.len().min(src.len().saturating_sub(offset) as usize);
    let began = std::time::Instant::now();
    let mut bad = 0u64;
    let mut lose = |buf: &mut [u8], from: usize, to: usize| {
        buf[from..to].fill(0);
        bad += (to - from).div_ceil(SECTOR) as u64;
    };
    let mut at = 0;
    while at < want {
        if skip && began.elapsed() > STRETCH_BUDGET {
            lose(buf, at, want);
            break;
        }
        let end = (at + PIECE).min(want);
        if src.read_at(offset + at as u64, &mut buf[at..end]).is_ok() {
            at = end;
            continue;
        }
        let mut step = block;
        while at < end {
            if skip && began.elapsed() > STRETCH_BUDGET {
                break;
            }
            let next = (at + block).min(end);
            if src.read_at(offset + at as u64, &mut buf[at..next]).is_ok() {
                at = next;
                step = block;
            } else if skip {
                let past = (at + step).min(end);
                lose(buf, at, past);
                at = past;
                step = (step * 2).min(PIECE);
            } else {
                lose(buf, at, next);
                at = next;
            }
        }
    }
    (want, bad)
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

fn device_len(file: &mut File) -> io::Result<u64> {
    let meta = file.metadata()?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    #[cfg(target_os = "macos")]
    if let Some(len) = macos_device_len(file) {
        return Ok(len);
    }
    // Linux block devices report their size through seek.
    let len = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    if len == 0 {
        return Err(io::Error::other(
            "could not determine the size of this device",
        ));
    }
    Ok(len)
}

/// macOS raw devices report a size of zero; ask the disk driver instead.
#[cfg(target_os = "macos")]
fn macos_device_len(file: &File) -> Option<u64> {
    use std::os::unix::io::AsRawFd;
    // _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t) from <sys/disk.h>.
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
    let fd = file.as_raw_fd();
    let mut block_size: u32 = 0;
    let mut block_count: u64 = 0;
    // SAFETY: both ioctls write a single integer of the declared size.
    unsafe {
        if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut block_size) != 0
            || libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut block_count) != 0
        {
            return None;
        }
    }
    Some(block_size as u64 * block_count).filter(|&n| n > 0)
}

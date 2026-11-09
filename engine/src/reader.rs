//! A buffered cursor over a [`Source`], used by the format parsers.
//!
//! Every accessor returns `None` at the end of the source or on an I/O
//! error. Parsers treat both the same way: the candidate file is not valid.

use crate::source::{Source, ALIGN};

const BUF_SIZE: usize = 256 * 1024;

/// Reads a parser may make before it is stopped. Real files never come
/// close; a walk that loops on damaged data would otherwise never end.
const MAX_OPS: u64 = 1 << 32;
/// Buffer loads allowed beyond those needed to read once through what the
/// walk has covered. A walk that keeps going back over the same ground (a
/// size field that wraps a position backwards) runs out of these.
const SPARE_LOADS: u64 = 4096;

pub struct Reader<'a> {
    src: &'a dyn Source,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
    pos: u64,
    /// For carving: stop walks that make no headway (see [`MAX_OPS`]).
    budgeted: bool,
    ops: u64,
    loads: u64,
    /// Where the walk began and the furthest it has read.
    lo: u64,
    hi: u64,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a dyn Source, pos: u64) -> Self {
        Self::with_capacity(src, pos, BUF_SIZE)
    }

    /// A reader with a smaller buffer, for walks that usually fail within
    /// a few kilobytes and would otherwise read far more than they parse.
    pub fn with_capacity(src: &'a dyn Source, pos: u64, size: usize) -> Self {
        let size = size.max(ALIGN as usize) / ALIGN as usize * ALIGN as usize;
        Self {
            src,
            buf: vec![0; size],
            buf_start: 0,
            buf_len: 0,
            pos,
            budgeted: false,
            ops: 0,
            loads: 0,
            lo: pos,
            hi: pos,
        }
    }

    /// A reader for a format walk from `pos`, which is stopped (reads fail)
    /// if it loops: more than [`MAX_OPS`] reads, or going back over what it
    /// read more than [`SPARE_LOADS`] times. Reading before `pos` counts
    /// as going back.
    pub fn budgeted(src: &'a dyn Source, pos: u64) -> Self {
        Self {
            budgeted: true,
            ..Self::new(src, pos)
        }
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    pub fn skip(&mut self, n: u64) {
        self.pos = self.pos.saturating_add(n);
    }

    pub fn source(&self) -> &'a dyn Source {
        self.src
    }

    pub fn source_len(&self) -> u64 {
        self.src.len()
    }

    /// Makes `pos` available in the buffer. Returns false at EOF or on error.
    fn fill(&mut self) -> bool {
        if self.pos >= self.buf_start && self.pos < self.buf_start + self.buf_len as u64 {
            return true;
        }
        let start = self.pos - self.pos % ALIGN;
        if self.budgeted {
            self.loads += 1;
            self.hi = self.hi.max(start + self.buf.len() as u64);
            let ahead = self.hi.saturating_sub(self.lo) / self.buf.len() as u64;
            if self.loads > SPARE_LOADS + 2 * ahead {
                self.buf_len = 0;
                return false;
            }
        }
        match self.src.read_at(start, &mut self.buf) {
            Ok(n) if start + n as u64 > self.pos => {
                self.buf_start = start;
                self.buf_len = n;
                true
            }
            _ => {
                self.buf_len = 0;
                false
            }
        }
    }

    /// The buffered bytes from the current position onwards (never empty
    /// unless at EOF).
    fn available(&mut self) -> Option<&[u8]> {
        self.ops += u64::from(self.budgeted);
        if self.ops > MAX_OPS || !self.fill() {
            return None;
        }
        let at = (self.pos - self.buf_start) as usize;
        Some(&self.buf[at..self.buf_len])
    }

    pub fn u8(&mut self) -> Option<u8> {
        let b = *self.available()?.first()?;
        self.pos += 1;
        Some(b)
    }

    /// Fills `out` from the current position and advances past it.
    pub fn read_exact(&mut self, out: &mut [u8]) -> Option<()> {
        let mut done = 0;
        while done < out.len() {
            let avail = self.available()?;
            let n = avail.len().min(out.len() - done);
            out[done..done + n].copy_from_slice(&avail[..n]);
            done += n;
            self.pos += n as u64;
        }
        Some(())
    }

    pub fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        // Lengths come from the data being parsed: refuse one that runs
        // past the end of the source before allocating for it, or a
        // corrupt size field could ask for terabytes.
        if n as u64 > self.src.len().saturating_sub(self.pos) {
            return None;
        }
        let mut v = vec![0; n];
        self.read_exact(&mut v)?;
        Some(v)
    }

    pub fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut a = [0; N];
        self.read_exact(&mut a)?;
        Some(a)
    }

    pub fn u16be(&mut self) -> Option<u16> {
        self.array().map(u16::from_be_bytes)
    }

    pub fn u32be(&mut self) -> Option<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub fn u64be(&mut self) -> Option<u64> {
        self.array().map(u64::from_be_bytes)
    }

    pub fn u32le(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    /// Advances to just past the next occurrence of `byte`, giving up once
    /// the position passes `limit`.
    pub fn skip_past(&mut self, byte: u8, limit: u64) -> Option<()> {
        loop {
            if self.pos >= limit {
                return None;
            }
            let avail = self.available()?;
            match memchr::memchr(byte, avail) {
                Some(i) => {
                    self.pos += i as u64 + 1;
                    return Some(());
                }
                None => self.pos += avail.len() as u64,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_stops_walks_that_loop() {
        let data = vec![7u8; 8 << 20];
        // Going back and forth between two places, as a walk whose size
        // field wrapped would, is stopped.
        let mut r = Reader::budgeted(&data, 4 << 20);
        let mut reads = 0u64;
        loop {
            r.seek(if reads.is_multiple_of(2) { 0 } else { 6 << 20 });
            if r.u8().is_none() {
                break;
            }
            reads += 1;
            assert!(reads < 100_000, "the walk was never stopped");
        }
        // Looping inside one buffer is stopped too, by the read count.
        let mut r = Reader::budgeted(&data, 0);
        r.ops = MAX_OPS - 10;
        assert!((0..20).any(|_| {
            r.seek(5);
            r.u8().is_none()
        }));
        // A walk forward through the whole source is not.
        let mut r = Reader::budgeted(&data, 0);
        for at in (0..data.len() as u64).step_by(4096) {
            r.seek(at);
            assert_eq!(r.u8(), Some(7));
        }
        // Nor is an unbudgeted reader going back and forth.
        let mut r = Reader::new(&data, 0);
        for i in 0..20_000u64 {
            r.seek(if i.is_multiple_of(2) { 0 } else { 6 << 20 });
            assert_eq!(r.u8(), Some(7));
        }
    }
}

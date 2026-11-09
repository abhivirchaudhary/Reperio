//! Byte ranges on the source, and a [`Source`] that stitches a list of them
//! into one contiguous stream.
//!
//! A recovered file is a list of extents: one for a contiguous file, several
//! for a file whose clusters were spread around other files. The same type
//! presents only the free space of a volume as a single stream, so carving it
//! reassembles files that were split around files still on the card.

use std::io;

use serde::{Deserialize, Serialize};

use crate::source::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

impl Extent {
    pub fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    /// Saturates: extents are built from on-disk values, and one reaching
    /// past the end of any disk is simply unreadable there.
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.len)
    }
}

/// Appends `ext` to `list`, merging it into the last extent when adjacent.
pub fn push_merged(list: &mut Vec<Extent>, ext: Extent) {
    if ext.len == 0 {
        return;
    }
    match list.last_mut() {
        Some(last) if last.end() == ext.offset => last.len += ext.len,
        _ => list.push(ext),
    }
}

/// Total length of a list of extents.
pub fn total_len(list: &[Extent]) -> u64 {
    list.iter().fold(0, |n, e| n.saturating_add(e.len))
}

/// A virtual stream made by concatenating extents of an underlying source.
pub struct MappedSource<'a> {
    src: &'a dyn Source,
    extents: Vec<Extent>,
    /// Virtual offset at which each extent begins.
    starts: Vec<u64>,
    len: u64,
}

impl<'a> MappedSource<'a> {
    pub fn new(src: &'a dyn Source, extents: Vec<Extent>) -> Self {
        let mut starts = Vec::with_capacity(extents.len());
        let mut len = 0;
        for e in &extents {
            starts.push(len);
            len = e.len.saturating_add(len);
        }
        Self {
            src,
            extents,
            starts,
            len,
        }
    }

    /// The physical extents behind the virtual range `[offset, offset + len)`.
    pub fn physical(&self, offset: u64, len: u64) -> Vec<Extent> {
        let mut out = Vec::new();
        let end = offset.saturating_add(len).min(self.len);
        let mut pos = offset;
        while pos < end {
            let i = self.index(pos);
            let e = self.extents[i];
            let within = pos - self.starts[i];
            let n = (e.len - within).min(end - pos);
            push_merged(&mut out, Extent::new(e.offset + within, n));
            pos += n;
        }
        out
    }

    fn index(&self, pos: u64) -> usize {
        self.starts.partition_point(|&s| s <= pos) - 1
    }
}

impl Source for MappedSource<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let end = offset.saturating_add(buf.len() as u64).min(self.len);
        let mut pos = offset;
        while pos < end {
            let i = self.index(pos);
            let e = self.extents[i];
            let within = pos - self.starts[i];
            let n = (e.len - within).min(end - pos) as usize;
            let at = (pos - offset) as usize;
            let got = self
                .src
                .read_at(e.offset.saturating_add(within), &mut buf[at..at + n])?;
            pos += got as u64;
            if got < n {
                break;
            }
        }
        Ok((pos.max(offset) - offset) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stitches_and_maps_back() {
        let data: Vec<u8> = (0..100u8).collect();
        let m = MappedSource::new(
            &data,
            vec![Extent::new(10, 5), Extent::new(50, 10), Extent::new(90, 10)],
        );
        assert_eq!(m.len(), 25);
        let mut buf = [0u8; 8];
        assert_eq!(m.read_at(3, &mut buf).unwrap(), 8);
        assert_eq!(buf, [13, 14, 50, 51, 52, 53, 54, 55]);
        assert_eq!(
            m.physical(3, 15),
            vec![Extent::new(13, 2), Extent::new(50, 10), Extent::new(90, 3)]
        );
        assert_eq!(m.read_at(24, &mut buf).unwrap(), 1);
    }
}

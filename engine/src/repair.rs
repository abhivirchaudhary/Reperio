//! Rebuilding photos stored in two pieces when nothing records the layout.
//!
//! Cameras write each photo into the first free clusters they find. When a
//! gap is too small, the photo continues after the next file in the way,
//! so it is stored in two pieces. The filesystem records the layout, but
//! after a format that record is gone, and plain carving returns the first
//! piece glued to whatever follows it.
//!
//! This implements bifragment gap carving: the JPEG's structure breaks
//! where the first piece ends, which narrows down where the split is
//! (often exactly where another file the deep scan found begins). Each
//! candidate start of the second piece is then tried, skipping space other
//! recovered files own and data that cannot be compressed image data, and
//! the first one that makes the whole JPEG parse cleanly to its end is
//! taken.

use crate::extent::{Extent, MappedSource};
use crate::formats::{bmff, jpeg, Format, Found};
use crate::reader::Reader;
use crate::source::Source;

/// Largest photo considered for repair.
const MAX_LEN: u64 = 256 << 20;
/// How far after the split the second piece is looked for.
const WINDOW: u64 = 512 << 20;
/// Split points tried, closest to the break first.
const MAX_SPLITS: usize = 6;
/// Candidate placements tried per photo, bounding the work on hopeless ones.
const MAX_TRIES: usize = 400;
/// Buffer for trial walks: wrong joins fail within a few kilobytes.
const PROBE_BUF: usize = 16 * 1024;
/// Compressed image data is close to random. Zeroed clusters and text
/// never are, and would otherwise pass as image data, since they contain no
/// bytes the JPEG walk would object to.
const MIN_ENTROPY_BITS: f64 = 6.0;

/// Tries to rebuild the broken JPEG starting at `start` in `view`.
///
/// `claimed` lists the ranges (in `view` offsets, sorted) that belong to
/// files already recovered. Returns the two pieces, in `view` offsets.
pub fn repair_jpeg(
    view: &dyn Source,
    start: u64,
    claimed: &[(u64, u64)],
    cluster: u64,
) -> Option<(Vec<Extent>, Found)> {
    let len = view.len();
    let limit = len.min(start + MAX_LEN);
    let fail = match jpeg::walk(&mut Reader::new(view, 0), start, limit) {
        Ok(_) => return None,
        Err(pos) => pos,
    };
    if fail < start + cluster {
        return None;
    }

    // Where the first piece may end: where another recovered file begins,
    // if that is before the break; then the cluster boundaries just before
    // the break.
    let mut splits = Vec::new();
    if let Some(&(b, _)) = claimed.iter().find(|&&(b, _)| b > start) {
        if b <= fail {
            splits.push(b);
        }
    }
    let last = start + (fail - start) / cluster * cluster;
    splits.extend(
        (0..MAX_SPLITS as u64)
            .map(|k| last.saturating_sub(k * cluster))
            .filter(|&s| s > start),
    );
    splits.dedup();

    // The second piece starts either soon after the split or soon after
    // one of the files in the way ends. Trying only those places keeps the
    // number of reads small on a slow card.
    let mut tries = 0;
    let mut sample = vec![0u8; cluster.min(4096) as usize];
    for &split in splits.iter().take(MAX_SPLITS) {
        let first = Extent::new(start, split - start);
        // The first piece is read once and reused for every attempt.
        let mut head = vec![0u8; first.len as usize];
        if view.read_at(first.offset, &mut head).unwrap_or(0) < head.len() {
            continue;
        }
        let window_end = len.min(split + WINDOW);
        let mut candidates: Vec<u64> = (1..=NEARBY_CLUSTERS).map(|k| split + k * cluster).collect();
        for &(_, end) in claimed
            .iter()
            .filter(|&&(_, e)| e > split && e < window_end)
        {
            let from = align_up(end, start, cluster);
            candidates.extend((0..NEARBY_CLUSTERS).map(|k| from + k * cluster));
        }
        candidates.sort_unstable();
        candidates.dedup();
        for c in candidates {
            if c >= window_end || tries >= MAX_TRIES {
                break;
            }
            if claimed.iter().any(|&(b, e)| b <= c && c < e) {
                continue;
            }
            let n = view.read_at(c, &mut sample).unwrap_or(0);
            if n == 0 || entropy(&sample[..n]) < MIN_ENTROPY_BITS {
                continue;
            }
            tries += 1;
            let joined = Joined {
                head: &head,
                tail: view,
                tail_at: c,
                len: first.len + (len - c),
            };
            let joined_limit = joined.len.min(MAX_LEN);
            if let Ok(found) = jpeg::walk(
                &mut Reader::with_capacity(&joined, 0, PROBE_BUF),
                0,
                joined_limit,
            ) {
                if found.len > first.len {
                    let second = Extent::new(c, found.len - first.len);
                    if !claimed
                        .iter()
                        .any(|&(b, e)| b < second.end() && second.offset < e)
                    {
                        return Some((vec![first, second], found));
                    }
                }
            }
        }
    }
    None
}

/// Clusters tried after each place a second piece may start.
const NEARBY_CLUSTERS: u64 = 64;

/// A candidate rejoined file: the first piece, already in memory, then the
/// source from where the second piece may start.
struct Joined<'a> {
    head: &'a [u8],
    tail: &'a dyn Source,
    tail_at: u64,
    len: u64,
}

impl Source for Joined<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let head_len = self.head.len() as u64;
        let mut done = 0;
        if offset < head_len {
            let n = buf.len().min((head_len - offset) as usize);
            buf[..n].copy_from_slice(&self.head[offset as usize..offset as usize + n]);
            done = n;
        }
        if done < buf.len() && offset + (done as u64) < self.len {
            let at = self.tail_at + (offset + done as u64 - head_len);
            done += self.tail.read_at(at, &mut buf[done..])?;
        }
        Ok(done)
    }
}

/// How far past a video's start its index is looked for.
const VIDEO_WINDOW: u64 = 16 << 30;

/// Tries to rebuild a broken MP4/MOV video starting at `start` in `view`.
///
/// Cameras write a video's frames first and its index (`moov`) last, so
/// when the video was split, the second piece ends with the index. The
/// index is found by its contents: every sample it lists must lie inside
/// this video's media data. It then gives the exact place of every frame,
/// and since H.264/H.265 frames begin with recognisable headers, each frame
/// can be tested in the first piece and in the second. The split lies
/// where frames stop checking out in the first and start in the second.
pub fn repair_mp4(
    view: &dyn Source,
    start: u64,
    claimed: &[(u64, u64)],
    indexes: &[u64],
    cluster: u64,
) -> Option<(Vec<Extent>, Found)> {
    let len = view.len();
    let mut r = Reader::new(view, 0);

    // The header: boxes up to the media data.
    let mut pos = start;
    let mut mdat = None;
    for i in 0..16 {
        r.seek(pos);
        let size32 = r.u32be()?;
        let typ = r.array::<4>()?;
        if i == 0 && &typ != b"ftyp" {
            return None;
        }
        let size = if size32 == 1 {
            r.u64be()?
        } else {
            u64::from(size32)
        };
        if size < 8 {
            return None;
        }
        match &typ {
            // Index-first files have no index left to find at the end.
            b"moov" => return None,
            b"mdat" => {
                mdat = Some((pos - start, size));
                break;
            }
            // Box sizes are untrusted 64-bit values.
            _ => pos = pos.checked_add(size)?,
        }
    }
    let (mdat_rel, mdat_size) = mdat?;
    let data_end = mdat_rel.checked_add(mdat_size)?;

    // The index lies after the media data, at an offset the scan noted
    // while reading the card, so nothing is read again to find it.
    let lo = start.checked_add(data_end)?;
    let hi = len.min(lo.saturating_add(VIDEO_WINDOW));
    let from = indexes.partition_point(|&i| i < lo);
    for &index in indexes[from..]
        .iter()
        .take_while(|&&i| i < hi)
        .take(MAX_INDEX_TRIES)
    {
        r.seek(index);
        let size = u64::from(r.u32be().unwrap_or(0));
        if size >= 16 && index + size <= len {
            if let Some(found) = try_index(
                view, &mut r, start, index, size, mdat_rel, data_end, claimed, cluster,
            ) {
                return Some(found);
            }
        }
    }
    None
}

/// Candidate indexes examined per broken video.
const MAX_INDEX_TRIES: usize = 256;

#[allow(clippy::too_many_arguments)]
fn try_index(
    view: &dyn Source,
    r: &mut Reader,
    start: u64,
    index: u64,
    index_size: u64,
    mdat_rel: u64,
    data_end: u64,
    claimed: &[(u64, u64)],
    cluster: u64,
) -> Option<(Vec<Extent>, Found)> {
    let tracks = bmff::tracks(r, index, index_size);
    // The index must describe this video's media data and nothing else.
    let all = tracks.iter().flat_map(|t| &t.samples);
    let inside = all
        .clone()
        .all(|&(o, n)| o >= mdat_rel + 8 && o.saturating_add(u64::from(n)) <= data_end);
    if tracks.is_empty() || !inside || all.count() == 0 {
        return None;
    }
    let video = tracks
        .iter()
        .find(|t| matches!(&t.codec, b"avc1" | b"avc3" | b"hvc1" | b"hev1"))?;
    let hevc = matches!(&video.codec, b"hvc1" | b"hev1");
    let mut samples = video.samples.clone();
    samples.sort_unstable();

    // The index follows the media data directly, which fixes where the
    // second piece sits: file offset `o` is at `base + o` in it.
    let base = index.checked_sub(data_end)?;
    if base < start {
        return None;
    }
    let first_ok = |o: u64, n: u32| frame_starts(view, start + o, n, hevc);
    let k = samples.iter().position(|&(o, n)| !first_ok(o, n))?;
    if k == 0 {
        return None;
    }
    let rest = &samples[k..];
    let good = rest
        .iter()
        .filter(|&&(o, n)| frame_starts(view, base + o, n, hevc))
        .count();
    if good * 10 < rest.len() * 9 {
        return None;
    }

    // The split is a cluster boundary after the last frame that checks out
    // in place and at or before the first that does not. Where another
    // recovered file begins in that range, that is it.
    let (lo, hi) = (samples[k - 1].0, samples[k].0);
    let next_claimed = claimed
        .iter()
        .map(|&(b, _)| b)
        .find(|&b| b > start)
        .map(|b| b - start);
    let split = match next_claimed {
        Some(c) if c > lo && c <= hi => c,
        _ => {
            let s = hi / cluster * cluster;
            if s <= lo {
                return None;
            }
            s
        }
    };

    let total = data_end + index_size;
    let first = Extent::new(start, split);
    let second = Extent::new(base + split, total - split);
    if second.offset < first.end()
        || claimed
            .iter()
            .any(|&(b, e)| b < second.end() && second.offset < e)
    {
        return None;
    }
    let joined = MappedSource::new(view, vec![first, second]);
    let found = bmff::Bmff.carve(&mut Reader::new(&joined, 0), 0, joined.len())?;
    (found.len == total).then_some((vec![first, second], found))
}

/// Whether an H.264/H.265 frame (length-prefixed NAL units) starts at `at`.
fn frame_starts(view: &dyn Source, at: u64, size: u32, hevc: bool) -> bool {
    let mut h = [0u8; 6];
    if view.read_at(at, &mut h).unwrap_or(0) < 6 {
        return false;
    }
    let nal_len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    if nal_len == 0 || nal_len > size.saturating_sub(4) || h[4] & 0x80 != 0 {
        return false;
    }
    if hevc {
        (h[4] >> 1) & 0x3F <= 40 && h[5] & 7 != 0
    } else {
        matches!(h[4] & 0x1F, 1..=23)
    }
}

/// Rounds `pos` up to the next cluster boundary, counting from `origin`.
fn align_up(pos: u64, origin: u64, cluster: u64) -> u64 {
    if pos <= origin {
        return origin;
    }
    origin + (pos - origin).div_ceil(cluster) * cluster
}

/// Shannon entropy in bits per byte.
fn entropy(data: &[u8]) -> f64 {
    let mut counts = [0u32; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let n = data.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::jpeg::tests::sample;

    /// Pseudo-random bytes standing in for another file's compressed data.
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn rejoins_photo_split_around_other_files() {
        const C: usize = 4096;
        let photo = sample(60_000);
        // Piece one fills 5 clusters, then another file (3 clusters of
        // random data, claimed), a zeroed cluster and a text cluster, then
        // piece two.
        let piece1 = &photo[..5 * C];
        let piece2 = &photo[5 * C..];
        let mut card = vec![0u8; 2 * C];
        let start = card.len() as u64;
        card.extend(piece1);
        let other = card.len() as u64;
        card.extend(noise(3 * C, 7));
        card.extend(vec![0u8; C]);
        card.extend(b"shopping list\n".iter().cycle().take(C));
        let second = card.len() as u64;
        card.extend(piece2);
        card.resize(card.len().div_ceil(C) * C + 4 * C, 0);

        let claimed = [(other, other + 3 * C as u64)];
        let (pieces, found) = repair_jpeg(&card, start, &claimed, C as u64).expect("repaired");
        assert_eq!(found.len, photo.len() as u64);
        assert_eq!(
            pieces,
            vec![
                Extent::new(start, 5 * C as u64),
                Extent::new(second, piece2.len() as u64)
            ]
        );
    }

    #[test]
    fn leaves_intact_photos_alone() {
        let photo = sample(10_000);
        assert!(repair_jpeg(&photo, 0, &[], 4096).is_none());
    }
}

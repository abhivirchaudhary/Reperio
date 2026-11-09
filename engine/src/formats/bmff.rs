//! ISO base media files: MP4, MOV, HEIC, AVIF, CR3, 3GP, M4A.
//!
//! These are a sequence of top-level boxes, each starting with its size, so
//! the file ends after the last valid box. Most cameras write the `moov`
//! index after the `mdat` media data; walking the boxes handles either order.

use super::{DateTime, Format, Found, Kind};
use crate::reader::Reader;

pub struct Bmff;

/// Box types allowed at the top level of a file. `ftyp` is deliberately
/// absent: it only appears first, and seeing it again means the next file
/// has begun.
const TOP_LEVEL: &[&[u8; 4]] = &[
    b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pnot", b"PICT", b"styp",
    b"sidx", b"moof", b"mfra", b"pdin", b"prft", b"emsg", b"meco", b"udta",
];

/// QuickTime files from older cameras can begin without an `ftyp` box.
const QT_FIRST: &[&[u8; 4]] = &[b"moov", b"mdat", b"wide"];

/// Seconds between 1904-01-01 (the QuickTime epoch) and 1970-01-01.
const QT_EPOCH_OFFSET: i64 = 2_082_844_800;

impl Format for Bmff {
    fn name(&self) -> &'static str {
        "mp4"
    }

    fn max_len(&self) -> u64 {
        256 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let size = u32::from_be_bytes(head[..4].try_into().unwrap());
        let typ: &[u8; 4] = head[4..8].try_into().unwrap();
        if typ == b"ftyp" {
            return (16..=1024).contains(&size)
                && head[8..12]
                    .iter()
                    .all(|b| b.is_ascii_graphic() || *b == b' ');
        }
        QT_FIRST.contains(&typ) && (size == 1 || size >= 8)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        let mut pos = start;
        let mut brand = None;
        let (mut has_moov, mut has_mdat, mut has_meta) = (false, false, false);
        let mut taken = None;

        loop {
            if pos + 8 > limit {
                break;
            }
            r.seek(pos);
            let Some(size32) = r.u32be() else { break };
            let Some(typ) = r.array::<4>() else { break };
            let first = pos == start;
            if first && &typ == b"ftyp" {
                brand = r.array::<4>();
            } else if !TOP_LEVEL.contains(&&typ) {
                break;
            }
            let size = match size32 {
                // A box running "to end of file" gives no length to carve.
                0 => return None,
                1 => r.u64be()?,
                n => u64::from(n),
            };
            // 64-bit box sizes are untrusted; compare without adding.
            if size < 8 || size > limit - pos {
                break;
            }
            match &typ {
                b"moov" => {
                    has_moov = true;
                    taken = taken.or_else(|| movie_date(r, pos, size));
                }
                b"mdat" => has_mdat = true,
                b"meta" => has_meta = true,
                _ => {}
            }
            pos += size;
        }

        let (ext, kind) = match brand {
            Some(b) => classify(&b),
            None if has_moov && has_mdat => ("mov", Kind::Video),
            None => return None,
        };
        let complete = match kind {
            Kind::Photo => has_meta && has_mdat,
            Kind::Raw => has_moov,
            _ => has_moov && has_mdat,
        };
        complete.then(|| Found {
            len: pos - start,
            ext,
            kind,
            taken,
        })
    }
}

fn classify(brand: &[u8; 4]) -> (&'static str, Kind) {
    match brand {
        b"qt  " => ("mov", Kind::Video),
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => {
            ("heic", Kind::Photo)
        }
        b"avif" | b"avis" => ("avif", Kind::Photo),
        b"crx " => ("cr3", Kind::Raw),
        b"M4A " | b"M4B " => ("m4a", Kind::Audio),
        [b'3', b'g', ..] => ("3gp", Kind::Video),
        _ => ("mp4", Kind::Video),
    }
}

/// Reads the creation time from the `mvhd` box inside a `moov` box.
fn movie_date(r: &mut Reader, moov: u64, moov_size: u64) -> Option<DateTime> {
    let end = moov + moov_size;
    let mut pos = moov + 8;
    for _ in 0..64 {
        if pos + 8 > end {
            return None;
        }
        r.seek(pos);
        let size = u64::from(r.u32be()?);
        let typ = r.array::<4>()?;
        if &typ == b"mvhd" {
            let version = r.u8()?;
            r.skip(3);
            let created = if version == 1 {
                r.u64be()?
            } else {
                u64::from(r.u32be()?)
            };
            return DateTime::from_unix(i64::try_from(created).ok()? - QT_EPOCH_OFFSET);
        }
        if size < 8 {
            return None;
        }
        pos += size;
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend(typ);
        v.extend(body);
        v
    }

    /// A minimal camera-style MOV: ftyp, mdat, then moov with an mvhd.
    pub fn sample(media_len: usize) -> Vec<u8> {
        let mut v = bx(b"ftyp", b"qt  \0\0\x02\0qt  ");
        v.extend(bx(b"mdat", &vec![0x5A; media_len]));
        let mut mvhd = vec![0, 0, 0, 0];
        // 2015-06-01 12:00:00 UTC in QuickTime time.
        mvhd.extend(((1_433_160_000i64 + QT_EPOCH_OFFSET) as u32).to_be_bytes());
        mvhd.extend([0; 92]);
        v.extend(bx(b"moov", &bx(b"mvhd", &mvhd)));
        v
    }

    #[test]
    fn carves_mov_with_trailing_index() {
        let mut data = sample(100_000);
        let len = data.len() as u64;
        // The next file's header must not be absorbed.
        data.extend(bx(b"ftyp", b"isom\0\0\0\0isom"));
        assert!(Bmff.matches(&data[..512]));
        let f = Bmff
            .carve(&mut Reader::new(&data, 0), 0, data.len() as u64)
            .unwrap();
        assert_eq!((f.len, f.ext), (len, "mov"));
        assert_eq!(f.taken.unwrap().to_string(), "2015-06-01 12.00.00");
    }

    #[test]
    fn rejects_mp4_missing_index() {
        let mut data = bx(b"ftyp", b"isom\0\0\0\0isom");
        data.extend(bx(b"mdat", &[0; 1000]));
        assert!(Bmff
            .carve(&mut Reader::new(&data, 0), 0, data.len() as u64)
            .is_none());
    }
}

/// A track's media samples, from a `moov` box's sample tables.
#[derive(Debug, Default)]
pub(crate) struct Track {
    /// The sample entry type: `avc1`, `hvc1`, `mp4a`...
    pub codec: [u8; 4],
    /// File-relative offset and size of every sample, in order.
    pub samples: Vec<(u64, u32)>,
}

/// Most samples a track may list; bounds the work on a corrupt table.
const MAX_SAMPLES: usize = 20_000_000;

/// Reads every track's sample layout from the `moov` box at `pos`.
pub(crate) fn tracks(r: &mut Reader, pos: u64, size: u64) -> Vec<Track> {
    let mut out = Vec::new();
    for (trak, trak_size) in children(r, pos + 8, pos + size, b"trak") {
        let Some((mdia, mdia_size)) = children(r, trak + 8, trak + trak_size, b"mdia")
            .into_iter()
            .next()
        else {
            continue;
        };
        let Some((minf, minf_size)) = children(r, mdia + 8, mdia + mdia_size, b"minf")
            .into_iter()
            .next()
        else {
            continue;
        };
        let Some((stbl, stbl_size)) = children(r, minf + 8, minf + minf_size, b"stbl")
            .into_iter()
            .next()
        else {
            continue;
        };
        if let Some(t) = sample_table(r, stbl + 8, stbl + stbl_size) {
            out.push(t);
        }
    }
    out
}

/// Child boxes of type `kind` between `pos` and `end`.
fn children(r: &mut Reader, mut pos: u64, end: u64, kind: &[u8; 4]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for _ in 0..4096 {
        if pos + 8 > end {
            break;
        }
        r.seek(pos);
        let (Some(size), Some(typ)) = (r.u32be(), r.array::<4>()) else {
            break;
        };
        let size = match size {
            1 => match r.u64be() {
                Some(s) => s,
                None => break,
            },
            0 => end - pos,
            n => u64::from(n),
        };
        if size < 8 || size > end - pos {
            break;
        }
        if &typ == kind {
            out.push((pos, size));
        }
        pos += size;
    }
    out
}

fn sample_table(r: &mut Reader, pos: u64, end: u64) -> Option<Track> {
    let find = |r: &mut Reader, kind: &[u8; 4]| children(r, pos, end, kind).into_iter().next();
    let mut track = Track::default();
    // Tables are reserved for no more entries than their box can hold (at
    // least four bytes each), not for the count a corrupt header claims.
    let room = |(_, size): (u64, u64)| (size / 4) as usize;

    let (stsd, _) = find(r, b"stsd")?;
    r.seek(stsd + 20);
    track.codec = r.array()?;

    let (offsets, wide) = match find(r, b"stco") {
        Some(b) => (b, false),
        None => (find(r, b"co64")?, true),
    };
    r.seek(offsets.0 + 12);
    let chunks = r.u32be()? as usize;
    if chunks > MAX_SAMPLES {
        return None;
    }
    let mut chunk_offsets = Vec::with_capacity(chunks.min(room(offsets)));
    for _ in 0..chunks {
        chunk_offsets.push(if wide {
            r.u64be()?
        } else {
            u64::from(r.u32be()?)
        });
    }

    let stsc = find(r, b"stsc")?;
    r.seek(stsc.0 + 12);
    let runs = r.u32be()? as usize;
    if runs > MAX_SAMPLES {
        return None;
    }
    let mut per_chunk = Vec::with_capacity(runs.min(room(stsc)));
    for _ in 0..runs {
        let first = r.u32be()?;
        let count = r.u32be()?;
        r.skip(4);
        per_chunk.push((first, count));
    }

    let stsz = find(r, b"stsz")?;
    r.seek(stsz.0 + 12);
    let fixed = r.u32be()?;
    let count = r.u32be()? as usize;
    if count > MAX_SAMPLES {
        return None;
    }
    let mut sizes = Vec::with_capacity(count.min(room(stsz)));
    for _ in 0..count {
        sizes.push(if fixed != 0 { fixed } else { r.u32be()? });
    }

    // Walk chunks, giving each its run's sample count.
    let mut sample = 0;
    let mut run = 0;
    for (i, &chunk) in chunk_offsets.iter().enumerate() {
        let number = i as u32 + 1;
        while run + 1 < per_chunk.len() && per_chunk[run + 1].0 <= number {
            run += 1;
        }
        let n = per_chunk
            .get(run)
            .filter(|r| r.0 <= number)
            .map_or(0, |r| r.1);
        let mut at = chunk;
        for _ in 0..n {
            let Some(&size) = sizes.get(sample) else {
                break;
            };
            track.samples.push((at, size));
            at = at.saturating_add(u64::from(size));
            sample += 1;
        }
    }
    Some(track)
}

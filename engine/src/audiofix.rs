//! Finding the sound of a video whose index is lost.
//!
//! Cameras write sound in chunks between the video frames. Once the video
//! frames are found (see [`crate::videofix`]), what lies between them is
//! sound and metadata. Uncompressed sound divides into samples by size.
//! AAC frames have no header or length to find them by, but each one
//! starts with a channel element whose first fields are tightly
//! constrained (element type and tag, window sequence, a band count
//! within the table for the sample rate, section data that adds up to it
//! exactly) and ends with an END element and zero padding. A chunk counts
//! as sound only if it divides end to end into such frames; anything else
//! (telemetry, timecode, leftovers) is left out.

/// Shortest AAC frame (a silent stereo frame is about 6 bytes).
const MIN_FRAME: usize = 4;
/// Longest AAC frame per channel: 6144 bits.
const MAX_FRAME_PER_CHANNEL: usize = 768;
/// Largest chunk examined as sound.
pub const MAX_CHUNK: usize = 8 << 20;

struct BitReader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.d.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }
}

/// Window layout of one channel's data.
struct IcsInfo {
    long: bool,
    max_sfb: u32,
    groups: u32,
}

fn ics_info(b: &mut BitReader) -> Option<IcsInfo> {
    if b.bits(1)? != 0 {
        return None; // reserved
    }
    let seq = b.bits(2)?;
    b.bits(1)?; // window shape
    if seq == 2 {
        let max_sfb = b.bits(4)?;
        let grouping = b.bits(7)?;
        if max_sfb > 15 {
            return None;
        }
        Some(IcsInfo {
            long: false,
            max_sfb,
            groups: 1 + (7 - grouping.count_ones()),
        })
    } else {
        let max_sfb = b.bits(6)?;
        // No sample rate has more than 51 long-window bands, and AAC-LC
        // has no prediction.
        if max_sfb > 51 || b.bits(1)? != 0 {
            return None;
        }
        Some(IcsInfo {
            long: true,
            max_sfb,
            groups: 1,
        })
    }
}

/// Section data: codebook runs that must cover the bands exactly.
fn section_data(b: &mut BitReader, info: &IcsInfo) -> Option<()> {
    let (bits, esc) = if info.long { (5, 31) } else { (3, 7) };
    for _ in 0..info.groups {
        let mut k = 0;
        while k < info.max_sfb {
            if b.bits(4)? == 12 {
                return None; // reserved codebook
            }
            let mut len = 0;
            loop {
                let v = b.bits(bits)?;
                len += v;
                if v != esc {
                    break;
                }
            }
            if len == 0 {
                return None;
            }
            k += len;
            if k > info.max_sfb {
                return None;
            }
        }
    }
    Some(())
}

/// Whether an AAC-LC frame for `channels` channels plausibly starts at the
/// beginning of `d`.
pub fn frame_starts(d: &[u8], channels: u16) -> bool {
    let mut b = BitReader { d, pos: 0 };
    (|| {
        let mut id = b.bits(3)?;
        // Encoders put a fill element with their name before the first
        // frame's data ("Lavc...").
        for _ in 0..2 {
            if id != 6 {
                break;
            }
            let mut count = b.bits(4)?;
            if count == 15 {
                count += b.bits(8)?.saturating_sub(1);
            }
            b.pos += count as usize * 8;
            id = b.bits(3)?;
        }
        if b.bits(4)? != 0 {
            return None; // element instance tag
        }
        match (id, channels) {
            (0, 1) => {
                b.bits(8)?; // global gain
                let info = ics_info(&mut b)?;
                section_data(&mut b, &info)
            }
            (1, 2) => {
                let common = b.bits(1)? == 1;
                let shared = if common {
                    let info = ics_info(&mut b)?;
                    let ms = b.bits(2)?;
                    if ms == 3 {
                        return None;
                    }
                    if ms == 1 {
                        for _ in 0..info.groups * info.max_sfb {
                            b.bits(1)?;
                        }
                    }
                    Some(info)
                } else {
                    None
                };
                b.bits(8)?; // global gain
                let info = match shared {
                    Some(i) => i,
                    None => ics_info(&mut b)?,
                };
                section_data(&mut b, &info)
            }
            _ => None,
        }
    })()
    .is_some()
}

/// Whether a frame can end just before `end`: the END element (`111`)
/// followed by padding, which encoders fill with zero bits and bytes.
fn frame_ends(d: &[u8], end: usize) -> bool {
    let Some(last) = d[..end.min(d.len())].iter().rposition(|&b| b != 0) else {
        return false;
    };
    let prev = if last >= 1 { d[last - 1] } else { 0 };
    // The three END bits, counted up from the lowest set bit, may reach
    // into the byte before.
    let word = (u32::from(prev) << 8) | u32::from(d[last]);
    (word >> d[last].trailing_zeros()) & 7 == 7
}

/// Divides a chunk into AAC frames end to end. Returns the frame sizes,
/// or `None` if the chunk is not AAC sound for `channels` channels.
///
/// `usual` is the typical frame size of the recording, once known (0
/// before): where a chunk can be divided in more than one way, the way
/// whose frames are closest to it is taken, since a stray run of bytes
/// inside a frame can look like a frame boundary.
pub fn split_aac(chunk: &[u8], channels: u16, usual: usize) -> Option<Vec<u32>> {
    let n = chunk.len();
    if !(MIN_FRAME..=MAX_CHUNK).contains(&n)
        || !frame_starts(chunk, channels)
        || !frame_ends(chunk, n)
    {
        return None;
    }
    // Places a frame may start: after a possible END, at a channel element
    // (or a fill element before one).
    let mut starts = vec![0usize];
    for p in 1..n {
        // A mono frame can begin with a zero byte; a run of zeros is
        // padding, which belongs to the frame before it.
        if (chunk[p] != 0 || chunk[p - 1] != 0)
            && matches!(chunk[p] >> 5, 0 | 1 | 6)
            && frame_starts(&chunk[p..], channels)
            && frame_ends(chunk, p)
        {
            starts.push(p);
        }
    }
    starts.push(n);
    let max = MAX_FRAME_PER_CHANNEL * usize::from(channels.max(1)) + 64;
    // From the end backwards: the cheapest way of dividing the rest of
    // the chunk, taking the earliest boundary among equals.
    let last = starts.len() - 1;
    let mut next = vec![usize::MAX; starts.len()];
    let mut cost = vec![u64::MAX; starts.len()];
    next[last] = last;
    cost[last] = 0;
    for i in (0..last).rev() {
        let p = starts[i];
        for j in i + 1..starts.len() {
            let len = starts[j] - p;
            if len > max {
                break;
            }
            if len < MIN_FRAME || cost[j] == u64::MAX {
                continue;
            }
            let c = cost[j] + off_size(len, usual);
            if c < cost[i] {
                cost[i] = c;
                next[i] = j;
            }
        }
    }
    if next[0] == usize::MAX {
        return None;
    }
    let mut sizes = Vec::new();
    let mut i = 0;
    while i != last {
        let j = next[i];
        sizes.push((starts[j] - starts[i]) as u32);
        i = j;
    }
    Some(sizes)
}

/// How far a frame of `len` bytes is from the usual size, capped so one
/// odd frame doesn't outweigh a wrong boundary.
fn off_size(len: usize, usual: usize) -> u64 {
    let d = len.abs_diff(usual) as u64;
    (d * d).min((usual as u64 * usual as u64) / 4)
}

const RATES: [u32; 12] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
];

/// The sample rate that `frames` AAC frames lasting `seconds` were
/// recorded at, if it is clearly one of the usual rates; otherwise
/// `fallback`.
pub fn measure_rate(frames: usize, seconds: f64, fallback: u32) -> u32 {
    if seconds < 2.0 || frames == 0 {
        return fallback;
    }
    let est = frames as f64 * 1024.0 / seconds;
    let near = |r: u32| (f64::from(r) - est).abs() / f64::from(r);
    RATES
        .into_iter()
        .filter(|&r| r <= 48000)
        .min_by(|&a, &b| near(a).total_cmp(&near(b)))
        .filter(|&r| near(r) < 0.03)
        .unwrap_or(fallback)
}

/// Channel count and sample rate from an `mp4a` sample entry's
/// AudioSpecificConfig.
pub fn aac_config(entry: &[u8]) -> Option<(u16, u32)> {
    let at = memchr::memmem::find(entry, b"esds")?;
    let d = entry.get(at + 8..)?;
    // DecoderSpecificInfo: tag 5, a length (7 bits per byte, the high bit
    // saying more follow), then the config.
    let c = d
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == 5)
        .find_map(|(i, _)| {
            let mut j = i + 1;
            let mut len = 0usize;
            while *d.get(j)? & 0x80 != 0 && j < i + 5 {
                len = (len << 7) | usize::from(d[j] & 0x7F);
                j += 1;
            }
            len = (len << 7) | usize::from(*d.get(j)? & 0x7F);
            let c = d.get(j + 1..j + 3)?;
            (len >= 2 && c[0] >> 3 == 2).then_some(c)
        })?;
    let object = c[0] >> 3;
    let rate_idx = ((c[0] & 7) << 1) | (c[1] >> 7);
    let channels = (c[1] >> 3) & 0xF;
    let rate = *RATES.get(usize::from(rate_idx))?;
    (object == 2 && (1..=2).contains(&channels)).then_some((u16::from(channels), rate))
}

/// An `mp4a` sample entry box for AAC-LC.
pub fn mp4a_entry(rate: u32, channels: u16) -> Vec<u8> {
    let index = RATES.iter().position(|&r| r == rate).unwrap_or(3) as u16;
    let asc = (2u16 << 11) | (index << 7) | (channels << 3);
    let mut dsi = vec![0x05, 2];
    dsi.extend(asc.to_be_bytes());
    let mut dcd = vec![0x04, 13 + dsi.len() as u8, 0x40, 0x15, 0, 0x18, 0];
    dcd.extend(0u32.to_be_bytes()); // bitrates unknown
    dcd.extend(0u32.to_be_bytes());
    dcd.extend(&dsi);
    let mut es = vec![0x03, (3 + dcd.len() + 3) as u8, 0, 2, 0];
    es.extend(&dcd);
    es.extend([0x06, 1, 2]);
    let mut esds = Vec::new();
    esds.extend(((12 + es.len()) as u32).to_be_bytes());
    esds.extend(b"esds");
    esds.extend([0, 0, 0, 0]);
    esds.extend(&es);
    sound_entry(b"mp4a", rate, channels, &esds)
}

/// A QuickTime `twos` (16-bit big-endian) sample entry box.
pub fn twos_entry(rate: u32, channels: u16) -> Vec<u8> {
    sound_entry(b"twos", rate, channels, &[])
}

fn sound_entry(typ: &[u8; 4], rate: u32, channels: u16, children: &[u8]) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend(((36 + children.len()) as u32).to_be_bytes());
    e.extend(typ);
    e.extend([0; 6]);
    e.extend(1u16.to_be_bytes());
    e.extend([0; 8]);
    e.extend(channels.to_be_bytes());
    e.extend(16u16.to_be_bytes());
    e.extend([0; 4]);
    e.extend((rate.min(65535) << 16).to_be_bytes());
    e.extend(children);
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits, most significant first.
    struct W(Vec<u8>, usize);
    impl W {
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                if self.1.is_multiple_of(8) {
                    self.0.push(0);
                }
                let last = self.0.len() - 1;
                self.0[last] |= (((v >> i) & 1) as u8) << (7 - self.1 % 8);
                self.1 += 1;
            }
        }
    }

    /// A stereo frame: CPE, common window, long window with `bands`
    /// bands in one section, some filler, then END.
    fn frame(bands: u32, filler: usize) -> Vec<u8> {
        let mut w = W(Vec::new(), 0);
        w.put(1, 3);
        w.put(0, 4);
        w.put(1, 1); // common window
        w.put(0, 1);
        w.put(0, 2);
        w.put(0, 1);
        w.put(bands, 6);
        w.put(0, 1);
        w.put(0, 2); // no M/S
        w.put(100, 8);
        if bands > 0 {
            w.put(11, 4);
            w.put(bands, 5);
        }
        for i in 0..filler {
            // Nothing that looks like a frame start.
            w.put(0x5A ^ (i as u32 & 0x0F), 8);
        }
        w.put(7, 3);
        w.0
    }

    #[test]
    fn splits_aac_chunks() {
        let frames = [frame(20, 40), frame(25, 300), frame(0, 3), frame(30, 120)];
        let chunk: Vec<u8> = frames.concat();
        let want: Vec<u32> = frames.iter().map(|f| f.len() as u32).collect();
        assert_eq!(split_aac(&chunk, 2, 0), Some(want));
        assert!(split_aac(b"DEVC\0\0\0\x01junkjunkjunk", 2, 0).is_none());
        // Trailing bytes that cannot end a frame.
        let mut longer = chunk.clone();
        longer.extend([0x10, 0x00, 0x01]);
        assert!(split_aac(&longer, 2, 0).is_none());
    }

    #[test]
    fn writes_and_reads_configs() {
        let e = mp4a_entry(44100, 2);
        assert_eq!(aac_config(&e), Some((2, 44100)));
        // FFmpeg writes its descriptor lengths in four bytes.
        let ffmpeg = b"....esds\0\0\0\0\x03\x80\x80\x80\x22\0\x02\0\x04\x80\x80\x80\x14\x40\x15\0\0\0\0\0\0\0\0\0\0\0\0\0\x05\x80\x80\x80\x02\x11\x90\x06\x80\x80\x80\x01\x02";
        assert_eq!(aac_config(ffmpeg), Some((2, 48000)));
        assert_eq!(measure_rate(4687, 100.0, 44100), 48000);
        assert_eq!(measure_rate(10, 1.0, 44100), 44100);
    }
}

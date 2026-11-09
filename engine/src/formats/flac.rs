//! FLAC audio: `fLaC`, metadata blocks (the first, STREAMINFO, gives the
//! stream's parameters and total sample count), then audio frames. Frames
//! carry no length, so each one is parsed down to its subframes and
//! residuals to find where it ends; its CRC-16 must then match, and its
//! header (with its own CRC-8) must continue the frame or sample
//! numbering. The file ends when the frames add up to the total sample
//! count, or, for a stream written without one, after the last valid
//! frame.

use super::crc::{crc16_flac, crc8_flac};
use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Flac;

/// Largest frame window tried before giving up on a candidate.
const MAX_WINDOW: usize = 16 << 20;

impl Format for Flac {
    fn name(&self) -> &'static str {
        "flac"
    }

    fn max_len(&self) -> u64 {
        2 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        &head[..4] == b"fLaC" && head[4] & 0x7F == 0 && head[5..8] == [0, 0, 34]
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 4);
        let mut info = None;
        for _ in 0..1024 {
            let h = r.array::<4>()?;
            let typ = h[0] & 0x7F;
            let len = u64::from(u32::from_be_bytes([0, h[1], h[2], h[3]]));
            match (typ, &info) {
                (0, None) if len == 34 => info = Some(StreamInfo::parse(&r.array::<34>()?)?),
                (1..=126, Some(_)) => r.skip(len),
                _ => return None,
            }
            if h[0] & 0x80 != 0 {
                break;
            }
        }
        let info = info?;
        let mut pos = r.pos();
        if pos >= limit {
            return None;
        }

        let mut state = Stream {
            info,
            frames: 0,
            samples: 0,
            variable: None,
        };
        let mut window = if info.max_frame > 0 {
            info.max_frame as usize + 64
        } else {
            64 << 10
        };
        let mut buf = Vec::new();
        loop {
            if info.total > 0 && state.samples >= info.total {
                break;
            }
            let avail = usize::try_from(limit - pos).unwrap_or(usize::MAX);
            let n = window.min(avail);
            buf.resize(n, 0);
            r.seek(pos);
            if n == 0 || r.read_exact(&mut buf).is_none() {
                return None;
            }
            match state.frame(&buf) {
                Ok(len) => pos += len as u64,
                // The window cut the frame short: try a larger one.
                Err(Short) if n < avail && window < MAX_WINDOW => {
                    window *= 4;
                    continue;
                }
                // Not a frame. Without a sample count, this is the end.
                Err(_) if info.total == 0 && state.frames > 0 => break,
                Err(_) => return None,
            }
        }
        (pos <= limit).then_some(Found {
            len: pos - start,
            ext: "flac",
            kind: Kind::Audio,
            taken: None,
        })
    }
}

#[derive(Clone, Copy)]
struct StreamInfo {
    max_block: u32,
    max_frame: u32,
    rate: u32,
    channels: u32,
    bps: u32,
    total: u64,
}

impl StreamInfo {
    fn parse(b: &[u8; 34]) -> Option<Self> {
        let min_block = u32::from(u16::from_be_bytes([b[0], b[1]]));
        let max_block = u32::from(u16::from_be_bytes([b[2], b[3]]));
        let min_frame = u32::from_be_bytes([0, b[4], b[5], b[6]]);
        let max_frame = u32::from_be_bytes([0, b[7], b[8], b[9]]);
        let packed = u64::from_be_bytes(b[10..18].try_into().ok()?);
        let info = Self {
            max_block,
            max_frame,
            rate: (packed >> 44) as u32,
            channels: ((packed >> 41) & 7) as u32 + 1,
            bps: ((packed >> 36) & 31) as u32 + 1,
            total: packed & 0xF_FFFF_FFFF,
        };
        let valid = min_block >= 16
            && max_block >= min_block
            && (max_frame == 0 || max_frame >= min_frame)
            && info.rate > 0
            && info.bps >= 4;
        valid.then_some(info)
    }
}

/// Why a frame did not parse.
#[derive(Debug)]
enum Bad {
    /// The data ran out first.
    Short,
    Invalid,
}
use Bad::{Invalid, Short};

struct Stream {
    info: StreamInfo,
    frames: u64,
    samples: u64,
    /// Whether frames are numbered by sample (variable block size) rather
    /// than by frame; fixed by the first frame.
    variable: Option<bool>,
}

impl Stream {
    /// Parses the frame at the start of `d` and returns its length.
    fn frame(&mut self, d: &[u8]) -> Result<usize, Bad> {
        let info = self.info;
        let mut b = Bits::new(d);
        if b.read(15)? != 0x7FFC {
            return Err(Invalid);
        }
        let variable = b.read(1)? == 1;
        if *self.variable.get_or_insert(variable) != variable {
            return Err(Invalid);
        }
        let block_code = b.read(4)?;
        let rate_code = b.read(4)?;
        let channel_code = b.read(4)?;
        let size_code = b.read(3)?;
        if b.read(1)? != 0 {
            return Err(Invalid);
        }
        let number = coded_number(&mut b)?;
        let expected = if variable { self.samples } else { self.frames };
        if number != expected {
            return Err(Invalid);
        }
        let block = match block_code {
            1 => 192,
            2..=5 => 576 << (block_code - 2),
            6 => b.read(8)? + 1,
            7 => b.read(16)? + 1,
            8..=15 => 256 << (block_code - 8),
            _ => return Err(Invalid),
        };
        let rate = match rate_code {
            0 => info.rate,
            1..=11 => [
                88200, 176400, 192000, 8000, 16000, 22050, 24000, 32000, 44100, 48000, 96000,
            ][rate_code as usize - 1],
            12 => b.read(8)? * 1000,
            13 => b.read(16)?,
            14 => b.read(16)? * 10,
            _ => return Err(Invalid),
        };
        let channels = match channel_code {
            0..=7 => channel_code + 1,
            8..=10 => 2,
            _ => return Err(Invalid),
        };
        let bps = match size_code {
            0 => info.bps,
            1 => 8,
            2 => 12,
            4 => 16,
            5 => 20,
            6 => 24,
            7 => 32,
            _ => return Err(Invalid),
        };
        let header_len = b.pos / 8;
        if rate != info.rate
            || channels != info.channels
            || bps != info.bps
            || block > info.max_block
            || b.read(8)? != u32::from(crc8_flac(&d[..header_len]))
        {
            return Err(Invalid);
        }

        for ch in 0..channels {
            // The difference channel of a stereo pair needs one more bit.
            let side = matches!((channel_code, ch), (8, 1) | (9, 0) | (10, 1));
            subframe(&mut b, block, bps + u32::from(side))?;
        }
        let pad = (8 - b.pos % 8) % 8;
        if b.read(pad as u32)? != 0 {
            return Err(Invalid);
        }
        let end = b.pos / 8;
        let crc = d.get(end..end + 2).ok_or(Short)?;
        if u16::from_be_bytes([crc[0], crc[1]]) != crc16_flac(&d[..end]) {
            return Err(Invalid);
        }
        self.frames += 1;
        self.samples += u64::from(block);
        Ok(end + 2)
    }
}

/// The frame or sample number, coded like UTF-8 but up to 36 bits.
fn coded_number(b: &mut Bits) -> Result<u64, Bad> {
    let first = b.read(8)?;
    let extra = match (first as u8).leading_ones() {
        0 => return Ok(u64::from(first)),
        n @ 2..=7 => n - 1,
        _ => return Err(Invalid),
    };
    let mut v = u64::from(first & (0x7F >> (extra + 1)));
    for _ in 0..extra {
        let c = b.read(8)?;
        if c & 0xC0 != 0x80 {
            return Err(Invalid);
        }
        v = v << 6 | u64::from(c & 0x3F);
    }
    Ok(v)
}

/// Steps over one channel's subframe.
fn subframe(b: &mut Bits, block: u32, mut bps: u32) -> Result<(), Bad> {
    if b.read(1)? != 0 {
        return Err(Invalid);
    }
    let typ = b.read(6)?;
    if b.read(1)? == 1 {
        let wasted = b.unary()? + 1;
        if wasted >= bps {
            return Err(Invalid);
        }
        bps -= wasted;
    }
    let block = u64::from(block);
    let bps = u64::from(bps);
    match typ {
        0 => b.skip(bps),
        1 => b.skip(bps * block),
        8..=12 => {
            let order = typ - 8;
            b.skip(u64::from(order) * bps)?;
            residual(b, block, order)
        }
        32..=63 => {
            let order = typ - 31;
            b.skip(u64::from(order) * bps)?;
            let precision = b.read(4)?;
            // A negative shift is not allowed.
            if precision == 15 || b.read(5)? & 0x10 != 0 {
                return Err(Invalid);
            }
            b.skip(u64::from(order) * u64::from(precision + 1))?;
            residual(b, block, order)
        }
        _ => Err(Invalid),
    }
}

/// Steps over a Rice-coded residual.
fn residual(b: &mut Bits, block: u64, order: u32) -> Result<(), Bad> {
    let (param_bits, escape) = match b.read(2)? {
        0 => (4, 15),
        1 => (5, 31),
        _ => return Err(Invalid),
    };
    let partition_order = b.read(4)?;
    let per = block >> partition_order;
    if per << partition_order != block || per < u64::from(order) {
        return Err(Invalid);
    }
    for p in 0..1u32 << partition_order {
        let n = per - if p == 0 { u64::from(order) } else { 0 };
        let k = b.read(param_bits)?;
        if k == escape {
            let raw = b.read(5)?;
            b.skip(n * u64::from(raw))?;
        } else {
            for _ in 0..n {
                b.unary()?;
                b.skip(u64::from(k))?;
            }
        }
    }
    Ok(())
}

/// A big-endian bit cursor.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, pos: 0 }
    }

    fn skip(&mut self, n: u64) -> Result<(), Bad> {
        let end = (self.pos as u64).checked_add(n).ok_or(Invalid)?;
        if end > self.d.len() as u64 * 8 {
            return Err(Short);
        }
        self.pos = end as usize;
        Ok(())
    }

    /// Reads `n` (at most 32) bits.
    fn read(&mut self, n: u32) -> Result<u32, Bad> {
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.d.len() * 8 {
            return Err(Short);
        }
        let byte = self.pos / 8;
        let mut w = [0u8; 8];
        let avail = (self.d.len() - byte).min(8);
        w[..avail].copy_from_slice(&self.d[byte..byte + avail]);
        let v = u64::from_be_bytes(w) << (self.pos % 8) >> (64 - n);
        self.pos += n as usize;
        Ok(v as u32)
    }

    /// Counts zero bits up to and past the next one bit.
    fn unary(&mut self) -> Result<u32, Bad> {
        let mut count = 0u32;
        loop {
            let byte = *self.d.get(self.pos / 8).ok_or(Short)?;
            let bits = byte << (self.pos % 8);
            if bits == 0 {
                let n = 8 - self.pos % 8;
                count = count.saturating_add(n as u32);
                self.pos += n;
            } else {
                let z = bits.leading_zeros();
                self.pos += z as usize + 1;
                return Ok(count + z);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A mono 16-bit stream of `frames` frames of 192 samples, each a
    /// verbatim subframe.
    pub(crate) fn build(frames: u32, total_known: bool) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.extend([0x80, 0, 0, 34]);
        out.extend(192u16.to_be_bytes());
        out.extend(192u16.to_be_bytes());
        out.extend([0; 6]);
        let total = if total_known { 192 * frames } else { 0 };
        let packed: u64 = (44100u64 << 44) | (15 << 36) | u64::from(total);
        out.extend(packed.to_be_bytes());
        out.extend([0; 16]);
        for i in 0..frames {
            // Block size code 1 (192), rate from STREAMINFO, mono, 16 bits.
            let mut f = vec![0xFF, 0xF8, 0x10, 0x08];
            // The frame number, coded like UTF-8.
            if i < 0x80 {
                f.push(i as u8);
            } else {
                f.extend([0xC0 | (i >> 6) as u8, 0x80 | (i & 0x3F) as u8]);
            }
            f.push(crc8_flac(&f));
            f.push(0x02); // verbatim subframe
            f.extend((0..192 * 2).map(|j| (j * 7 + i as usize) as u8));
            let crc = crc16_flac(&f);
            f.extend(crc.to_be_bytes());
            out.extend(f);
        }
        out
    }

    fn carve(d: &[u8]) -> Option<u64> {
        let d = d.to_vec();
        Flac.carve(&mut Reader::new(&d, 0), 0, d.len() as u64)
            .map(|f| f.len)
    }

    #[test]
    fn walks_every_frame() {
        for known in [true, false] {
            let good = build(300, known);
            let mut d = good.clone();
            d.extend([0xFF; 700]);
            assert_eq!(carve(&d), Some(good.len() as u64));
        }
        // A damaged frame breaks a stream that knows its length.
        let mut d = build(5, true);
        d[100] ^= 1;
        assert_eq!(carve(&d), None);
    }

    #[test]
    fn reads_bits() {
        let mut b = Bits::new(&[0b1010_0001, 0b0000_0000, 0b0100_0000]);
        assert_eq!(b.read(3).unwrap(), 0b101);
        assert_eq!(b.unary().unwrap(), 4);
        assert_eq!(b.unary().unwrap(), 9);
        assert!(matches!(b.unary(), Err(Short)));
    }
}

//! MP3 audio: an ID3 tag, then MPEG audio frames whose headers give each
//! frame's length, then optionally a 128-byte ID3v1 tag.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Mp3;

/// Frames required before data counts as audio.
const MIN_FRAMES: u32 = 8;

impl Format for Mp3 {
    fn name(&self) -> &'static str {
        "mp3"
    }

    fn max_len(&self) -> u64 {
        512 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        &head[..3] == b"ID3" && (2..=4).contains(&head[3]) && head[6..10].iter().all(|&b| b < 0x80)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let tag = r.array::<10>()?;
        let size = tag[6..10]
            .iter()
            .fold(0u64, |acc, &b| (acc << 7) | u64::from(b));
        let footer = if tag[5] & 0x10 != 0 { 10 } else { 0 };
        let mut pos = start + 10 + size + footer;
        // Some encoders pad the tag with zeros beyond its stated size.
        r.seek(pos);
        for _ in 0..4096 {
            if r.u8()? != 0 {
                break;
            }
            pos += 1;
        }

        let mut frames = 0;
        while pos + 4 <= limit {
            r.seek(pos);
            let Some(h) = r.array::<4>() else { break };
            let Some(len) = frame_len(h) else { break };
            pos += len;
            frames += 1;
        }
        if frames < MIN_FRAMES {
            return None;
        }
        r.seek(pos);
        if r.array::<3>().as_ref() == Some(b"TAG") {
            pos += 128;
        }
        (pos <= limit).then_some(Found {
            len: pos - start,
            ext: "mp3",
            kind: Kind::Audio,
            taken: None,
        })
    }
}

/// The length of the MPEG audio frame whose header is `h`.
fn frame_len(h: [u8; 4]) -> Option<u64> {
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0: 2.5, 2: 2, 3: 1
    let layer = (h[1] >> 1) & 3; // 1: III, 2: II, 3: I
    let bitrate_index = usize::from(h[2] >> 4);
    let rate_index = usize::from((h[2] >> 2) & 3);
    let padding = u64::from((h[2] >> 1) & 1);
    if version == 1 || layer == 0 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    const V1_L1: [u64; 15] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1_L2: [u64; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1_L3: [u64; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2_L1: [u64; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2_L23: [u64; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let kbps = match (version, layer) {
        (3, 3) => V1_L1,
        (3, 2) => V1_L2,
        (3, 1) => V1_L3,
        (_, 3) => V2_L1,
        _ => V2_L23,
    }[bitrate_index];
    let rate = match version {
        3 => [44_100, 48_000, 32_000],
        2 => [22_050, 24_000, 16_000],
        _ => [11_025, 12_000, 8_000],
    }[rate_index];
    let bps = kbps * 1000;
    Some(match layer {
        3 => (12 * bps / rate + padding) * 4,
        1 if version != 3 => 72 * bps / rate + padding,
        _ => 144 * bps / rate + padding,
    })
}

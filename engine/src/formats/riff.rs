//! RIFF containers: AVI (older digital cameras), WebP and WAV. The header
//! states the total size. AVIs over 1 GB (OpenDML) continue in further
//! `RIFF AVIX` chunks, which are followed too.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Riff;

impl Format for Riff {
    fn name(&self) -> &'static str {
        "avi"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        &head[..4] == b"RIFF" && matches!(&head[8..12], b"AVI " | b"WEBP" | b"WAVE")
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 4);
        let size = u64::from(r.u32le()?);
        let form = r.array::<4>()?;
        let (ext, kind) = match &form {
            b"AVI " => ("avi", Kind::Video),
            b"WEBP" => ("webp", Kind::Photo),
            b"WAVE" => ("wav", Kind::Audio),
            _ => return None,
        };
        if size < 4 {
            return None;
        }
        let mut end = start + 8 + size + (size & 1);

        if ext == "avi" {
            loop {
                r.seek(end);
                let Some(hdr) = r.array::<12>() else { break };
                if &hdr[..4] != b"RIFF" || &hdr[8..] != b"AVIX" {
                    break;
                }
                let size = u64::from(u32::from_le_bytes(hdr[4..8].try_into().unwrap()));
                end += 8 + size + (size & 1);
            }
        }

        (end <= limit && end <= r.source_len()).then_some(Found {
            len: end - start,
            ext,
            kind,
            taken: None,
        })
    }
}

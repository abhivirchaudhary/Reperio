//! AVCHD camcorder video (.MTS / .M2TS): 192-byte packets, each a 4-byte
//! timestamp followed by an MPEG transport stream packet starting with the
//! sync byte 0x47. The file ends where the packet rhythm breaks.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Mts;

const PACKET: u64 = 192;
const SYNC: u8 = 0x47;
/// Shortest stream accepted, so that a few stray 0x47 bytes are not a file.
const MIN_PACKETS: u64 = 1000;

impl Format for Mts {
    fn name(&self) -> &'static str {
        "mts"
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        (0..3).all(|i| head[4 + i * PACKET as usize] == SYNC)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        let mut packets = 0;
        loop {
            let at = start + packets * PACKET;
            if at + PACKET > limit {
                break;
            }
            r.seek(at + 4);
            if r.u8() != Some(SYNC) {
                break;
            }
            packets += 1;
        }
        (packets >= MIN_PACKETS).then_some(Found {
            len: packets * PACKET,
            ext: "mts",
            kind: Kind::Video,
            taken: None,
        })
    }
}

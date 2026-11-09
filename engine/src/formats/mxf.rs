//! MXF, the professional video format of Sony, Canon and Panasonic
//! cameras: a sequence of KLV packets (a 16-byte SMPTE key, a BER length
//! and a value) grouped into partitions. A finished file's header
//! partition pack says where the footer partition starts; the file ends
//! after the footer's packets, usually with a random index pack whose
//! last four bytes repeat its own length. A recording cut off before the
//! footer was written ends after its last whole packet.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Mxf;

/// Every SMPTE key starts with these bytes.
const KEY_PREFIX: [u8; 4] = [0x06, 0x0E, 0x2B, 0x34];
/// Partition packs: the next byte gives the kind (2 header, 3 body,
/// 4 footer), the one after it whether the partition is open or closed.
const PARTITION: [u8; 13] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0D, 0x01, 0x02, 0x01, 0x01,
];
const HEADER: u8 = 0x02;
const FOOTER: u8 = 0x04;
const RANDOM_INDEX: u8 = 0x11;

impl Format for Mxf {
    fn name(&self) -> &'static str {
        "mxf"
    }

    fn max_len(&self) -> u64 {
        256 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        partition_kind(&head[..16]) == Some(HEADER)
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start);
        let (key, len) = klv(r)?;
        if partition_kind(&key) != Some(HEADER) || !(88..=1 << 20).contains(&len) {
            return None;
        }
        let pack = r.array::<88>()?;
        let be32 = |o: usize| u32::from_be_bytes([pack[o], pack[o + 1], pack[o + 2], pack[o + 3]]);
        let be64 = |o: usize| u64::from_be_bytes(pack[o..o + 8].try_into().unwrap_or_default());
        let kag = be32(4);
        if u16::from_be_bytes([pack[0], pack[1]]) != 1
            || kag == 0
            || kag > 1 << 20
            || be64(8) != 0
            || be64(16) != 0
        {
            return None;
        }
        let footer = be64(24);

        let mut pos;
        let mut in_footer = false;
        if footer > 0 {
            pos = start.checked_add(footer)?;
            if pos >= limit {
                return None;
            }
            r.seek(pos);
            let (key, _) = klv(r)?;
            if partition_kind(&key) != Some(FOOTER)
                || r.array::<16>()?[8..16] != footer.to_be_bytes()
            {
                return None;
            }
            in_footer = true;
        } else {
            pos = start;
        }

        // Walk packets to the end of the footer, or of the data.
        loop {
            r.seek(pos);
            let Some((key, len)) = klv(r) else { break };
            let Some(next) = r.pos().checked_add(len).filter(|&n| n <= limit) else {
                break;
            };
            match partition_kind(&key) {
                Some(HEADER) if pos != start => break,
                Some(FOOTER) if !in_footer => {
                    // An open file: the footer must say where it is.
                    let this = r
                        .array::<16>()
                        .map(|v| u64::from_be_bytes(v[8..16].try_into().unwrap_or_default()));
                    if this != Some(pos - start) {
                        return None;
                    }
                    in_footer = true;
                }
                Some(RANDOM_INDEX) if in_footer => {
                    r.seek(next.checked_sub(4)?);
                    if u64::from(r.u32be()?) != next - pos {
                        return None;
                    }
                    pos = next;
                    break;
                }
                _ => {}
            }
            pos = next;
        }
        (pos > start).then_some(Found {
            len: pos - start,
            ext: "mxf",
            kind: Kind::Video,
            taken: None,
        })
    }
}

/// The kind byte of a partition pack or random index pack key.
fn partition_kind(key: &[u8]) -> Option<u8> {
    let kind = key[13];
    let valid = key[..13] == PARTITION
        && key[15] == 0
        && match kind {
            HEADER..=FOOTER => (1..=4).contains(&key[14]),
            RANDOM_INDEX => key[14] == 1,
            _ => false,
        };
    valid.then_some(kind)
}

/// Reads a packet's key and BER-coded length, leaving the reader at its
/// value.
fn klv(r: &mut Reader) -> Option<([u8; 16], u64)> {
    let key = r.array::<16>()?;
    if key[..4] != KEY_PREFIX {
        return None;
    }
    let first = r.u8()?;
    let len = match first {
        0..=0x7F => u64::from(first),
        0x81..=0x88 => {
            let mut v = 0u64;
            for _ in 0..first & 0x7F {
                v = v << 8 | u64::from(r.u8()?);
            }
            v
        }
        _ => return None,
    };
    Some((key, len))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn packet(key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut out = key.to_vec();
        out.push(0x83);
        out.extend(&(value.len() as u32).to_be_bytes()[1..]);
        out.extend(value);
        out
    }

    fn partition(kind: u8, this: u64, footer: u64) -> Vec<u8> {
        let mut key = PARTITION.to_vec();
        key.extend([kind, 4, 0]);
        let mut v = vec![0u8; 88];
        v[1] = 1;
        v[4..8].copy_from_slice(&512u32.to_be_bytes());
        v[8..16].copy_from_slice(&this.to_be_bytes());
        v[24..32].copy_from_slice(&footer.to_be_bytes());
        packet(&key, &v)
    }

    /// A header partition, one essence packet and a footer, with the
    /// footer's offset recorded in the header or not.
    pub(crate) fn build(closed: bool) -> Vec<u8> {
        let essence_key = [6, 14, 43, 52, 1, 2, 1, 1, 13, 1, 3, 1, 21, 1, 5, 1];
        let mut body = packet(&essence_key, &[0x5D; 4000]);
        let footer_at = partition(HEADER, 0, 0).len() + body.len();
        let mut out = partition(HEADER, 0, if closed { footer_at as u64 } else { 0 });
        out.append(&mut body);
        out.extend(partition(FOOTER, footer_at as u64, footer_at as u64));
        let mut rip_key = PARTITION.to_vec();
        rip_key.extend([RANDOM_INDEX, 1, 0]);
        let mut rip = packet(&rip_key, &[0; 12]);
        let n = rip.len() as u32;
        let at = rip.len() - 4;
        rip[at..].copy_from_slice(&n.to_be_bytes());
        out.extend(rip);
        out
    }

    #[test]
    fn walks_to_the_random_index() {
        for closed in [true, false] {
            let good = build(closed);
            let mut d = good.clone();
            d.extend(build(true));
            let carve = |d: &Vec<u8>| {
                Mxf.carve(&mut Reader::new(d, 0), 0, d.len() as u64)
                    .map(|f| f.len)
            };
            assert_eq!(carve(&d), Some(good.len() as u64));
        }
    }
}

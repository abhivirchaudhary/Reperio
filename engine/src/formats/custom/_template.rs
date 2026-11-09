//! Template for a custom scanner. Copy this file to `my_format.rs` (no
//! leading underscore; lower-case letters, digits and `_` only), edit it,
//! and run `cargo test -p reperio-engine custom` then `cargo build`.
//!
//! A scanner answers two questions at a sector-aligned offset on the drive:
//!   1. `matches`: do the first 512 bytes look like the start of my file?
//!      Cheap and quick; it runs on every candidate sector.
//!   2. `carve`: walk the file's own structure and say how long it is, or
//!      return `None` if it is not really my file.
//!
//! Both are used by the quick-first scan, the deep scan and `reperio scan`.
//! Keep reads inside `start..start + limit`. Never trust a length field
//! without checking it against `limit`; damaged data is the normal case.

use crate::formats::{Format, Found, Kind};
use crate::reader::Reader;

pub struct MyFormat;

impl Format for MyFormat {
    /// Name for `reperio scan --formats <name>`.
    fn name(&self) -> &'static str {
        "myformat"
    }

    /// Largest file you will ever carve; bounds the work on a bad candidate.
    fn max_len(&self) -> u64 {
        256 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.starts_with(b"MYFT")
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        // Example: bytes 4..8 hold the total length, little endian.
        r.seek(start);
        let header = r.bytes(8)?;
        let len = u64::from(u32::from_le_bytes(header[4..8].try_into().ok()?));
        if len < 8 || len > limit {
            return None;
        }
        Some(Found {
            len,
            ext: "myf",
            kind: Kind::Other,
            taken: None,
        })
    }
}

/// Called by `build.rs`. Return your scanner.
pub fn format() -> Box<dyn Format> {
    Box::new(MyFormat)
}

/// Valid example files. The test suite flips bits in them, truncates them
/// and fills them with noise, and your scanner must never panic or claim
/// more bytes than exist. Return `vec![]` to skip (not recommended).
pub fn samples() -> Vec<Vec<u8>> {
    let mut f = b"MYFT".to_vec();
    f.extend(&40u32.to_le_bytes());
    f.resize(40, 7);
    vec![f]
}

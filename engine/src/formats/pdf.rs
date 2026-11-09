//! PDF documents. A PDF ends with `startxref`, an offset, and `%%EOF`.
//! Edited PDFs append updates, each with its own `%%EOF`, so an end marker
//! followed by more objects is not the end.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Pdf;

impl Format for Pdf {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn max_len(&self) -> u64 {
        1 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.starts_with(b"%PDF-1.") || head.starts_with(b"%PDF-2.")
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        r.seek(start + 8);
        loop {
            r.skip_past(b'%', limit)?;
            let at = r.pos() - 1;
            if r.array::<4>().as_ref() != Some(b"%EOF") {
                r.seek(at + 1);
                continue;
            }
            // A real end marker follows `startxref` and an offset.
            r.seek(at.saturating_sub(48).max(start));
            let before = r.bytes((at - at.saturating_sub(48).max(start)) as usize)?;
            if !before.windows(9).any(|w| w == b"startxref") {
                r.seek(at + 5);
                continue;
            }
            let mut end = at + 5;
            r.seek(end);
            match r.array::<2>() {
                Some([b'\r', b'\n']) => end += 2,
                Some([b'\r' | b'\n', _]) => end += 1,
                _ => {}
            }
            // An incremental update continues with objects or a table.
            r.seek(end);
            let next = r.bytes(32).unwrap_or_default();
            let trimmed: Vec<u8> = next
                .iter()
                .copied()
                .skip_while(|b| b.is_ascii_whitespace())
                .collect();
            let more = trimmed.starts_with(b"xref") || starts_with_object(&trimmed);
            if !more {
                return (end <= limit).then_some(Found {
                    len: end - start,
                    ext: "pdf",
                    kind: Kind::Document,
                    taken: None,
                });
            }
            r.seek(end);
        }
    }
}

/// Whether `s` begins with an object header such as `12 0 obj`.
fn starts_with_object(s: &[u8]) -> bool {
    let number = |s: &[u8]| s.iter().take_while(|b| b.is_ascii_digit()).count();
    let a = number(s);
    if a == 0 || s.get(a) != Some(&b' ') {
        return false;
    }
    let rest = &s[a + 1..];
    let b = number(rest);
    b > 0 && rest[b..].starts_with(b" obj")
}

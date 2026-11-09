//! Plain text (.txt, and the text formats with a recognisable first line:
//! .html, .xml, .json, .rtf).
//!
//! Text has no header and no end marker, so a file is recognised by what
//! it reads like, and it ends where the text does. That is much weaker than
//! any format with a signature, so the checks are strict, to keep ordinary
//! leftovers out:
//!
//! - it starts at a sector boundary with text, and runs unbroken to a zero
//!   byte or a byte that is not text (a file's unused end is zeros);
//! - the text is UTF-8 (any script) or UTF-16 with a byte-order mark, or
//!   ASCII with a few accented letters of an older encoding;
//! - it reads like writing: spaces and line breaks make up a sensible part
//!   of it, and most of the rest is letters. Base64, hex dumps, repeated
//!   characters and tables of numbers do not.
//!
//! The end is only a guess: a text file followed by other old text in the
//! same cluster runs on into it, and a text file split in pieces is found
//! only as its first piece.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct PlainText;

/// Shortest text accepted, in bytes.
const MIN_LEN: u64 = 128;
/// Shortest start of a candidate that the first sector must show.
const MIN_HEAD: usize = 48;
/// Bytes read at a time.
const STEP: usize = 64 << 10;
/// Share of the bytes that are spaces and line breaks: prose is about
/// 0.15, indented source code up to about 0.55, and nothing that is not text
/// writing comes near these.
const WHITESPACE: std::ops::RangeInclusive<f64> = 0.04..=0.60;

impl Format for PlainText {
    fn name(&self) -> &'static str {
        "text"
    }

    fn max_len(&self) -> u64 {
        256 << 20
    }

    fn matches(&self, head: &[u8]) -> bool {
        if let Some(units) = utf16_units(head) {
            return units.len() >= MIN_HEAD / 2 && reads_like_writing(&Stats::of_units(&units));
        }
        let text = text_prefix(head);
        // The text runs to the end of the sector, or is followed by nothing
        // but zeros.
        if text.len() < MIN_HEAD || head[text.len()..].iter().any(|&b| b != 0) {
            return false;
        }
        let sample = whole_chars(text);
        match Encoding::of(sample) {
            Some(_) => reads_like_writing(&Stats::of(sample)),
            None => false,
        }
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        let mut first = vec![0u8; 4];
        r.seek(start);
        r.read_exact(&mut first)?;
        r.seek(start);
        let (len, ext) = if first.starts_with(&[0xFF, 0xFE]) || first.starts_with(&[0xFE, 0xFF]) {
            (carve_utf16(r, limit)?, "txt")
        } else {
            carve_bytes(r, limit)?
        };
        (len >= MIN_LEN && ends_like_a_file(r, start + len, limit)).then_some(Found {
            len,
            ext,
            kind: Kind::from_ext(ext),
            taken: None,
        })
    }
}

/// Whether text that ends at `end` ends like a file does. Whatever writes a
/// file fills the rest of its last sector with zeros, so a run of text with
/// anything else after it is a stretch of something larger: the readable
/// strings in a program, a database, an archive. A file that ends exactly
/// at a sector boundary cannot be told this way, and is accepted.
fn ends_like_a_file(r: &mut Reader, end: u64, limit: u64) -> bool {
    let rest = (512 - end % 512) % 512;
    if rest == 0 || end >= r.source_len() || end >= limit {
        return true;
    }
    let rest = rest.min(r.source_len() - end) as usize;
    r.seek(end);
    r.bytes(rest).is_some_and(|b| b.iter().all(|&x| x == 0))
}

/// How the bytes of a text are encoded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// ASCII, or UTF-8 for other scripts.
    Utf8,
    /// ASCII with some bytes of 0x80 and over that are not UTF-8 (an older
    /// single-byte encoding): only a few of them.
    Legacy,
}

impl Encoding {
    fn of(sample: &[u8]) -> Option<Self> {
        if std::str::from_utf8(sample).is_ok() {
            return Some(Encoding::Utf8);
        }
        let high = sample.iter().filter(|&&b| b >= 0x80).count();
        (high * 20 <= sample.len()).then_some(Encoding::Legacy)
    }
}

fn is_text(b: u8) -> bool {
    matches!(b, 0x20..=0x7E | b'\t' | b'\r' | b'\n' | 0x0C) || b >= 0x80
}

/// The leading part of `data` made of text bytes.
fn text_prefix(data: &[u8]) -> &[u8] {
    &data[..data.iter().position(|&b| !is_text(b)).unwrap_or(data.len())]
}

/// `text` without a last character cut short by the end of the sample.
fn whole_chars(text: &[u8]) -> &[u8] {
    let mut end = text.len();
    // A UTF-8 character is at most four bytes: drop a partial one.
    for back in 1..=3.min(end) {
        let b = text[end - back];
        if b & 0xC0 == 0xC0 {
            // A lead byte: whole only if all its bytes are present.
            let need = if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else {
                2
            };
            if back < need {
                end -= back;
            }
            break;
        }
        if b & 0x80 == 0 {
            break;
        }
    }
    &text[..end]
}

/// What the characters of a text are.
#[derive(Default)]
struct Stats {
    total: usize,
    space: usize,
    /// Letters, in any script.
    letters: usize,
}

impl Stats {
    fn of(bytes: &[u8]) -> Self {
        let mut s = Stats::default();
        s.add(bytes);
        s
    }

    fn add(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        for &b in bytes {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.space += 1;
            } else if b.is_ascii_alphabetic() || b >= 0x80 {
                self.letters += 1;
            }
        }
    }

    fn of_units(units: &[u16]) -> Self {
        let mut s = Stats::default();
        for &u in units {
            s.total += 1;
            if matches!(u, 0x20 | 0x09 | 0x0D | 0x0A) {
                s.space += 1;
            } else if u >= 0x80 || (u < 0x80 && (u as u8).is_ascii_alphabetic()) {
                s.letters += 1;
            }
        }
        s
    }
}

/// Whether the text reads like writing: a sensible share of spaces and
/// line breaks, and mostly letters in the rest.
fn reads_like_writing(s: &Stats) -> bool {
    if s.total == 0 {
        return false;
    }
    let space = s.space as f64 / s.total as f64;
    let rest = (s.total - s.space).max(1);
    WHITESPACE.contains(&space) && s.letters * 100 >= rest * 40
}

/// Carves a text of bytes (UTF-8 or an older encoding).
fn carve_bytes(r: &mut Reader, limit: u64) -> Option<(u64, &'static str)> {
    let start = r.pos();
    let mut stats = Stats::default();
    let mut len = 0u64;
    let mut head: Vec<u8> = Vec::new();
    let mut encoding: Option<Encoding> = None;
    let mut buf = Vec::new();
    // The bytes of a UTF-8 character cut by the end of the last chunk.
    let mut carry: Vec<u8> = Vec::new();
    while len < limit {
        let n = (limit - len).min(STEP as u64) as usize;
        let want = n.min(
            r.source_len()
                .saturating_sub(start + len + carry.len() as u64) as usize,
        );
        if want == 0 {
            break;
        }
        buf.clear();
        buf.resize(want, 0);
        r.read_exact(&mut buf)?;
        let end = buf.iter().position(|&b| !is_text(b)).unwrap_or(buf.len());
        let stopped = end < buf.len();
        let mut cur = std::mem::take(&mut carry);
        cur.extend_from_slice(&buf[..end]);
        if encoding.is_none() {
            // Decided on the start of the text, which the first chunk holds.
            head = cur[..cur.len().min(4096)].to_vec();
            encoding = Some(Encoding::of(whole_chars(&head))?);
        }
        let mut valid = cur.len();
        let mut broken = false;
        if encoding == Some(Encoding::Utf8) {
            if let Err(e) = std::str::from_utf8(&cur) {
                valid = e.valid_up_to();
                if e.error_len().is_none() && !stopped {
                    // A character cut by the end of the chunk: the next
                    // chunk completes it.
                    carry = cur[valid..].to_vec();
                } else {
                    broken = true;
                }
            }
        }
        stats.add(&cur[..valid]);
        len += valid as u64;
        if stopped || broken || want < n {
            break;
        }
    }
    // Too short, or text that does not read like writing, is not a file.
    if len < MIN_LEN || !reads_like_writing(&stats) {
        return None;
    }
    Some((len, extension(&head)))
}

/// The extension a text takes from its first line.
fn extension(head: &[u8]) -> &'static str {
    let t = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let t = &t[t
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(t.len())..];
    let lower: Vec<u8> = t.iter().take(64).map(u8::to_ascii_lowercase).collect();
    if lower.starts_with(b"{\\rtf") {
        "rtf"
    } else if lower.starts_with(b"<?xml") {
        "xml"
    } else if lower.starts_with(b"<!doctype html") || lower.starts_with(b"<html") {
        "html"
    } else if lower.starts_with(b"{\"") || lower.starts_with(b"[{") || lower.starts_with(b"[\"") {
        "json"
    } else {
        "txt"
    }
}

/// The UTF-16 code units of the start of `head`, if it has a byte-order
/// mark and is text.
fn utf16_units(head: &[u8]) -> Option<Vec<u16>> {
    let big = match head.get(..2)? {
        [0xFF, 0xFE] => false,
        [0xFE, 0xFF] => true,
        _ => return None,
    };
    let mut units = Vec::new();
    for pair in head[2..].chunks_exact(2) {
        let u = if big {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        };
        if u == 0 {
            // The text ends in zeros, to the end of the sector.
            if head[2 + units.len() * 2..].iter().any(|&b| b != 0) {
                return None;
            }
            break;
        }
        if u < 0x20 && !matches!(u, 0x09 | 0x0A | 0x0D | 0x0C) {
            return None;
        }
        units.push(u);
    }
    Some(units)
}

/// Carves a UTF-16 text with a byte-order mark.
fn carve_utf16(r: &mut Reader, limit: u64) -> Option<u64> {
    let start = r.pos();
    let mut bom = [0u8; 2];
    r.read_exact(&mut bom)?;
    let big = bom == [0xFE, 0xFF];
    let mut stats = Stats::default();
    let mut len = 2u64;
    let mut buf = Vec::new();
    'read: while len < limit {
        let n = (limit - len).min(STEP as u64) as usize & !1;
        let want = n.min(r.source_len().saturating_sub(start + len) as usize) & !1;
        if want == 0 {
            break;
        }
        buf.clear();
        buf.resize(want, 0);
        r.read_exact(&mut buf)?;
        for pair in buf.chunks_exact(2) {
            let u = if big {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            };
            if u == 0 || (u < 0x20 && !matches!(u, 0x09 | 0x0A | 0x0D | 0x0C)) {
                break 'read;
            }
            stats.total += 1;
            if matches!(u, 0x20 | 0x09 | 0x0D | 0x0A) {
                stats.space += 1;
            } else if u >= 0x80 || (u as u8).is_ascii_alphabetic() {
                stats.letters += 1;
            }
            len += 2;
        }
        if want < n {
            break;
        }
    }
    (len >= MIN_LEN && reads_like_writing(&stats)).then_some(len)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::HEAD_LEN;
    use super::*;

    const PROSE: &str = "The quick brown fox jumps over the lazy dog. We met on Tuesday, and talked for hours about the garden, the weather, and the long road home.\nNobody minded the rain; it was warm, and the evening was quiet.\n";

    fn prose(n: usize) -> String {
        PROSE.repeat(n)
    }

    fn carve(data: &[u8]) -> Option<(u64, &'static str)> {
        let mut d = data.to_vec();
        d.resize(d.len().max(HEAD_LEN), 0);
        if !PlainText.matches(&d[..HEAD_LEN]) {
            return None;
        }
        PlainText
            .carve(&mut Reader::new(&d, 0), 0, d.len() as u64)
            .map(|f| (f.len, f.ext))
    }

    #[test]
    fn finds_english_text_exactly() {
        let t = prose(60);
        let mut d = t.clone().into_bytes();
        let len = d.len() as u64;
        d.extend(vec![0u8; 3000]);
        assert_eq!(carve(&d), Some((len, "txt")));
        // Short, under a sector.
        let t = prose(1);
        let mut d = t.clone().into_bytes();
        let len = d.len() as u64;
        d.extend(vec![0u8; 800]);
        assert_eq!(carve(&d), Some((len, "txt")));
    }

    #[test]
    fn finds_text_in_other_scripts() {
        let hindi =
            "यह मेरा पहला पत्र है। मैं आशा करता हूँ कि आप सब ठीक होंगे और घर में सब कुशल होगा।\n".repeat(40);
        let mut d = hindi.clone().into_bytes();
        let len = d.len() as u64;
        d.extend(vec![0u8; 2000]);
        assert_eq!(carve(&d), Some((len, "txt")));
        let french = "Il était une fois, dans un petit village près de Lyon, une femme très âgée qui aimait les fleurs.\n".repeat(30);
        let mut d = french.clone().into_bytes();
        let len = d.len() as u64;
        d.extend(vec![0u8; 2000]);
        assert_eq!(carve(&d), Some((len, "txt")));
    }

    #[test]
    fn finds_older_encodings_and_utf16() {
        // Windows-1252: é as the single byte 0xE9.
        let mut legacy = prose(30).into_bytes();
        for i in (0..legacy.len()).step_by(197) {
            if legacy[i] == b'e' {
                legacy[i] = 0xE9;
            }
        }
        let len = legacy.len() as u64;
        legacy.extend(vec![0u8; 2000]);
        assert_eq!(carve(&legacy), Some((len, "txt")));
        // UTF-16 with a byte-order mark, as Notepad's "Unicode".
        for big in [false, true] {
            let mut d: Vec<u8> = if big {
                vec![0xFE, 0xFF]
            } else {
                vec![0xFF, 0xFE]
            };
            for u in prose(30).encode_utf16() {
                d.extend(if big {
                    u.to_be_bytes()
                } else {
                    u.to_le_bytes()
                });
            }
            let len = d.len() as u64;
            d.extend(vec![0u8; 2000]);
            assert_eq!(carve(&d), Some((len, "txt")), "big endian: {big}");
        }
    }

    #[test]
    fn names_text_formats_by_their_first_line() {
        let cases = [
            (
                format!("<!DOCTYPE html>\n<html><body>{}</body></html>\n", prose(10)),
                "html",
            ),
            (
                format!("<?xml version=\"1.0\"?>\n<note>{}</note>\n", prose(10)),
                "xml",
            ),
            (
                format!("{{\"text\": \"{}\"}}\n", prose(10).replace('\n', " ")),
                "json",
            ),
            (format!("{{\\rtf1\\ansi {}}}\n", prose(10)), "rtf"),
        ];
        for (text, ext) in cases {
            let mut d = text.into_bytes();
            d.extend(vec![0u8; 2000]);
            assert_eq!(carve(&d).map(|c| c.1), Some(ext), "{ext}");
        }
    }

    #[test]
    fn is_not_fooled_by_what_is_not_writing() {
        // Base64, hex, repeated characters, numbers, a lone word, binary.
        let b64: String = (0..200)
            .map(|i| {
                format!(
                    "{}\n",
                    "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo"
                        .chars()
                        .cycle()
                        .skip(i)
                        .take(76)
                        .collect::<String>()
                )
            })
            .collect();
        assert_eq!(carve(b64.as_bytes()), None);
        let hex: String = (0..100)
            .map(|i| {
                format!(
                    "{:08x} 00 11 22 33 44 55 66 77 88 99 aa bb cc dd ee ff\n",
                    i * 16
                )
            })
            .collect();
        assert_eq!(carve(hex.as_bytes()), None);
        assert_eq!(carve("a".repeat(5000).as_bytes()), None);
        assert_eq!(carve("-".repeat(80).repeat(60).as_bytes()), None);
        let numbers: String = (0..800)
            .map(|i| format!("{}.{}\n", i * 7, i % 10))
            .collect();
        assert_eq!(carve(numbers.as_bytes()), None);
        let mut binary: Vec<u8> = (0..8000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        binary[0] = b'T';
        assert_eq!(carve(&binary), None);
        assert_eq!(carve(&vec![0u8; 4096]), None);
        // Too short to be a file.
        assert_eq!(carve(b"Hello there, this is a very short note."), None);
    }

    #[test]
    fn stops_at_the_end_of_the_text() {
        let t = prose(20);
        let mut d = t.clone().into_bytes();
        let len = d.len() as u64;
        // Zeros, then more text further on (another file's).
        d.extend(vec![0u8; 700]);
        d.extend(prose(5).into_bytes());
        assert_eq!(carve(&d), Some((len, "txt")));
    }

    #[test]
    fn survives_damage_without_panicking() {
        let good = prose(40).into_bytes();
        let mut x = 0x9E37_79B9u32;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..400 {
            let mut d = good.clone();
            for _ in 0..(rand() % 30) {
                let at = rand() as usize % d.len();
                d[at] = rand() as u8;
            }
            d.truncate(rand() as usize % (d.len() + 1));
            let _ = carve(&d);
        }
    }
}

//! Delimited text (.csv and .tsv): rows of fields separated by commas,
//! semicolons or tabs, which have no header of their own to recognise.
//!
//! So a file is recognised by its shape instead. A file starts at a sector
//! boundary with a row of at least two fields, and every row after it has
//! the same number of fields. Fields in double quotes may hold the
//! delimiter and line breaks. The file ends at the last whole row with the
//! right number of fields, before the text stops (a zero byte, a byte that
//! is not text, or the end of the data) or the shape breaks.
//!
//! This is the weakest kind of find: a file's end is only a guess, text
//! split in pieces is not rejoined, and logs or code with regular columns
//! look the same. It is the only way a deleted `.csv` comes back once the
//! drive's file list is gone, as after a format.

use super::{Format, Found, Kind};
use crate::reader::Reader;

pub struct Delimited;

/// Records (rows) needed, header included. Fewer would find too much
/// ordinary text.
const MIN_RECORDS: usize = 4;
/// Fields needed per record.
const MIN_FIELDS: usize = 2;
/// Records with the wrong number of fields in a row that end the file.
const MAX_BAD_IN_A_ROW: usize = 3;
/// Bytes read at a time.
const STEP: usize = 64 << 10;

impl Format for Delimited {
    fn name(&self) -> &'static str {
        "csv"
    }

    fn max_len(&self) -> u64 {
        1 << 30
    }

    fn matches(&self, head: &[u8]) -> bool {
        let text = text_prefix(head);
        let body = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
        // Text must run to the end of the sector, or be followed by nothing
        // but zeros (a short file's unused part).
        if body.len() < 12 || head[text.len()..].iter().any(|&b| b != 0) {
            return false;
        }
        // The first row must fit in the sector and have a delimiter, and a
        // second must have begun.
        let Some(nl) = body.iter().position(|&b| b == b'\n') else {
            return false;
        };
        nl + 1 < body.len() && delimiter_of(&body[..nl]).is_some() && high_fraction(body) <= 0.25
    }

    fn carve(&self, r: &mut Reader, start: u64, limit: u64) -> Option<Found> {
        let mut state = Walk::default();
        let mut pos = start;
        let mut buf = Vec::new();
        while pos < limit {
            let n = (limit - pos).min(STEP as u64) as usize;
            buf.clear();
            buf.resize(n, 0);
            r.seek(pos);
            let Some(got) = read_some(r, &mut buf) else {
                break;
            };
            buf.truncate(got);
            let ended = state.feed(&buf);
            pos += got as u64;
            if ended || got < n {
                break;
            }
        }
        let (len, delim) = state.finish()?;
        Some(Found {
            len,
            ext: if delim == b'\t' { "tsv" } else { "csv" },
            kind: Kind::Document,
            taken: None,
        })
    }
}

/// Reads up to `out.len()` bytes; a short count at the end of the source.
fn read_some(r: &mut Reader, out: &mut [u8]) -> Option<usize> {
    let want = out
        .len()
        .min(r.source_len().saturating_sub(r.pos()) as usize);
    if want == 0 {
        return None;
    }
    r.read_exact(&mut out[..want])?;
    Some(want)
}

/// The leading part of `data` made of text: printable ASCII, tab, line
/// breaks, and bytes of 0x80 and over (UTF-8 or an older single-byte
/// encoding).
fn text_prefix(data: &[u8]) -> &[u8] {
    let end = data.iter().position(|&b| !is_text(b)).unwrap_or(data.len());
    &data[..end]
}

fn is_text(b: u8) -> bool {
    matches!(b, 0x20..=0x7E | b'\t' | b'\r' | b'\n') || b >= 0x80
}

fn high_fraction(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    data.iter().filter(|&&b| b >= 0x80).count() as f64 / data.len() as f64
}

/// The delimiter a row most likely uses: the one of comma, semicolon, tab
/// and pipe that occurs most outside quotes, at least once.
fn delimiter_of(row: &[u8]) -> Option<u8> {
    let mut counts = [0usize; 4];
    let mut quoted = false;
    for &b in row {
        match b {
            b'"' => quoted = !quoted,
            b',' if !quoted => counts[0] += 1,
            b';' if !quoted => counts[1] += 1,
            b'\t' if !quoted => counts[2] += 1,
            b'|' if !quoted => counts[3] += 1,
            _ => {}
        }
    }
    let (i, &best) = counts
        .iter()
        .enumerate()
        .max_by_key(|&(i, &c)| (c, 3 - i))?;
    (best >= MIN_FIELDS - 1).then_some(b",;\t|"[i])
}

/// Follows the rows of a file as its bytes arrive.
#[derive(Default)]
struct Walk {
    /// Bytes seen so far.
    seen: u64,
    delim: u8,
    /// Fields in the first (header) record.
    expected: usize,
    /// Record being read.
    fields: usize,
    quoted: bool,
    /// Something has been read in the current record.
    started: bool,
    /// The first record's bytes, until it is complete.
    first: Vec<u8>,
    records: usize,
    bad_in_a_row: usize,
    /// Length up to the end of the last good record.
    good_len: u64,
    /// Records with the right number of fields so far, and all records
    /// (good or not) up to the last good one.
    good_count: usize,
    records_at_good: usize,
    done: bool,
}

impl Walk {
    /// Takes the next bytes. Returns true once the file has ended.
    fn feed(&mut self, data: &[u8]) -> bool {
        for (i, &b) in data.iter().enumerate() {
            if self.done {
                return true;
            }
            if !is_text(b) {
                // The text stops here: a last record with no line break
                // still counts.
                self.end_record(self.seen + i as u64, true);
                self.done = true;
                return true;
            }
            self.byte(b, self.seen + i as u64 + 1);
        }
        self.seen += data.len() as u64;
        self.done
    }

    fn byte(&mut self, b: u8, end: u64) {
        if self.records == 0 {
            // Still reading the header, whose delimiter is not known yet.
            self.first.push(b);
            if b == b'"' {
                self.quoted = !self.quoted;
            }
            if b == b'\n' && !self.quoted {
                match delimiter_of(&self.first) {
                    Some(d) => {
                        self.delim = d;
                        self.expected = count_fields(&self.first, d);
                        self.records = 1;
                        self.good_len = end;
                        self.good_count = 1;
                        self.records_at_good = 1;
                        self.first = Vec::new();
                        self.fields = 1;
                        self.started = false;
                        self.quoted = false;
                    }
                    None => self.done = true,
                }
            } else if self.first.len() > (1 << 20) {
                self.done = true;
            }
            return;
        }
        match b {
            b'"' => {
                self.quoted = !self.quoted;
                self.started = true;
            }
            b'\n' if !self.quoted => self.end_record(end, false),
            b'\r' if !self.quoted => {}
            d if d == self.delim && !self.quoted => {
                self.fields += 1;
                self.started = true;
            }
            _ => self.started = true,
        }
    }

    /// A record ended at `end` (a line break, or the text stopping).
    fn end_record(&mut self, end: u64, text_stopped: bool) {
        if self.records == 0 {
            // The first record had no line break before the text stopped.
            if !self.first.is_empty() {
                self.done = true;
            }
            return;
        }
        if !self.started {
            // A blank line: not a record, and not the end either.
            return;
        }
        let ok = self.fields == self.expected && !self.quoted;
        self.records += 1;
        if ok {
            self.good_len = end;
            self.good_count += 1;
            self.records_at_good = self.records;
            self.bad_in_a_row = 0;
        } else {
            self.bad_in_a_row += 1;
            if self.bad_in_a_row >= MAX_BAD_IN_A_ROW || text_stopped {
                self.done = true;
            }
        }
        self.fields = 1;
        self.started = false;
        self.quoted = false;
    }

    /// The length and delimiter of the file, if it is one.
    fn finish(self) -> Option<(u64, u8)> {
        // Most records up to the end must have the right shape: a few odd
        // rows are normal, a text that only now and then fits is not a table.
        let shaped = self.good_count * 10 >= self.records_at_good * 9;
        (self.good_count >= MIN_RECORDS && self.expected >= MIN_FIELDS && shaped)
            .then_some((self.good_len, self.delim))
    }
}

/// Fields in a record, counting delimiters outside quotes.
fn count_fields(row: &[u8], delim: u8) -> usize {
    let mut quoted = false;
    let mut n = 1;
    for &b in row {
        match b {
            b'"' => quoted = !quoted,
            d if d == delim && !quoted => n += 1,
            _ => {}
        }
    }
    n
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::HEAD_LEN;
    use super::*;

    pub fn build(rows: usize, delim: char, end_with_newline: bool) -> Vec<u8> {
        let mut s = format!("id{d}name{d}city{d}note\r\n", d = delim);
        for i in 0..rows {
            s.push_str(&format!(
                "{i}{d}\"Person, {i}\"{d}Delhi{d}\"line one\nline two\"",
                d = delim
            ));
            if i + 1 < rows || end_with_newline {
                s.push_str("\r\n");
            }
        }
        s.into_bytes()
    }

    fn carve(data: &[u8]) -> Option<(u64, &'static str)> {
        let mut d = data.to_vec();
        d.resize(d.len().max(HEAD_LEN), 0);
        if !Delimited.matches(&d[..HEAD_LEN]) {
            return None;
        }
        Delimited
            .carve(&mut Reader::new(&d, 0), 0, d.len() as u64)
            .map(|f| (f.len, f.ext))
    }

    #[test]
    fn finds_the_whole_file_amid_zeros_and_junk() {
        for (delim, ext) in [(',', "csv"), (';', "csv"), ('\t', "tsv")] {
            let good = build(400, delim, true);
            let mut with_zeros = good.clone();
            with_zeros.extend(vec![0u8; 5000]);
            assert_eq!(
                carve(&with_zeros),
                Some((good.len() as u64, ext)),
                "{delim:?}"
            );
            // Other stale data after it, which is not text.
            let mut with_junk = good.clone();
            with_junk.extend(
                (0..4000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 21) as u8 | 0x80 | 1),
            );
            with_junk.extend([0u8; 600]);
            let (len, _) = carve(&with_junk).unwrap();
            assert!(len >= good.len() as u64, "{len}");
        }
    }

    #[test]
    fn a_last_row_without_a_line_break_counts() {
        let good = build(50, ',', false);
        let mut d = good.clone();
        d.extend(vec![0u8; 2000]);
        assert_eq!(carve(&d), Some((good.len() as u64, "csv")));
    }

    #[test]
    fn a_file_under_a_sector_is_found() {
        let good = b"a,b,c\r\n1,2,3\r\n4,5,6\r\n7,8,9\r\n".to_vec();
        let mut d = good.clone();
        d.extend(vec![0u8; 1000]);
        assert_eq!(carve(&d), Some((good.len() as u64, "csv")));
    }

    #[test]
    fn stops_where_the_columns_change() {
        let good = build(100, ',', true);
        let mut d = good.clone();
        d.extend(b"this is something else entirely\r\nthat is just prose\r\nwith no columns\r\nat all\r\nreally\r\n");
        d.extend(vec![0u8; 3000]);
        assert_eq!(carve(&d), Some((good.len() as u64, "csv")));
    }

    #[test]
    fn is_not_fooled_by_other_data() {
        // Prose, code, binary, a header alone, one column.
        let prose = "Dear all, thanks for coming, it was lovely, truly.\nSee you soon, take care, bye.\nWith love, and thanks, again.\n".repeat(30);
        assert_eq!(carve(prose.as_bytes()), None);
        let mut binary: Vec<u8> = (0..8000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        binary[0] = b'a';
        assert_eq!(carve(&binary), None);
        assert_eq!(carve(b"id,name,city\r\n"), None);
        assert_eq!(
            carve("one\ntwo\nthree\nfour\nfive\n".repeat(40).as_bytes()),
            None
        );
        assert_eq!(carve(&vec![0u8; 4096]), None);
    }

    #[test]
    fn survives_damage_without_panicking() {
        let good = build(200, ',', true);
        let mut x = 0x9E37_79B9u32;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..300 {
            let mut d = good.clone();
            for _ in 0..(rand() % 20) {
                let at = rand() as usize % d.len();
                d[at] = rand() as u8;
            }
            d.truncate(rand() as usize % (d.len() + 1));
            let _ = carve(&d);
        }
    }
}

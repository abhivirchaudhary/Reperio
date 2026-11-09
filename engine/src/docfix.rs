//! Repairing documents that won't open.
//!
//! - **Word, Excel and PowerPoint (ZIP).** A ZIP's index (the central
//!   directory) is at its end, but every entry also has its own header in
//!   front of its data. With the index lost, the headers are walked,
//!   every entry is inflated and checked against its checksum, and a new
//!   ZIP is written from the whole ones. A part cut short (the main text
//!   of a document, say) is trimmed to its last complete paragraph or
//!   row and closed, so Office opens what survived; its text is also
//!   given as plain text.
//! - **PDF.** Readers find objects through a table of offsets near the
//!   end. With that table damaged, the file is scanned for `N G obj`,
//!   a new table is appended and readers open it. Text is read from page
//!   streams that can't be shown and given as plain text.
//! - **Old Word (.doc).** The text is read from the document stream
//!   through its piece table; failing that, text runs are picked out of
//!   the file.
//!
//! Everything read here is untrusted: sizes are checked against the data
//! at hand, output is capped, and no loop runs without bound.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use flate2::read::{DeflateDecoder, ZlibDecoder};
use serde::{Deserialize, Serialize};

use crate::formats::crc::crc32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocRepair {
    /// A ZIP-based document written again from its intact entries.
    Zip,
    /// A PDF with a new table of objects.
    Pdf,
    /// Only the text could be rescued.
    Text,
}

impl DocRepair {
    /// Extension of the repaired file, when it differs from the damaged one's.
    pub fn ext(self) -> Option<&'static str> {
        match self {
            DocRepair::Text => Some("txt"),
            _ => None,
        }
    }

    /// Added to the name of what was rescued, when it is less than the document.
    pub fn suffix(self) -> &'static str {
        match self {
            DocRepair::Text => "(text)",
            _ => "",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            DocRepair::Zip => {
                "Repaired: the document was written again from the parts that survived."
            }
            DocRepair::Pdf => "Repaired: the PDF's table of contents was rebuilt.",
            DocRepair::Text => "The document could not be repaired; its text was rescued.",
        }
    }
}

pub struct Fixed {
    pub data: Vec<u8>,
    /// Text rescued from parts that could not be repaired, to keep beside
    /// the repaired file.
    pub text: Option<String>,
    pub repair: DocRepair,
}

/// Most bytes inflated from one part, and from a whole document.
const MAX_PART: u64 = 512 << 20;
const MAX_TOTAL: u64 = 1 << 30;
/// Most entries or objects considered.
const MAX_ITEMS: usize = 200_000;

/// Repairs a damaged document. Returns `None` when it is intact or
/// nothing could be rescued.
pub fn repair(data: &[u8], ext: &str) -> Option<Fixed> {
    if data.starts_with(b"PK") {
        return repair_zip(data, ext);
    }
    if data.starts_with(b"%PDF-") || ext == "pdf" {
        return repair_pdf(data);
    }
    // An old Word file keeps its text wherever the start went.
    if data.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) || ext == "doc" {
        let text = doc_text(data)?;
        return Some(Fixed {
            data: text.into_bytes(),
            text: None,
            repair: DocRepair::Text,
        });
    }
    // The start is lost: a ZIP or PDF by extension.
    match ext {
        "docx" | "xlsx" | "pptx" | "zip" => repair_zip(data, ext),
        _ => None,
    }
}

// ------------------------------------------------------------------ ZIP

struct Entry {
    name: String,
    /// The compressed bytes, as stored, and the method.
    raw: Vec<u8>,
    method: u16,
    crc: u32,
    size: u32,
    /// The inflated contents, for parts kept in memory.
    whole: bool,
    /// What inflated, for parts that did not do so completely.
    partial: Vec<u8>,
}

fn le16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn le32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Inflates up to `cap` bytes. Returns what came out, whether the stream
/// ended properly, and how many input bytes it used.
fn inflate(raw: &[u8], zlib: bool, cap: u64) -> (Vec<u8>, bool, u64) {
    let mut out = Vec::new();
    let (ok, used) = if zlib {
        let mut d = ZlibDecoder::new(raw).take(cap);
        (read_all_lenient(&mut d, &mut out), d.get_ref().total_in())
    } else {
        let mut d = DeflateDecoder::new(raw).take(cap);
        (read_all_lenient(&mut d, &mut out), d.get_ref().total_in())
    };
    let complete = ok && (out.len() as u64) < cap;
    (out, complete, used)
}

/// Reads until the end or an error, keeping what was read. True if the
/// stream ended without an error.
fn read_all_lenient(r: &mut impl Read, out: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 16 << 10];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => out.extend(&buf[..n]),
            Err(_) => return false,
        }
    }
}

/// Entries found by walking local headers.
fn walk_local_headers(data: &[u8]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut total = 0u64;
    let mut pos = 0usize;
    while let Some(i) = memchr::memmem::find(&data[pos.min(data.len())..], b"PK\x03\x04") {
        let at = pos + i;
        pos = at + 4;
        if out.len() >= MAX_ITEMS {
            break;
        }
        let Some(e) = local_entry(data, at, &mut total) else {
            continue;
        };
        // Skip over a good entry; a damaged one may have junk in it.
        pos = pos.max(at + 30 + e.name.len() + e.raw.len());
        out.push(e);
    }
    out
}

fn local_entry(data: &[u8], at: usize, total: &mut u64) -> Option<Entry> {
    let flags = le16(data, at + 6)?;
    let method = le16(data, at + 8)?;
    let crc = le32(data, at + 14)?;
    let csize = le32(data, at + 18)? as usize;
    let usize_ = le32(data, at + 22)?;
    let nlen = usize::from(le16(data, at + 26)?);
    let xlen = usize::from(le16(data, at + 28)?);
    if nlen == 0 || flags & 1 != 0 || !matches!(method, 0 | 8) {
        return None;
    }
    let name = String::from_utf8_lossy(data.get(at + 30..at + 30 + nlen)?).into_owned();
    if name.chars().any(|c| c.is_control()) || name.starts_with('/') || name.contains("..") {
        return None;
    }
    let start = at + 30 + nlen + xlen;
    let rest = data.get(start..)?;
    let mut e = Entry {
        name,
        raw: Vec::new(),
        method,
        crc,
        size: usize_,
        whole: false,
        partial: Vec::new(),
    };
    let known = flags & 8 == 0 && csize > 0 || (flags & 8 == 0 && csize == 0 && usize_ == 0);
    if method == 0 {
        // Stored: the sizes must be in the header.
        let n = if known { csize } else { return None };
        let raw = rest.get(..n)?;
        e.whole = crc32(raw) == crc;
        e.raw = raw.to_vec();
        e.partial = raw.to_vec();
        return Some(e);
    }
    let cap = MAX_PART.min(MAX_TOTAL.saturating_sub(*total));
    if known && csize <= rest.len() {
        let raw = &rest[..csize];
        let (out, ended, _) = inflate(raw, false, cap);
        if ended && out.len() == usize_ as usize && crc32(&out) == crc {
            e.whole = true;
            e.raw = raw.to_vec();
            *total += out.len() as u64;
            e.partial = out;
            return Some(e);
        }
        // Damaged inside, or the size field is wrong: the inflated part.
        if out.is_empty() {
            return None;
        }
        *total += out.len() as u64;
        e.raw = raw.to_vec();
        e.partial = out;
        return Some(e);
    }
    // Sizes after the data (a data descriptor), or damaged: let the
    // deflate stream say where it ends.
    let (out, ended, used) = inflate(rest, false, cap);
    if out.is_empty() {
        return None;
    }
    *total += out.len() as u64;
    e.size = out.len() as u32;
    e.crc = crc32(&out);
    if ended {
        e.whole = true;
        e.raw = rest[..used as usize].to_vec();
        // A descriptor with the real checksum follows; trust it if there.
        let d = rest.get(used as usize..)?;
        let d = d.strip_prefix(b"PK\x07\x08").unwrap_or(d);
        if let Some(c) = le32(d, 0) {
            if c != e.crc {
                e.whole = false;
            }
        }
    } else {
        e.raw = rest[..(used as usize).min(rest.len())].to_vec();
    }
    e.partial = out;
    Some(e)
}

fn kind_of_package(names: &[&str]) -> Option<&'static str> {
    let any = |p: &str| names.iter().any(|n| n.starts_with(p));
    if any("word/") {
        Some("docx")
    } else if any("xl/") {
        Some("xlsx")
    } else if any("ppt/") {
        Some("pptx")
    } else {
        None
    }
}

/// The part that holds a document's main content, and how its XML is
/// closed after being cut at its last complete unit.
fn main_part(kind: &str, name: &str) -> Option<(&'static str, &'static str)> {
    match (kind, name) {
        ("docx", "word/document.xml") => Some(("</w:p>", "</w:body></w:document>")),
        ("xlsx", n) if n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml") => {
            Some(("</row>", "</sheetData></worksheet>"))
        }
        _ => None,
    }
}

fn repair_zip(data: &[u8], ext: &str) -> Option<Fixed> {
    let mut entries = walk_local_headers(data);
    if entries.is_empty() {
        return None;
    }
    // Later entries of the same name replace earlier ones (a stale copy).
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        match by_name.get(&e.name) {
            Some(&j) if entries[j].whole && !e.whole => {}
            _ => {
                by_name.insert(e.name.clone(), i);
            }
        }
    }
    let names: Vec<String> = by_name.keys().cloned().collect();
    let kind =
        kind_of_package(&names.iter().map(String::as_str).collect::<Vec<_>>()).or(match ext {
            "docx" => Some("docx"),
            "xlsx" => Some("xlsx"),
            "pptx" => Some("pptx"),
            _ => None,
        });
    let all_whole = by_name.values().all(|&i| entries[i].whole);
    if all_whole && has_directory(data) {
        // Nothing wrong that Reperio can see.
        return None;
    }

    let mut text = String::new();
    let mut out_entries: Vec<(String, Vec<u8>, u16, u32, u32)> = Vec::new();
    let mut damaged_main = false;
    for (name, &i) in &by_name {
        let e = &mut entries[i];
        if e.whole {
            out_entries.push((
                name.clone(),
                std::mem::take(&mut e.raw),
                e.method,
                e.crc,
                e.size,
            ));
            continue;
        }
        // A part cut short or damaged.
        let kind_s = kind.unwrap_or("");
        if let Some((unit, close)) = main_part(kind_s, name) {
            if let Some(cut) = memchr::memmem::rfind(&e.partial, unit.as_bytes()) {
                let mut xml = e.partial[..cut + unit.len()].to_vec();
                xml.extend(close.as_bytes());
                let crc = crc32(&xml);
                let size = xml.len() as u32;
                out_entries.push((name.clone(), deflate_store(&xml), 8, crc, size));
                damaged_main = true;
                text.push_str(&xml_text(&xml));
                continue;
            }
        }
        // A part nothing can be done with: its text goes to the .txt.
        if kind.is_some() && (name.ends_with(".xml")) && !e.partial.is_empty() {
            let t = xml_text(&e.partial);
            if !t.trim().is_empty() {
                text.push_str(&t);
                damaged_main = true;
            }
        }
    }
    if out_entries.is_empty() {
        return text_only(text);
    }
    if let Some(k) = kind {
        synthesise_package(k, &mut out_entries);
    }
    let zip = write_zip(&out_entries)?;
    let text = (damaged_main && !text.trim().is_empty()).then_some(text);
    Some(Fixed {
        data: zip,
        text,
        repair: DocRepair::Zip,
    })
}

fn text_only(text: String) -> Option<Fixed> {
    (!text.trim().is_empty()).then(|| Fixed {
        data: text.into_bytes(),
        text: None,
        repair: DocRepair::Text,
    })
}

/// Whether the ZIP's own index is intact: the end record points at a
/// central directory that starts with its signature.
fn has_directory(data: &[u8]) -> bool {
    let tail = &data[data.len().saturating_sub(70_000)..];
    let Some(i) = memchr::memmem::rfind(tail, b"PK\x05\x06") else {
        return false;
    };
    let base = data.len() - tail.len() + i;
    let (Some(size), Some(off)) = (le32(data, base + 12), le32(data, base + 16)) else {
        return false;
    };
    (off as usize).checked_add(size as usize) == Some(base)
        && data.get(off as usize..off as usize + 4) == Some(b"PK\x01\x02")
}

/// Stores `xml` as a deflate stream.
fn deflate_store(xml: &[u8]) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    use std::io::Write;
    let mut e = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = e.write_all(xml);
    e.finish().unwrap_or_default()
}

/// Adds the files an Office document needs to be recognised, if lost.
fn synthesise_package(kind: &str, entries: &mut Vec<(String, Vec<u8>, u16, u32, u32)>) {
    let has =
        |entries: &[(String, Vec<u8>, u16, u32, u32)], n: &str| entries.iter().any(|e| e.0 == n);
    let add = |entries: &mut Vec<_>, name: &str, body: String| {
        let b = body.into_bytes();
        entries.push((
            name.to_string(),
            deflate_store(&b),
            8u16,
            crc32(&b),
            b.len() as u32,
        ));
    };
    let main = match kind {
        "docx" => "word/document.xml",
        "xlsx" => "xl/workbook.xml",
        _ => "ppt/presentation.xml",
    };
    if !has(entries, main) {
        return;
    }
    if !has(entries, "_rels/.rels") {
        let rel = match kind {
            "docx" | "pptx" => "officeDocument",
            _ => "officeDocument",
        };
        add(
            entries,
            "_rels/.rels",
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{rel}\" Target=\"{main}\"/></Relationships>"
            ),
        );
    }
    if !has(entries, "[Content_Types].xml") {
        let ct = match kind {
            "docx" => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
            }
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
            _ => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
            }
        };
        let mut overrides = format!("<Override PartName=\"/{main}\" ContentType=\"{ct}\"/>");
        let names: Vec<String> = entries.iter().map(|e| e.0.clone()).collect();
        for n in names {
            let t = match n.as_str() {
                "word/styles.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml",
                ),
                "xl/sharedStrings.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml",
                ),
                "xl/styles.xml" => {
                    Some("application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml")
                }
                n if n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml") => Some(
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml",
                ),
                n if n.starts_with("ppt/slides/slide") && n.ends_with(".xml") => {
                    Some("application/vnd.openxmlformats-officedocument.presentationml.slide+xml")
                }
                _ => None,
            };
            if let Some(t) = t {
                overrides.push_str(&format!(
                    "<Override PartName=\"/{n}\" ContentType=\"{t}\"/>"
                ));
            }
        }
        add(
            entries,
            "[Content_Types].xml",
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>{overrides}</Types>"
            ),
        );
    }
    // [Content_Types].xml first, as Office writes it.
    entries.sort_by_key(|e| (e.0 != "[Content_Types].xml", e.0 != "_rels/.rels"));
}

fn write_zip(entries: &[(String, Vec<u8>, u16, u32, u32)]) -> Option<Vec<u8>> {
    if entries.len() > 65_000 {
        return None;
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, raw, method, crc, size) in entries {
        let off = u32::try_from(out.len()).ok()?;
        let mut h = Vec::new();
        h.extend(20u16.to_le_bytes());
        h.extend(0x0800u16.to_le_bytes()); // names are UTF-8
        h.extend(method.to_le_bytes());
        h.extend([0, 0, 0x21, 0]); // 1980-01-01
        h.extend(crc.to_le_bytes());
        h.extend((u32::try_from(raw.len()).ok()?).to_le_bytes());
        h.extend(size.to_le_bytes());
        h.extend((name.len() as u16).to_le_bytes());
        h.extend(0u16.to_le_bytes());
        out.extend(b"PK\x03\x04");
        out.extend(&h);
        out.extend(name.as_bytes());
        out.extend(raw);
        central.extend(b"PK\x01\x02");
        central.extend(20u16.to_le_bytes());
        central.extend(&h[..26]);
        // comment length, disk, attributes, offset
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(off.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let cd_off = u32::try_from(out.len()).ok()?;
    let cd_len = u32::try_from(central.len()).ok()?;
    out.extend(central);
    out.extend(b"PK\x05\x06\0\0\0\0");
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_len.to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    Some(out)
}

/// Plain text from WordprocessingML, SpreadsheetML or DrawingML: the text
/// elements (`t`), with a line break after each paragraph or row.
fn xml_text(xml: &[u8]) -> String {
    let s = String::from_utf8_lossy(xml);
    let mut out = String::new();
    let mut rest: &str = &s;
    let mut in_text = false;
    while let Some(i) = rest.find('<') {
        if in_text {
            out.push_str(&unescape(&rest[..i]));
        }
        rest = &rest[i + 1..];
        let Some(j) = rest.find('>') else { break };
        let tag = &rest[..j];
        rest = &rest[j + 1..];
        let name = tag
            .trim_start_matches('/')
            .split([' ', '/'])
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or("");
        let closing = tag.starts_with('/');
        match local {
            "t" if !closing && !tag.ends_with('/') => in_text = true,
            "t" => in_text = false,
            "p" | "row" | "si" | "tr" if closing => out.push('\n'),
            "br" | "tab" if !closing => out.push(if local == "tab" { '\t' } else { '\n' }),
            _ => {}
        }
        if out.len() > 64 << 20 {
            break;
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ------------------------------------------------------------------ PDF

#[derive(Debug, Clone)]
struct Obj {
    num: u32,
    gen: u32,
    /// Offset of the `N G obj` line, and the end of `endobj`.
    start: usize,
    end: usize,
}

/// Whether `data[at..]` begins an indirect object header; returns its
/// number, generation and the offset just after `obj`.
fn obj_header(data: &[u8], at: usize) -> Option<(u32, u32, usize)> {
    let mut p = at;
    let num = number(data, &mut p)?;
    if !data.get(p)?.is_ascii_whitespace() {
        return None;
    }
    while data.get(p)?.is_ascii_whitespace() {
        p += 1;
    }
    let gen = number(data, &mut p)?;
    while data.get(p)?.is_ascii_whitespace() {
        p += 1;
    }
    if data.get(p..p + 3)? != b"obj" {
        return None;
    }
    Some((num, gen, p + 3))
}

fn number(data: &[u8], p: &mut usize) -> Option<u32> {
    let s = *p;
    while data.get(*p).is_some_and(u8::is_ascii_digit) {
        *p += 1;
        if *p - s > 10 {
            return None;
        }
    }
    if *p == s {
        return None;
    }
    std::str::from_utf8(&data[s..*p]).ok()?.parse().ok()
}

/// Every `N G obj ... endobj` in the file, in file order.
fn scan_objects(data: &[u8]) -> Vec<Obj> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = memchr::memmem::find(&data[pos..], b"obj") {
        let at = pos + i;
        pos = at + 3;
        if out.len() >= MAX_ITEMS {
            break;
        }
        // Back over "N G " to the start of the header.
        let mut s = at;
        let mut fields = 0;
        while s > 0 && fields < 2 {
            let mut t = s;
            while t > 0 && data[t - 1].is_ascii_whitespace() {
                t -= 1;
            }
            let digits = t;
            while t > 0 && data[t - 1].is_ascii_digit() {
                t -= 1;
            }
            if t == digits {
                break;
            }
            s = t;
            fields += 1;
        }
        if fields < 2 || (s > 0 && !data[s - 1].is_ascii_whitespace() && data[s - 1] != b'>') {
            continue;
        }
        let Some((num, gen, body)) = obj_header(data, s) else {
            continue;
        };
        if body != at + 3 {
            continue;
        }
        // The end: `endobj`, but a stream's data may hold the word, so
        // when there is a stream go by its `endstream` first.
        let rest = &data[body..];
        let stream = memchr::memmem::find(rest, b"stream");
        let first_end = memchr::memmem::find(rest, b"endobj");
        let end = match (stream, first_end) {
            (Some(st), Some(e)) if st < e => {
                let after = body + st;
                memchr::memmem::find(&data[after..], b"endstream").and_then(|k| {
                    let from = after + k + 9;
                    memchr::memmem::find(&data[from..], b"endobj").map(|e| from + e + 6)
                })
            }
            (_, Some(e)) => Some(body + e + 6),
            _ => None,
        };
        if let Some(end) = end {
            out.push(Obj {
                num,
                gen,
                start: s,
                end,
            });
            pos = end;
        }
    }
    out
}

/// The dictionary text of an object (before any stream).
fn dict_of<'a>(data: &'a [u8], o: &Obj) -> &'a [u8] {
    let body = &data[o.start..o.end];
    let end = memchr::memmem::find(body, b"stream").unwrap_or(body.len());
    &body[..end]
}

fn has_key(dict: &[u8], key: &[u8]) -> bool {
    memchr::memmem::find_iter(dict, key).any(|i| {
        !dict
            .get(i + key.len())
            .is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// The stream data of an object, if it has one that ends properly.
fn stream_of<'a>(data: &'a [u8], o: &Obj) -> Option<&'a [u8]> {
    let body = &data[o.start..o.end];
    let i = memchr::memmem::find(body, b"stream")?;
    let mut s = i + 6;
    if body.get(s) == Some(&b'\r') {
        s += 1;
    }
    if body.get(s) == Some(&b'\n') {
        s += 1;
    }
    let e = memchr::memmem::rfind(body, b"endstream").unwrap_or(body.len());
    body.get(s..e.max(s))
}

/// Whether the file's cross-reference table is usable: `startxref` points
/// at a table or cross-reference stream, and a table's entries point at
/// the objects they name.
fn xref_ok(data: &[u8]) -> bool {
    let tail = &data[data.len().saturating_sub(2048)..];
    let Some(i) = memchr::memmem::rfind(tail, b"startxref") else {
        return false;
    };
    let mut p = data.len() - tail.len() + i + 9;
    while data.get(p).is_some_and(u8::is_ascii_whitespace) {
        p += 1;
    }
    let Some(off) = number(data, &mut p).map(|n| n as usize) else {
        return false;
    };
    let Some(at) = data.get(off..) else {
        return false;
    };
    if at.starts_with(b"xref") {
        // Check a sample of entries.
        let mut p = off + 4;
        let mut checked = 0;
        loop {
            while data.get(p).is_some_and(u8::is_ascii_whitespace) {
                p += 1;
            }
            let (Some(first), true) = (number(data, &mut p), data.get(p) == Some(&b' ')) else {
                return checked > 0;
            };
            p += 1;
            let Some(count) = number(data, &mut p) else {
                return false;
            };
            while data.get(p).is_some_and(u8::is_ascii_whitespace) {
                p += 1;
            }
            for k in 0..count.min(5000) {
                let Some(line) = data.get(p..p + 20) else {
                    return false;
                };
                let Ok(s) = std::str::from_utf8(&line[..18]) else {
                    return false;
                };
                let (Ok(o), Some(kind)) = (s[..10].parse::<usize>(), line.get(17)) else {
                    return false;
                };
                if *kind == b'n' {
                    match obj_header(data, o) {
                        Some((n, _, _)) if n == first + k => {}
                        _ => return false,
                    }
                    checked += 1;
                }
                p += 20;
            }
            if count > 5000 {
                return true;
            }
        }
    }
    obj_header(data, off).is_some()
}

fn repair_pdf(data: &[u8]) -> Option<Fixed> {
    if data.len() < 32 {
        return None;
    }
    let objs = scan_objects(data);
    // Streams the page content is in: inflate each, noting those cut short.
    let mut damaged_streams = 0;
    let mut text = String::new();
    let mut pages = 0;
    let mut last: BTreeMap<u32, Obj> = BTreeMap::new();
    for o in &objs {
        last.insert(o.num, o.clone());
    }
    let mut total = 0u64;
    for o in last.values() {
        let dict = dict_of(data, o);
        if has_key(dict, b"/Page") && memchr::memmem::find(dict, b"/Type").is_some() {
            pages += 1;
        }
        if memchr::memmem::find(dict, b"/FlateDecode").is_none() {
            continue;
        }
        if has_key(dict, b"/ObjStm") || has_key(dict, b"/XRef") || has_key(dict, b"/Subtype") {
            continue;
        }
        let Some(raw) = stream_of(data, o) else {
            continue;
        };
        let (out, ended, _) = inflate(raw, true, MAX_PART.min(MAX_TOTAL.saturating_sub(total)));
        total += out.len() as u64;
        if !ended {
            damaged_streams += 1;
        }
        if looks_like_content(&out) && (!ended || pages == 0) {
            text.push_str(&pdf_text(&out));
            text.push('\n');
        }
    }
    let intact = xref_ok(data);
    if intact && damaged_streams == 0 {
        return None;
    }
    if intact {
        // The table is fine; only the text of cut streams is rescued.
        return text_only(text);
    }
    if last.is_empty() {
        return None;
    }

    // Objects inside object streams are written out whole.
    let mut extra: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut total = 0u64;
    for o in last.values() {
        let dict = dict_of(data, o);
        if !has_key(dict, b"/ObjStm") {
            continue;
        }
        let Some(raw) = stream_of(data, o) else {
            continue;
        };
        let (out, _, _) = inflate(raw, true, MAX_PART.min(MAX_TOTAL.saturating_sub(total)));
        total += out.len() as u64;
        let n = int_after(dict, b"/N").unwrap_or(0).min(100_000) as usize;
        let first = int_after(dict, b"/First").unwrap_or(0) as usize;
        let mut p = 0;
        let mut pairs = Vec::new();
        for _ in 0..n {
            let (Some(num), Some(off)) = (number_at(&out, &mut p), number_at(&out, &mut p)) else {
                break;
            };
            pairs.push((num, off as usize));
        }
        for (k, &(num, off)) in pairs.iter().enumerate() {
            let from = first.saturating_add(off);
            let to = pairs
                .get(k + 1)
                .map_or(out.len(), |n| first.saturating_add(n.1));
            if let Some(body) = out.get(from..to.min(out.len())).filter(|_| from < to) {
                if !last.contains_key(&num) {
                    extra.push((num, body.to_vec()));
                }
            }
        }
    }

    let mut max = last.keys().next_back().copied().unwrap_or(0);
    for (n, _) in &extra {
        max = max.max(*n);
    }
    let mut out = data.to_vec();
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let mut offsets: HashMap<u32, (usize, u32)> = HashMap::new();
    for o in last.values() {
        // Objects that were found are referenced where they are.
        offsets.insert(o.num, (o.start, o.gen));
    }
    for (num, body) in &extra {
        offsets.insert(*num, (out.len(), 0));
        out.extend(format!("{num} 0 obj\n").as_bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    }

    // Catalog: the object that says so, else made from the page tree.
    let find_by = |pred: &dyn Fn(&[u8]) -> bool| -> Option<u32> {
        offsets
            .iter()
            .filter_map(|(&n, &(at, _))| {
                let body = data.get(at..).or(None)?;
                let end = memchr::memmem::find(&body[..body.len().min(4096)], b"endobj")
                    .unwrap_or(body.len().min(4096));
                pred(&body[..end]).then_some(n)
            })
            .max()
    };
    let is_catalog =
        |b: &[u8]| memchr::memmem::find(b, b"/Type").is_some() && has_key(b, b"/Catalog");
    let mut root = trailer_root(data).filter(|r| offsets.contains_key(r));
    if root.is_none() {
        root = find_by(&is_catalog);
    }
    if root.is_none() {
        // Extra objects (from object streams) are not in `data`.
        for (n, body) in &extra {
            if is_catalog(body) {
                root = Some(*n);
            }
        }
    }
    let root = match root {
        Some(r) => r,
        None => {
            let pages_root = find_by(&|b: &[u8]| {
                has_key(b, b"/Pages")
                    && !has_key(b, b"/Parent")
                    && memchr::memmem::find(b, b"/Kids").is_some()
            });
            let pages_root = match pages_root {
                Some(p) => p,
                None => {
                    let kids: Vec<u32> = last
                        .values()
                        .filter(|o| {
                            let d = dict_of(data, o);
                            has_key(d, b"/Page") && memchr::memmem::find(d, b"/Kids").is_none()
                        })
                        .map(|o| o.num)
                        .collect();
                    if kids.is_empty() {
                        return text_only(text);
                    }
                    max += 1;
                    offsets.insert(max, (out.len(), 0));
                    let list: String = kids.iter().map(|k| format!("{k} 0 R ")).collect();
                    out.extend(
                        format!(
                            "{max} 0 obj\n<< /Type /Pages /Kids [ {list}] /Count {} >>\nendobj\n",
                            kids.len()
                        )
                        .as_bytes(),
                    );
                    max
                }
            };
            max += 1;
            offsets.insert(max, (out.len(), 0));
            out.extend(
                format!("{max} 0 obj\n<< /Type /Catalog /Pages {pages_root} 0 R >>\nendobj\n")
                    .as_bytes(),
            );
            max
        }
    };

    let xref_at = out.len();
    let size = max + 1;
    out.extend(format!("xref\n0 {size}\n").as_bytes());
    out.extend(b"0000000000 65535 f \n");
    for n in 1..size {
        match offsets.get(&n) {
            Some(&(at, gen)) => out.extend(format!("{at:010} {gen:05} n \n").as_bytes()),
            None => out.extend(b"0000000000 65535 f \n"),
        }
    }
    let info = trailer_key(data, b"/Info").filter(|i| offsets.contains_key(i));
    let mut trailer = format!("trailer\n<< /Size {size} /Root {root} 0 R");
    if let Some(i) = info {
        trailer.push_str(&format!(" /Info {i} 0 R"));
    }
    trailer.push_str(" >>\n");
    out.extend(trailer.as_bytes());
    out.extend(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());
    let text = (damaged_streams > 0 && !text.trim().is_empty()).then_some(text);
    Some(Fixed {
        data: out,
        text,
        repair: DocRepair::Pdf,
    })
}

fn number_at(d: &[u8], p: &mut usize) -> Option<u32> {
    while d.get(*p).is_some_and(u8::is_ascii_whitespace) {
        *p += 1;
    }
    number(d, p)
}

fn int_after(dict: &[u8], key: &[u8]) -> Option<u32> {
    let i = memchr::memmem::find(dict, key)? + key.len();
    let mut p = i;
    number_at(dict, &mut p)
}

/// `/Root N 0 R` from the last trailer dictionary that has one.
fn trailer_root(data: &[u8]) -> Option<u32> {
    trailer_key(data, b"/Root")
}

fn trailer_key(data: &[u8], key: &[u8]) -> Option<u32> {
    let mut found = None;
    for t in memchr::memmem::find_iter(data, b"trailer").take(10_000) {
        let dict = &data[t..data.len().min(t + 1024)];
        if let Some(i) = memchr::memmem::find(dict, key) {
            let mut p = i + key.len();
            found = number_at(dict, &mut p).or(found);
        }
    }
    found
}

/// Whether inflated data is a page content stream.
fn looks_like_content(d: &[u8]) -> bool {
    let head = &d[..d.len().min(4096)];
    !head.is_empty()
        && head.iter().filter(|b| b.is_ascii()).count() * 10 >= head.len() * 9
        && (memchr::memmem::find(head, b" Tf").is_some()
            || memchr::memmem::find(head, b"BT").is_some()
            || memchr::memmem::find(head, b" cm").is_some())
}

/// Text shown by a content stream: strings drawn with `Tj`, `TJ`, `'`
/// and `"`. Fonts with their own encodings come out as the codes they use.
fn pdf_text(d: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut line_started = false;
    while i < d.len() && out.len() < 64 << 20 {
        match d[i] {
            b'(' => {
                let (s, next) = literal(d, i);
                out.extend(s.iter().map(|&b| latin1(b)));
                line_started = true;
                i = next;
            }
            b'<' if d.get(i + 1) != Some(&b'<') => {
                let end = d[i..]
                    .iter()
                    .position(|&b| b == b'>')
                    .map_or(d.len(), |k| i + k);
                let hex: Vec<u8> = d[i + 1..end]
                    .iter()
                    .copied()
                    .filter(u8::is_ascii_hexdigit)
                    .collect();
                // Two-byte codes with a zero high byte are Latin-1 text.
                let bytes: Vec<u8> = hex
                    .chunks(2)
                    .filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
                    .collect();
                if bytes.len().is_multiple_of(2) && bytes.iter().step_by(2).all(|&b| b == 0) {
                    out.extend(bytes.iter().skip(1).step_by(2).map(|&b| latin1(b)));
                } else {
                    out.extend(bytes.iter().map(|&b| latin1(b)));
                }
                i = end + 1;
            }
            b'T' if matches!(d.get(i + 1), Some(b'd' | b'D' | b'*')) => {
                if line_started {
                    out.push('\n');
                    line_started = false;
                }
                i += 2;
            }
            b'E' if d.get(i + 1) == Some(&b'T') => {
                if line_started {
                    out.push('\n');
                    line_started = false;
                }
                i += 2;
            }
            b']' => {
                i += 1;
            }
            b'-' | b'0'..=b'9' | b'.' if line_started => {
                // TJ kerning: a big negative number is a word space.
                let s = i;
                while d
                    .get(i)
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'.'))
                {
                    i += 1;
                }
                if let Ok(v) = std::str::from_utf8(&d[s..i]).unwrap_or("").parse::<f32>() {
                    if v < -200.0 {
                        out.push(' ');
                    }
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn latin1(b: u8) -> char {
    match b {
        0x20..=0x7E | 0xA0..=0xFF => char::from(b),
        _ => ' ',
    }
}

/// A literal string starting at the `(` at `i`; returns its bytes and the
/// position after it.
fn literal(d: &[u8], mut i: usize) -> (Vec<u8>, usize) {
    let mut depth = 0;
    let mut out = Vec::new();
    while i < d.len() {
        match d[i] {
            b'(' => {
                depth += 1;
                if depth > 1 {
                    out.push(b'(');
                }
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return (out, i + 1);
                }
                out.push(b')');
            }
            b'\\' => {
                i += 1;
                match d.get(i) {
                    Some(b'n') => out.push(b'\n'),
                    Some(b'r' | b't' | b'b' | b'f') => out.push(b' '),
                    Some(c @ b'0'..=b'7') => {
                        let mut v = u32::from(c - b'0');
                        for _ in 0..2 {
                            match d.get(i + 1) {
                                Some(c @ b'0'..=b'7') => {
                                    v = v * 8 + u32::from(c - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(v as u8);
                    }
                    Some(c) => out.push(*c),
                    None => break,
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    (out, i)
}

// ------------------------------------------------------------------ DOC

/// Text of an old Word document: through its structure when that holds,
/// otherwise by picking text runs out of the file.
fn doc_text(data: &[u8]) -> Option<String> {
    let text = structured_doc_text(data)
        .filter(|t| t.chars().filter(|c| c.is_alphanumeric()).count() >= 8)
        .or_else(|| text_runs(data))?;
    Some(text)
}

/// The streams of a compound file by name, read leniently: a sector that
/// can't be found reads as zeros.
fn ole_streams(data: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    let h = data.get(..512)?;
    let shift = u32::from(le16(h, 30)?);
    if !(7..=16).contains(&shift) {
        return None;
    }
    let ss = 1usize << shift;
    let sector = |s: u32| -> Option<&[u8]> {
        let at = (s as usize).checked_add(1)?.checked_mul(ss)?;
        data.get(at..at.checked_add(ss)?)
    };
    let num_fat = le32(h, 44)? as usize;
    let mut fat_secs: Vec<u32> = (0..109)
        .filter_map(|i| le32(h, 76 + 4 * i))
        .take(num_fat.min(109))
        .collect();
    // DIFAT chain.
    let mut next = le32(h, 68)?;
    for _ in 0..le32(h, 72)?.min(4096) {
        let Some(s) = sector(next) else { break };
        for i in 0..ss / 4 - 1 {
            if fat_secs.len() < num_fat.min(1 << 20) {
                fat_secs.push(le32(s, 4 * i)?);
            }
        }
        next = le32(s, ss - 4)?;
    }
    let mut fat: Vec<u32> = Vec::new();
    for s in fat_secs {
        match sector(s) {
            Some(b) => fat.extend(
                b.chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap())),
            ),
            None => fat.extend(std::iter::repeat_n(0xFFFF_FFFF, ss / 4)),
        }
    }
    let chain = |first: u32, limit: usize| -> Vec<u32> {
        let mut v = Vec::new();
        let mut s = first;
        while (s as usize) < fat.len() && v.len() < limit {
            v.push(s);
            s = fat[s as usize];
        }
        v
    };
    let read_chain = |first: u32| -> Vec<u8> {
        let mut out = Vec::new();
        for s in chain(first, 1 << 22) {
            match sector(s) {
                Some(b) => out.extend(b),
                None => out.extend(std::iter::repeat_n(0u8, ss)),
            }
        }
        out
    };
    let dir = read_chain(le32(h, 48)?);
    let entry = |i: usize| dir.get(i * 128..i * 128 + 128);
    let name = |e: &[u8]| -> String {
        let n = usize::from(le16(e, 64).unwrap_or(0)).min(64);
        let u: Vec<u16> = e[..n.saturating_sub(2)]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u)
    };
    let root = entry(0)?;
    let mini_stream = read_chain(le32(root, 116)?);
    let mini_fat: Vec<u32> = read_chain(le32(h, 60)?)
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let mut out = HashMap::new();
    for i in 1..dir.len() / 128 {
        let Some(e) = entry(i) else { break };
        if e[66] != 2 {
            continue;
        }
        let size =
            usize::try_from(u64::from_le_bytes(e[120..128].try_into().ok()?) & 0xFFFF_FFFF).ok()?;
        let first = le32(e, 116)?;
        let mut body = if size < 4096 {
            let mut v = Vec::new();
            let mut s = first;
            while (s as usize) < mini_fat.len() && v.len() < size + 64 {
                let at = s as usize * 64;
                v.extend(mini_stream.get(at..at + 64).unwrap_or(&[0; 64]));
                s = mini_fat[s as usize];
            }
            v
        } else {
            read_chain(first)
        };
        body.truncate(size);
        out.insert(name(e), body);
    }
    (!out.is_empty()).then_some(out)
}

fn structured_doc_text(data: &[u8]) -> Option<String> {
    let streams = ole_streams(data)?;
    let word = streams.get("WordDocument")?;
    if le16(word, 0)? != 0xA5EC {
        return None;
    }
    let flags = le16(word, 0x0A)?;
    let table = streams.get(if flags & 0x200 != 0 {
        "1Table"
    } else {
        "0Table"
    });
    let mut text = String::new();
    // The piece table in the Table stream says where each run of text is
    // and whether it is 8- or 16-bit.
    if let Some(t) = table {
        let fc_clx = le32(word, 0x01A2)? as usize;
        let lcb = le32(word, 0x01A6)? as usize;
        if let Some(clx) = t.get(fc_clx..fc_clx.saturating_add(lcb).min(t.len())) {
            let mut p = 0;
            while clx.get(p) == Some(&1) {
                p += 3 + usize::from(le16(clx, p + 1)?);
            }
            if clx.get(p) == Some(&2) {
                let plc_len = le32(clx, p + 1)? as usize;
                let plc = clx.get(p + 5..(p + 5).checked_add(plc_len)?)?;
                let n = plc_len.saturating_sub(4) / 12;
                for k in 0..n.min(100_000) {
                    let cp0 = le32(plc, 4 * k)? as usize;
                    let cp1 = le32(plc, 4 * (k + 1))? as usize;
                    let pcd = plc.get(4 * (n + 1) + 8 * k..4 * (n + 1) + 8 * k + 8)?;
                    let fc = le32(pcd, 2)?;
                    let chars = cp1.checked_sub(cp0)?.min(1 << 24);
                    if fc & 0x4000_0000 != 0 {
                        let at = (fc & 0x3FFF_FFFF) as usize / 2;
                        if let Some(b) = word.get(at..at.checked_add(chars)?) {
                            text.extend(b.iter().map(|&c| cp1252(c)));
                        }
                    } else if let Some(b) =
                        word.get(fc as usize..(fc as usize).checked_add(chars * 2)?)
                    {
                        let u: Vec<u16> = b
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        text.push_str(&String::from_utf16_lossy(&u));
                    }
                }
            }
        }
    }
    if text.is_empty() {
        // Without the piece table: the text is one run from fcMin.
        let fc_min = le32(word, 0x18)? as usize;
        let fc_mac = le32(word, 0x1C)? as usize;
        let b = word.get(fc_min..fc_mac.min(word.len()))?;
        text.extend(b.iter().map(|&c| cp1252(c)));
    }
    Some(clean_word_text(&text))
}

fn cp1252(b: u8) -> char {
    match b {
        0x80..=0x9F => {
            const T: [char; 32] = [
                '€', ' ', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', ' ', 'Ž', ' ',
                ' ', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', ' ', 'ž', 'Ÿ',
            ];
            T[usize::from(b - 0x80)]
        }
        _ => char::from(b),
    }
}

/// Word's control characters: paragraph (13) and cell (7) marks become
/// line breaks; fields and others are dropped.
fn clean_word_text(t: &str) -> String {
    let mut out = String::new();
    for c in t.chars() {
        match c {
            '\r' | '\u{7}' | '\u{b}' | '\u{c}' => out.push('\n'),
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Text found by looking for long runs of printable characters, 8-bit and
/// 16-bit.
fn text_runs(data: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= 24 && run.chars().filter(|c| *c == ' ').count() >= 3 {
            out.push_str(run);
            out.push('\n');
        }
        run.clear();
    };
    // 16-bit first: Word 97 and later store most text that way.
    let mut i = 0;
    while i + 1 < data.len() && out.len() < 16 << 20 {
        let (a, b) = (data[i], data[i + 1]);
        if b == 0 && (a == b'\r' || a == b'\t' || (0x20..0x7F).contains(&a)) {
            run.push(if a == b'\r' { '\n' } else { char::from(a) });
            i += 2;
        } else {
            flush(&mut run, &mut out);
            i += 1;
        }
    }
    flush(&mut run, &mut out);
    if out.is_empty() {
        for &b in data {
            if (0x20..0x7F).contains(&b) {
                run.push(char::from(b));
            } else {
                flush(&mut run, &mut out);
            }
            if out.len() > 16 << 20 {
                break;
            }
        }
        flush(&mut run, &mut out);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let entries: Vec<_> = files
            .iter()
            .map(|(n, b)| {
                (
                    n.to_string(),
                    deflate_store(b),
                    8u16,
                    crc32(b),
                    b.len() as u32,
                )
            })
            .collect();
        write_zip(&entries).unwrap()
    }

    #[test]
    fn rebuilds_a_zip_without_its_directory() {
        let doc =
            b"<w:document><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p></w:body></w:document>";
        let z = zip_of(&[("word/document.xml", doc), ("word/styles.xml", b"<s/>")]);
        let cd = memchr::memmem::rfind(&z, b"PK\x01\x02").unwrap();
        let broken = &z[..cd];
        let fixed = repair(broken, "docx").unwrap();
        assert_eq!(fixed.repair, DocRepair::Zip);
        let entries = walk_local_headers(&fixed.data);
        assert_eq!(entries.len(), 4, "two parts and the two package files");
        assert!(has_directory(&fixed.data));
        // Intact documents are left alone.
        assert!(repair(&z, "docx").is_none());
    }

    #[test]
    fn closes_a_cut_main_part() {
        let mut doc = b"<w:document><w:body>".to_vec();
        for i in 0..400 {
            doc.extend(
                format!("<w:p><w:r><w:t>Paragraph number {i} of text</w:t></w:r></w:p>").as_bytes(),
            );
        }
        doc.extend(b"</w:body></w:document>");
        let z = zip_of(&[("word/document.xml", &doc)]);
        // Cut the compressed data in half.
        let cut = &z[..30 + 17 + (z.len() - 30 - 17 - 120) / 2];
        let fixed = repair(cut, "docx").unwrap();
        let entries = walk_local_headers(&fixed.data);
        let main = entries
            .iter()
            .find(|e| e.name == "word/document.xml")
            .unwrap();
        assert!(main.whole);
        assert!(main.partial.ends_with(b"</w:body></w:document>"));
        assert!(fixed.text.unwrap().contains("Paragraph number 0 "));
    }

    #[test]
    fn rebuilds_pdf_tables() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\ntrailer\n<< /Root 1 0 R /Size 4 >>\nstartxref\n9999\n%%EOF\n";
        let fixed = repair(pdf, "pdf").unwrap();
        assert_eq!(fixed.repair, DocRepair::Pdf);
        assert!(xref_ok(&fixed.data));
    }

    #[test]
    fn pulls_text_from_content_streams() {
        let t = pdf_text(
            b"BT /F1 12 Tf 72 700 Td (Hello \\(world\\)) Tj 0 -14 Td [(Foo)-300(bar)] TJ ET",
        );
        assert_eq!(t, "Hello (world)\nFoo bar\n");
    }

    #[test]
    fn survives_garbage() {
        let junk: Vec<u8> = (0..60_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for head in [
            &b"PK\x03\x04"[..],
            b"%PDF-1.4\n",
            b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1",
        ] {
            let mut d = head.to_vec();
            d.extend(&junk);
            let _ = repair(&d, "docx");
            let _ = repair(&d, "pdf");
        }
    }
}

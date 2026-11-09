//! The text of a PDF, for previewing PDFs where nothing renders them.
//!
//! This reads the text drawing operators of each page's content streams.
//! It knows nothing of fonts, so text in fonts with their own encodings
//! (most PDFs made from Word on Windows) comes out unreadable; that is
//! detected and nothing is returned, rather than gibberish.

use memchr::memmem;

/// Most decompressed bytes read from all of a PDF's streams.
const MAX_INFLATED: usize = 64 << 20;
/// Most decompressed bytes from one stream.
const MAX_STREAM: usize = 16 << 20;
/// Most text returned; a preview needs no more.
const MAX_TEXT: usize = 200_000;
/// Most streams looked at.
const MAX_STREAMS: usize = 20_000;

/// The readable text of the PDF in `data`, if any.
pub fn text(data: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut inflated = 0usize;
    let mut pos = 0;
    let mut streams = 0;
    while let Some(i) = memmem::find(&data[pos..], b"stream") {
        let kw = pos + i;
        pos = kw + 6;
        // "endstream" contains "stream"; and the keyword is followed by an
        // end of line.
        if data[..kw].ends_with(b"end") {
            continue;
        }
        let body = match data.get(pos..pos + 2) {
            Some([b'\r', b'\n']) => pos + 2,
            Some([b'\n', _]) | Some([b'\r', _]) => pos + 1,
            _ => continue,
        };
        streams += 1;
        if streams > MAX_STREAMS || out.len() >= MAX_TEXT || inflated >= MAX_INFLATED {
            break;
        }
        let Some(end) = memmem::find(&data[body..], b"endstream").map(|e| body + e) else {
            break;
        };
        pos = end + 9;
        // The stream's dictionary is just before it.
        let dict_start = data[..kw]
            .windows(2)
            .rposition(|w| w == b"<<")
            .filter(|&d| kw - d < 4096)
            .unwrap_or(kw);
        let dict = &data[dict_start..kw];
        // Pictures, fonts and cross-reference tables hold no page text.
        if [
            &b"/Image"[..],
            b"/FontFile",
            b"/Length1",
            b"/XRef",
            b"/Metadata",
        ]
        .iter()
        .any(|k| memmem::find(dict, k).is_some())
        {
            continue;
        }
        let raw = &data[body..end];
        let content = if memmem::find(dict, b"/FlateDecode").is_some() {
            let room = MAX_STREAM.min(MAX_INFLATED - inflated);
            match miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(raw, room) {
                Ok(v) => v,
                Err(e) if !e.output.is_empty() => e.output,
                Err(_) => continue,
            }
        } else if memmem::find(dict, b"/Filter").is_some() {
            continue;
        } else {
            raw.to_vec()
        };
        inflated += content.len();
        page_text(&content, &mut out);
    }
    let out = out.trim().to_string();
    readable(&out).then_some(out)
}

/// Whether text looks like words rather than bytes of a font's own
/// encoding: mostly letters, digits, spaces and punctuation.
fn readable(text: &str) -> bool {
    let total = text.chars().count();
    if total < 2 {
        return false;
    }
    let good = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || ".,;:!?'\"()-%&/".contains(*c))
        .count();
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    good * 10 >= total * 9 && letters * 2 >= total
}

/// Appends the text drawn by one content stream.
fn page_text(content: &[u8], out: &mut String) {
    let mut i = 0;
    let mut in_text = false;
    // Strings since the last operator, with the gaps of TJ arrays.
    let mut pending = String::new();
    let mut last_num = 0f32;
    while i < content.len() && out.len() < MAX_TEXT {
        let c = content[i];
        match c {
            b'(' => {
                let (s, next) = literal(content, i + 1);
                pending.push_str(&s);
                i = next;
            }
            b'<' if content.get(i + 1) != Some(&b'<') => {
                let end = memchr::memchr(b'>', &content[i..]).map_or(content.len(), |e| i + e);
                pending.push_str(&hex(&content[i + 1..end]));
                i = end + 1;
            }
            b'%' => {
                // A comment, to the end of the line.
                i = memchr::memchr2(b'\n', b'\r', &content[i..]).map_or(content.len(), |e| i + e);
            }
            b'-' | b'+' | b'.' | b'0'..=b'9' => {
                let start = i;
                while i < content.len() && matches!(content[i], b'-' | b'+' | b'.' | b'0'..=b'9') {
                    i += 1;
                }
                last_num = std::str::from_utf8(&content[start..i])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                // Inside a TJ array, a large gap between strings is a space.
                if last_num < -180.0 && !pending.is_empty() && !pending.ends_with(' ') {
                    pending.push(' ');
                }
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'\'' | b'"' => {
                let start = i;
                i += 1;
                while i < content.len()
                    && (content[i].is_ascii_alphanumeric() || content[i] == b'*')
                {
                    i += 1;
                }
                match &content[start..i] {
                    b"BT" => in_text = true,
                    b"ET" => {
                        in_text = false;
                        if !out.is_empty() && !out.ends_with(['\n', ' ']) {
                            out.push(' ');
                        }
                    }
                    b"Tj" | b"TJ" if in_text => out.push_str(&pending),
                    b"'" | b"\"" if in_text => {
                        out.push('\n');
                        out.push_str(&pending);
                    }
                    b"T*" => out.push('\n'),
                    b"Td" | b"TD" if last_num != 0.0 => out.push('\n'),
                    b"Tm" if !out.is_empty() && !out.ends_with('\n') => out.push('\n'),
                    _ => {}
                }
                pending.clear();
            }
            _ => i += 1,
        }
    }
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
}

/// A literal string's text, from just after its `(` to just after its `)`.
fn literal(content: &[u8], mut i: usize) -> (String, usize) {
    let mut s = String::new();
    let mut depth = 1;
    while i < content.len() {
        let c = content[i];
        i += 1;
        match c {
            b'\\' => {
                let Some(&e) = content.get(i) else { break };
                i += 1;
                match e {
                    b'n' => s.push('\n'),
                    b'r' | b't' => s.push(' '),
                    b'0'..=b'7' => {
                        let mut v = u32::from(e - b'0');
                        for _ in 0..2 {
                            match content.get(i) {
                                Some(&d @ b'0'..=b'7') => {
                                    v = v * 8 + u32::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        s.push(char::from(v as u8));
                    }
                    b'\n' | b'\r' => {}
                    other => s.push(char::from(other)),
                }
            }
            b'(' => {
                depth += 1;
                s.push('(');
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                s.push(')');
            }
            // PDFDocEncoding is Latin-1 for the printable characters.
            other => s.push(char::from(other)),
        }
    }
    (s, i)
}

/// A hex string's text: one byte per character.
fn hex(digits: &[u8]) -> String {
    let nibbles: Vec<u8> = digits
        .iter()
        .filter_map(|&d| char::from(d).to_digit(16).map(|v| v as u8))
        .collect();
    nibbles
        .chunks(2)
        .map(|p| char::from(p[0] << 4 | p.get(1).copied().unwrap_or(0)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(content: &[u8], compress: bool) -> Vec<u8> {
        let (body, filter) = if compress {
            (
                miniz_oxide::deflate::compress_to_vec_zlib(content, 6),
                " /Filter /FlateDecode",
            )
        } else {
            (content.to_vec(), "")
        };
        let mut v = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n4 0 obj\n".to_vec();
        v.extend(format!("<< /Length {}{} >>\nstream\n", body.len(), filter).as_bytes());
        v.extend(body);
        v.extend(b"\nendstream\nendobj\n%%EOF\n");
        v
    }

    #[test]
    fn reads_text_from_compressed_and_plain_pages() {
        let page =
            b"BT /F1 12 Tf 72 712 Td (Holiday plans) Tj 0 -14 Td [(Rome) -250 (in May)] TJ ET";
        for compress in [true, false] {
            let t = text(&pdf(page, compress)).expect("text");
            assert_eq!(t, "Holiday plans\nRome in May", "compressed: {compress}");
        }
    }

    #[test]
    fn gives_nothing_for_font_encoded_text_or_junk() {
        let page = b"BT <0102030405060708090a0b0c> Tj ET";
        assert_eq!(text(&pdf(page, true)), None);
        assert_eq!(text(&[0xFF; 10_000]), None);
        // An unterminated stream and string must not loop or panic.
        assert_eq!(text(b"<< >>\nstream\nBT (abc"), None);
    }
}

// Example Reperio plug-in in Rust (standard library only, no crates).
//
// Reads a made-up file system called TOYFS so the example can run; replace
// `scan` with your own parsing. Build and install:
//
//     rustc --edition 2021 -O toyfs.rs -o toyfs-rust
//     cp toyfs-rust <a plug-ins folder>        (reperio plugins lists them)
//
// TOYFS layout:
//   0..8   "TOYFS001"        8..12  entry count (u32 LE)
//   then 64-byte entries: name[40], start u64, size u64, flags u32 (1 = deleted)
//
// Protocol (full description: docs/plugins.md): Reperio sends one JSON line
// on stdin; you answer with JSON lines on stdout. To read the drive, print
// {"read":{...}} and read one reply line back. Never open the device yourself.

use std::io::{self, BufRead, Write};

const NAME: &str = "toyfs-rust";
const WANTS: &str = r#"["unknown"]"#; // ntfs, fat, exfat, hfs+, apfs, ext, unknown, any

fn send(line: &str) {
    let mut out = io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
}

/// The number after `"key":` in a line of JSON (enough for Reperio's lines).
fn number_after(line: &str, key: &str) -> Option<u64> {
    let at = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let digits: String = line[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn base64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for v in s.bytes().filter_map(val) {
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Bytes of the volume (offset 0 is its first byte), read through Reperio.
fn read(stdin: &mut impl BufRead, offset: u64, len: u64) -> Vec<u8> {
    send(&format!(r#"{{"read":{{"offset":{offset},"len":{len}}}}}"#));
    let mut reply = String::new();
    stdin.read_line(&mut reply).unwrap();
    match reply.split("\"data\":\"").nth(1) {
        Some(rest) => base64_decode(rest.split('"').next().unwrap_or("")),
        None => Vec::new(), // {"ok":false}
    }
}

fn scan(stdin: &mut impl BufRead) {
    let head = read(stdin, 0, 12);
    if head.len() < 12 || &head[..8] != b"TOYFS001" {
        return; // not ours: say nothing, Reperio moves on
    }
    let count = u32::from_le_bytes(head[8..12].try_into().unwrap()) as u64;
    let table = read(stdin, 12, 64 * count);
    for entry in table.chunks_exact(64) {
        let name = String::from_utf8_lossy(&entry[..40]);
        let name = name.trim_end_matches('\0');
        let start = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        let size = u64::from_le_bytes(entry[48..56].try_into().unwrap());
        let flags = u32::from_le_bytes(entry[56..60].try_into().unwrap());
        send(&format!(
            r#"{{"file":{{"path":"Photos/{name}","size":{size},"deleted":{},"extents":[[{start},{size}]]}}}}"#,
            flags & 1 != 0
        ));
    }
    send(r#"{"done":{"label":"TOYFS volume"}}"#);
}

fn main() {
    let mut stdin = io::stdin().lock();
    let mut line = String::new();
    stdin.read_line(&mut line).unwrap();
    if line.contains("\"info\"") {
        send(&format!(r#"{{"info":{{"name":"{NAME}","fs":{WANTS}}}}}"#));
    } else if line.contains("\"scan\"") {
        let _volume_len = number_after(&line, "len"); // size of the volume, if you need it
        scan(&mut stdin);
    }
}

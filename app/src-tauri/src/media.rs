//! A tiny local web server for playing previews of videos and recordings.
//!
//! WebKit on macOS plays media from the app's `asset:` URLs. WebKitGTK
//! (Linux) and WebView2 (Windows) hand media to GStreamer and Media
//! Foundation, which don't know custom URL schemes, so a video there needs
//! a plain `http://` address that answers range requests (seeking).
//!
//! It only ever serves the files in the preview cache, never listing it,
//! and only to requests that carry the secret path the app made at start
//! up, so other programs and web pages cannot read the previews. It
//! listens on the loopback address only.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the previews are served from: `http://127.0.0.1:PORT/TOKEN/`.
#[derive(Clone)]
pub struct Media {
    base: String,
}

/// A request's head is at most this long.
const MAX_HEAD: usize = 16 << 10;

impl Media {
    /// Starts serving `dir`. `None` if no port could be opened; previews
    /// then fall back to the app's own URLs.
    pub fn start(dir: PathBuf) -> Option<Media> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        let token = token();
        let secret = format!("/{token}/");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (dir, secret) = (dir.clone(), secret.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &dir, &secret);
                });
            }
        });
        Some(Media {
            base: format!("http://127.0.0.1:{port}/{token}/"),
        })
    }

    /// The address of the file `name` in the cache.
    pub fn url(&self, name: &str) -> String {
        format!("{}{}", self.base, name)
    }
}

/// 128 random bits as hex: the hasher's keys come from the system's
/// random source.
fn token() -> String {
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn content_type(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "3gp" => "video/3gpp",
        "3g2" => "video/3gpp2",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "aif" | "aiff" => "audio/aiff",
        _ => "application/octet-stream",
    }
}

fn respond(out: &mut TcpStream, status: &str, extra: &str, len: u64) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}\r\n"
    )
}

fn serve(stream: TcpStream, dir: &Path, secret: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut out = stream.try_clone()?;
    let mut input = BufReader::new(stream.take(MAX_HEAD as u64));
    let mut line = String::new();
    input.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut range = None;
    loop {
        let mut header = String::new();
        if input.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            if k.eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            }
        }
    }
    // Only the secret path, then the name of one file: no folders.
    let name = target
        .strip_prefix(secret)
        .map(|n| n.split('?').next().unwrap_or(""))
        .filter(|n| !n.is_empty() && !n.contains(['/', '\\', '%']) && !n.starts_with('.'));
    let (Some(name), true) = (name, matches!(method, "GET" | "HEAD")) else {
        return respond(&mut out, "404 Not Found", "", 0);
    };
    let Ok(mut file) = std::fs::File::open(dir.join(name)) else {
        return respond(&mut out, "404 Not Found", "", 0);
    };
    let total = file.metadata()?.len();
    let (start, end) = match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
        Some(r) => {
            let (a, b) = r.split_once('-').unwrap_or((r, ""));
            match (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
                (Ok(a), Ok(b)) => (a, b.min(total.saturating_sub(1))),
                (Ok(a), Err(_)) => (a, total.saturating_sub(1)),
                // "-N": the last N bytes.
                (Err(_), Ok(n)) => (total.saturating_sub(n), total.saturating_sub(1)),
                _ => (0, total.saturating_sub(1)),
            }
        }
        None => (0, total.saturating_sub(1)),
    };
    let ctype = content_type(name);
    if total == 0 || start > end || start >= total {
        let extra = format!("Content-Range: bytes */{total}\r\n");
        return respond(&mut out, "416 Range Not Satisfiable", &extra, 0);
    }
    let len = end - start + 1;
    let (status, extra) = if range.is_some() {
        (
            "206 Partial Content",
            format!("Content-Type: {ctype}\r\nContent-Range: bytes {start}-{end}/{total}\r\n"),
        )
    } else {
        ("200 OK", format!("Content-Type: {ctype}\r\n"))
    };
    respond(&mut out, status, &extra, len)?;
    if method == "HEAD" {
        return Ok(());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut left = len;
    let mut buf = vec![0u8; 256 << 10];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(url: &str, range: Option<&str>) -> (String, Vec<u8>) {
        let rest = url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_once('/').unwrap();
        let mut s = TcpStream::connect(host).unwrap();
        let r = range.map_or(String::new(), |r| format!("Range: bytes={r}\r\n"));
        write!(s, "GET /{path} HTTP/1.1\r\nHost: {host}\r\n{r}\r\n").unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        let at = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (
            String::from_utf8_lossy(&all[..at]).into_owned(),
            all[at + 4..].to_vec(),
        )
    }

    #[test]
    fn serves_ranges_of_cache_files_to_those_who_know_the_secret() {
        let dir = std::env::temp_dir().join(format!("reperio-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("1.mp4"), &data).unwrap();
        let media = Media::start(dir.clone()).unwrap();
        let url = media.url("1.mp4");

        let (head, body) = get(&url, None);
        assert!(
            head.starts_with("HTTP/1.1 200") && head.contains("video/mp4"),
            "{head}"
        );
        assert_eq!(body, data);
        let (head, body) = get(&url, Some("100-199"));
        assert!(
            head.starts_with("HTTP/1.1 206") && head.contains("bytes 100-199/10000"),
            "{head}"
        );
        assert_eq!(body, &data[100..200]);
        let (_, body) = get(&url, Some("9990-"));
        assert_eq!(body, &data[9990..]);
        assert!(get(&url, Some("20000-")).0.starts_with("HTTP/1.1 416"));
        // Wrong secret, other folders and missing files get nothing.
        let wrong = url.replace(&media.base[media.base.len() - 33..], "0000/");
        assert!(get(&wrong, None).0.starts_with("HTTP/1.1 404"));
        assert!(get(&media.url("../x"), None).0.starts_with("HTTP/1.1 404"));
        assert!(get(&media.url("nothing.mp4"), None)
            .0
            .starts_with("HTTP/1.1 404"));
        let _ = std::fs::remove_dir_all(dir);
    }
}

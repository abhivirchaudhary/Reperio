//! The Recovery Vault: a record of the files on a protected drive, kept
//! while they are still there, so they can be recognised after they are
//! deleted or the drive is formatted.
//!
//! A deep scan finds deleted photos and videos by their content, but not
//! their names or folders. The vault remembers, for every file, its path,
//! size, date and a fingerprint of its first few kilobytes. After a loss,
//! files found by the deep scan whose size and fingerprint match an entry
//! get their name and folder back.
//!
//! The record is made through the mounted drive with ordinary file reads,
//! so keeping it needs no administrator rights and never touches the
//! drive's structures. It is refreshed incrementally: only files whose size
//! or date changed are read again.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::extent::MappedSource;
use crate::session::{Item, Origin};
use crate::source::Source;

/// Bytes fingerprinted at the start of each file.
pub const HEAD: usize = 4096;
/// Files smaller than this are not worth recording: a fingerprint of a
/// tiny file is not distinctive.
const MIN_SIZE: u64 = 8192;
/// Most files recorded per drive.
const MAX_FILES: usize = 2_000_000;
/// Deepest folder nesting followed.
const MAX_DEPTH: usize = 64;

/// Folders the operating system keeps on drives for its own use.
const SKIP_DIRS: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    "System Volume Information",
    "$RECYCLE.BIN",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Path inside the drive, `/` separated.
    pub path: String,
    pub size: u64,
    /// Modification time, seconds since 1970.
    pub modified: i64,
    /// Fingerprint of the first [`HEAD`] bytes.
    pub head: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Vault {
    /// The drive's volume identifier, as the operating system reports it.
    pub volume: String,
    /// The drive's name when the record was last updated.
    pub label: String,
    /// When the record was last updated, seconds since 1970.
    pub updated: i64,
    pub files: Vec<Entry>,
}

/// 64-bit FNV-1a: stable across versions and platforms, unlike the
/// standard library's hasher, which matters for a record kept on disk.
pub fn fingerprint(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn seconds(t: io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

impl Vault {
    /// Records the files under `root` (a mounted drive), reusing the
    /// fingerprints of files unchanged since `self` was made.
    pub fn refresh(&self, root: &Path) -> io::Result<Vault> {
        let known: HashMap<(&str, u64, i64), u64> = self
            .files
            .iter()
            .map(|e| ((e.path.as_str(), e.size, e.modified), e.head))
            .collect();
        let mut files = Vec::new();
        let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
        while let Some((dir, rel, depth)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if files.len() >= MAX_FILES {
                    break;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let Ok(meta) = entry.metadata() else { continue };
                // Never follow links: they lead off the drive.
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                        stack.push((entry.path(), path, depth + 1));
                    }
                    continue;
                }
                if !meta.is_file() || meta.len() < MIN_SIZE || name.starts_with("._") {
                    continue;
                }
                let modified = seconds(meta.modified());
                let head = match known.get(&(path.as_str(), meta.len(), modified)) {
                    Some(&h) => h,
                    None => {
                        let mut buf = vec![0u8; HEAD];
                        let Ok(mut f) = fs::File::open(entry.path()) else {
                            continue;
                        };
                        let n = read_full(&mut f, &mut buf)?;
                        fingerprint(&buf[..n])
                    }
                };
                files.push(Entry {
                    path,
                    size: meta.len(),
                    modified,
                    head,
                });
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Vault {
            volume: self.volume.clone(),
            label: self.label.clone(),
            updated: seconds(Ok(std::time::SystemTime::now())),
            files,
        })
    }

    pub fn load(path: &Path) -> io::Result<Vault> {
        let f = io::BufReader::new(fs::File::open(path)?);
        serde_json::from_reader(f).map_err(io::Error::other)
    }

    /// Saves the record, replacing the old one only once the new one is
    /// completely written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("partial");
        {
            let f = io::BufWriter::new(fs::File::create(&tmp)?);
            serde_json::to_writer(f, self).map_err(io::Error::other)?;
        }
        fs::rename(tmp, path)
    }

    /// Gives files found by the deep scan their names back. Returns how
    /// many were named.
    pub fn name_found_files(&self, src: &dyn Source, items: &mut [Item]) -> usize {
        let mut by_key: HashMap<(u64, u64), Vec<&Entry>> = HashMap::new();
        for e in &self.files {
            by_key.entry((e.size, e.head)).or_default().push(e);
        }
        let sizes: std::collections::HashSet<u64> = self.files.iter().map(|e| e.size).collect();
        // Names already given back, so two copies of a file get both names.
        let mut used: HashMap<(u64, u64), usize> = HashMap::new();
        let mut named = 0;
        for item in items
            .iter_mut()
            .filter(|i| i.origin == Origin::Carved && i.fix.is_none())
        {
            if !sizes.contains(&item.size) {
                continue;
            }
            let view = MappedSource::new(src, item.extents.clone());
            let mut buf = vec![0u8; HEAD];
            let n = view.read_at(0, &mut buf).unwrap_or(0);
            let key = (item.size, fingerprint(&buf[..n]));
            let Some(entries) = by_key.get(&key) else {
                continue;
            };
            let k = used.entry(key).or_insert(0);
            let entry = entries[*k % entries.len()];
            *k += 1;
            let (dir, name) = match entry.path.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), entry.path.clone()),
            };
            item.name = name;
            item.dir = dir;
            item.origin = Origin::Deleted;
            if item.modified.is_none() {
                item.modified = crate::formats::DateTime::from_unix(entry.modified);
            }
            named += 1;
        }
        named
    }
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // An unreadable file is skipped by its caller via a short read.
            Err(_) => break,
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extent::Extent;
    use crate::formats::Kind;
    use crate::session::Health;

    #[test]
    fn fingerprint_is_stable() {
        // Known FNV-1a 64 values.
        assert_eq!(fingerprint(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fingerprint(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn records_and_names_found_files() {
        let dir = std::env::temp_dir().join(format!("reperio-vault-{}", std::process::id()));
        fs::create_dir_all(dir.join("DCIM/100CANON")).unwrap();
        let photo: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(dir.join("DCIM/100CANON/IMG_0001.JPG"), &photo).unwrap();
        fs::write(dir.join("tiny.txt"), b"too small").unwrap();

        let vault = Vault::default().refresh(&dir).unwrap();
        assert_eq!(vault.files.len(), 1);
        assert_eq!(vault.files[0].path, "DCIM/100CANON/IMG_0001.JPG");
        // Unchanged files keep their fingerprint without being read again.
        let again = vault.refresh(&dir).unwrap();
        assert_eq!(again.files, vault.files);

        // The card after a format: the photo found by deep scan, unnamed.
        let mut card = vec![0u8; 4096];
        card.extend(&photo);
        let mut items = vec![Item {
            id: 0,
            name: "Photo 8.jpg".into(),
            dir: String::new(),
            ext: "jpg".into(),
            kind: Kind::Photo,
            origin: Origin::Carved,
            health: Health::Good,
            size: photo.len() as u64,
            taken: None,
            modified: None,
            extents: vec![Extent::new(4096, photo.len() as u64)],
            volume: None,
            fix: None,
            found_by: None,
            nsrl_status: None,
        }];
        assert_eq!(vault.name_found_files(&card, &mut items), 1);
        assert_eq!(items[0].path(), "DCIM/100CANON/IMG_0001.JPG");
        fs::remove_dir_all(&dir).unwrap();
    }
}

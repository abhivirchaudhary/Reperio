//! Known File Filter (KFF) using the NIST NSRL hash database.
//!
//! Quickly eliminates known-good system files from recovered results so
//! analysts focus on unknown or suspicious data.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::Connection;
use sha1::{Digest, Sha1};

/// A filter backed by an SQLite database of SHA-1 hashes.
pub struct NsrlFilter {
    db: Connection,
    count: u64,
}

impl NsrlFilter {
    /// Open an existing NSRL SQLite database.
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        db.execute(
            "CREATE TABLE IF NOT EXISTS hashes (sha1 TEXT PRIMARY KEY)",
            [],
        )
        .map_err(|e| e.to_string())?;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM hashes", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        Ok(NsrlFilter {
            db,
            count: count as u64,
        })
    }

    /// Create (or overwrite) a new empty filter at `path`.
    pub fn create(path: &Path) -> Result<Self, String> {
        let _ = std::fs::remove_file(path);
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        db.execute("CREATE TABLE hashes (sha1 TEXT PRIMARY KEY)", [])
            .map_err(|e| e.to_string())?;
        Ok(NsrlFilter { db, count: 0 })
    }

    /// Number of hashes in the database.
    pub fn len(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Check whether a SHA-1 hex string is known.
    pub fn is_known(&self, sha1_hex: &str) -> bool {
        let mut stmt = match self.db.prepare("SELECT 1 FROM hashes WHERE sha1 = ?1") {
            Ok(s) => s,
            Err(_) => return false,
        };
        stmt.exists([sha1_hex]).unwrap_or(false)
    }

    /// Check whether the SHA-1 of `data` is known.
    pub fn is_known_data(&self, data: &[u8]) -> bool {
        let hash = hex_sha1(data);
        self.is_known(&hash)
    }

    /// Insert a single SHA-1 hex string.
    pub fn insert(&mut self, sha1_hex: &str) -> Result<(), String> {
        self.db
            .execute(
                "INSERT OR IGNORE INTO hashes (sha1) VALUES (?1)",
                [sha1_hex],
            )
            .map_err(|e| e.to_string())?;
        self.count += 1;
        Ok(())
    }

    /// Import hashes from a text file: one SHA-1 hex string per line.
    /// Lines starting with '#' are ignored.
    pub fn import_text(&mut self, path: &Path) -> Result<u64, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let mut inserted: u64 = 0;
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            let hash = line.trim();
            if hash.is_empty() || hash.starts_with('#') {
                continue;
            }
            tx.execute("INSERT OR IGNORE INTO hashes (sha1) VALUES (?1)", [hash])
                .map_err(|e| e.to_string())?;
            if tx.changes() > 0 {
                inserted += 1;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.count += inserted;
        Ok(inserted)
    }

    /// Import from the NIST NSRL RDS minimal format:
    /// a pipe-delimited file where the first field is SHA-1.
    /// (e.g. `SHA1|MD5|CRC32|FileName|FileSize|ProductCode|OpSystemCode|SpecialCode`)
    pub fn import_nsrl_rds(&mut self, path: &Path) -> Result<u64, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let mut inserted: u64 = 0;
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.starts_with("SHA1") || line.is_empty() {
                continue;
            }
            let hash = line.split('|').next().unwrap_or("").trim();
            if hash.len() != 40 {
                continue;
            }
            tx.execute("INSERT OR IGNORE INTO hashes (sha1) VALUES (?1)", [hash])
                .map_err(|e| e.to_string())?;
            if tx.changes() > 0 {
                inserted += 1;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.count += inserted;
        Ok(inserted)
    }

    /// Build a lightweight "common OS" filter from a handful of known
    /// system-file hashes supplied inline. Useful for quick demos when the
    /// full NSRL is not available.
    pub fn build_lightweight(path: &Path) -> Result<Self, String> {
        let mut filter = Self::create(path)?;
        // A few well-known placeholder hashes (empty file, single-space, etc.)
        // In practice a real lightweight set would contain ~10k Windows/macOS/Linux system hashes.
        let known: &[&str] = &[
            "da39a3ee5e6b4b0d3255bfef95601890afd80709", // empty file
            "f4585c4e6118ad8b84d76543a7518e7d12323229", // notepad.exe placeholder
        ];
        for h in known {
            filter.insert(h)?;
        }
        Ok(filter)
    }
}

/// Compute the SHA-1 of `data` and return as a lowercase hex string.
pub fn hex_sha1(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

// tiny hex helper so we don't need an extra crate
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        const HEX: &[u8] = b"0123456789abcdef";
        let bytes = bytes.as_ref();
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_filter() -> NsrlFilter {
        let tmp = std::env::temp_dir().join(format!("reperio-nsrl-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        NsrlFilter::create(&tmp).unwrap()
    }

    #[test]
    fn insert_and_check() {
        let mut f = temp_filter();
        assert!(!f.is_known("abc123"));
        f.insert("abc123").unwrap();
        assert!(f.is_known("abc123"));
        assert!(!f.is_known("xyz789"));
    }

    #[test]
    fn is_known_data() {
        let mut f = temp_filter();
        let data = b"hello world";
        let hash = hex_sha1(data);
        assert!(!f.is_known(&hash));
        f.insert(&hash).unwrap();
        assert!(f.is_known_data(data));
    }

    #[test]
    fn import_text() {
        let mut f = temp_filter();
        let tmp = std::env::temp_dir().join(format!("nsrl-import-{}", std::process::id()));
        std::fs::write(&tmp, "# comment\nabc123\ndef456\n\n").unwrap();
        let n = f.import_text(&tmp).unwrap();
        assert_eq!(n, 2);
        assert!(f.is_known("abc123"));
        assert!(f.is_known("def456"));
        let _ = std::fs::remove_file(&tmp);
    }
}

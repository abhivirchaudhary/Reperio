//! Enterprise case management: track forensic cases, evidence items, scan
//! results, and chain of custody in a local SQLite database.
//!
//! When a scan is run with `--case` and `--evidence`, every result is
//! persisted so it can be reviewed, exported, and correlated later.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// A forensic case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub id: u64,
    pub name: String,
    pub description: Option<String>,
    pub investigator: String,
    pub status: CaseStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseStatus {
    Open,
    Closed,
    Archived,
}

impl std::fmt::Display for CaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaseStatus::Open => write!(f, "open"),
            CaseStatus::Closed => write!(f, "closed"),
            CaseStatus::Archived => write!(f, "archived"),
        }
    }
}

impl std::str::FromStr for CaseStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "open" => Ok(CaseStatus::Open),
            "closed" => Ok(CaseStatus::Closed),
            "archived" => Ok(CaseStatus::Archived),
            _ => Err(format!("unknown case status: {s}")),
        }
    }
}

/// An item of evidence attached to a case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub id: u64,
    pub case_id: u64,
    pub source_path: String,
    pub source_type: String,
    pub size_bytes: Option<u64>,
    pub acquisition_hash: Option<String>,
    pub seized_by: Option<String>,
    pub seized_at: Option<u64>,
    pub notes: Option<String>,
}

/// A chain-of-custody entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustodyEntry {
    pub id: u64,
    pub evidence_id: u64,
    pub from_analyst: Option<String>,
    pub to_analyst: String,
    pub action: String,
    pub timestamp: u64,
    pub notes: Option<String>,
}

/// A persisted scan result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRecord {
    pub id: u64,
    pub evidence_id: u64,
    pub scan_started_at: Option<u64>,
    pub scan_finished_at: Option<u64>,
    pub result_json: String,
    pub items_found: Option<u64>,
    pub items_recovered: Option<u64>,
    pub bad_sectors: Option<u64>,
}

/// Manages the case database.
pub struct CaseManager {
    db: Connection,
}

impl CaseManager {
    /// Open (or create) the case database in the user's data directory.
    pub fn open() -> Result<Self, String> {
        let path = db_path()?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(&path).map_err(|e| e.to_string())?;
        let mgr = CaseManager { db };
        mgr.init_schema()?;
        Ok(mgr)
    }

    /// Open at an explicit path (useful for tests).
    pub fn open_at(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        let mgr = CaseManager { db };
        mgr.init_schema()?;
        Ok(mgr)
    }

    fn init_schema(&self) -> Result<(), String> {
        self.db
            .execute_batch(
                r#"
CREATE TABLE IF NOT EXISTS cases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    description TEXT,
    investigator TEXT NOT NULL,
    status TEXT CHECK(status IN ('open','closed','archived')) DEFAULT 'open',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    case_id INTEGER NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
    source_path TEXT NOT NULL,
    source_type TEXT CHECK(source_type IN ('disk','image','memory','phone','raid')) NOT NULL,
    size_bytes INTEGER,
    acquisition_hash TEXT,
    seized_by TEXT,
    seized_at INTEGER,
    notes TEXT
);

CREATE TABLE IF NOT EXISTS scan_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    evidence_id INTEGER NOT NULL REFERENCES evidence(id) ON DELETE CASCADE,
    scan_started_at INTEGER,
    scan_finished_at INTEGER,
    result_json TEXT NOT NULL,
    items_found INTEGER,
    items_recovered INTEGER,
    bad_sectors INTEGER
);

CREATE TABLE IF NOT EXISTS custody_chain (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    evidence_id INTEGER NOT NULL REFERENCES evidence(id) ON DELETE CASCADE,
    from_analyst TEXT,
    to_analyst TEXT NOT NULL,
    action TEXT CHECK(action IN ('seized','imaged','analyzed','returned','transferred')) NOT NULL,
    timestamp INTEGER NOT NULL,
    notes TEXT
);
"#,
            )
            .map_err(|e| e.to_string())
    }

    // ------------------------------------------------------------------
    // Cases
    // ------------------------------------------------------------------

    pub fn create_case(
        &self,
        name: &str,
        investigator: &str,
        description: Option<&str>,
    ) -> Result<u64, String> {
        let now = now_secs();
        self.db
            .execute(
                "INSERT INTO cases (name, description, investigator, status, created_at, updated_at) VALUES (?1, ?2, ?3, 'open', ?4, ?4)",
                (name, description, investigator, now),
            )
            .map_err(|e| e.to_string())?;
        Ok(self.db.last_insert_rowid() as u64)
    }

    pub fn list_cases(&self) -> Result<Vec<Case>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, name, description, investigator, status, created_at, updated_at FROM cases ORDER BY created_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Case {
                    id: row.get::<_, i64>(0)? as u64,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    investigator: row.get(3)?,
                    status: status_from_str(&row.get::<_, String>(4)?),
                    created_at: row.get::<_, i64>(5)? as u64,
                    updated_at: row.get::<_, i64>(6)? as u64,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn get_case(&self, id: u64) -> Result<Option<Case>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, name, description, investigator, status, created_at, updated_at FROM cases WHERE id = ?1")
            .map_err(|e| e.to_string())?;
        let case_opt = stmt
            .query_row([id as i64], |row| {
                Ok(Case {
                    id: row.get::<_, i64>(0)? as u64,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    investigator: row.get(3)?,
                    status: status_from_str(&row.get::<_, String>(4)?),
                    created_at: row.get::<_, i64>(5)? as u64,
                    updated_at: row.get::<_, i64>(6)? as u64,
                })
            })
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(case_opt)
    }

    pub fn update_case_status(&self, id: u64, status: CaseStatus) -> Result<bool, String> {
        let now = now_secs();
        let changed = self
            .db
            .execute(
                "UPDATE cases SET status = ?1, updated_at = ?2 WHERE id = ?3",
                (status.to_string(), now, id as i64),
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    }

    pub fn delete_case(&self, id: u64) -> Result<bool, String> {
        let deleted = self
            .db
            .execute("DELETE FROM cases WHERE id = ?1", [id as i64])
            .map_err(|e| e.to_string())?;
        Ok(deleted > 0)
    }

    // ------------------------------------------------------------------
    // Evidence
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn add_evidence(
        &self,
        case_id: u64,
        source_path: &str,
        source_type: &str,
        size_bytes: Option<u64>,
        acquisition_hash: Option<&str>,
        seized_by: Option<&str>,
        notes: Option<&str>,
    ) -> Result<u64, String> {
        let now = now_secs();
        self.db
            .execute(
                "INSERT INTO evidence (case_id, source_path, source_type, size_bytes, acquisition_hash, seized_by, seized_at, notes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (case_id as i64, source_path, source_type, size_bytes.map(|v| v as i64), acquisition_hash, seized_by, now, notes),
            )
            .map_err(|e| e.to_string())?;
        let id = self.db.last_insert_rowid() as u64;
        // auto-record initial custody entry
        self.custody_transfer(id, None, seized_by.unwrap_or("unknown"), "seized", None)?;
        Ok(id)
    }

    pub fn list_evidence(&self, case_id: u64) -> Result<Vec<Evidence>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, case_id, source_path, source_type, size_bytes, acquisition_hash, seized_by, seized_at, notes FROM evidence WHERE case_id = ?1 ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([case_id as i64], |row| {
                Ok(Evidence {
                    id: row.get::<_, i64>(0)? as u64,
                    case_id: row.get::<_, i64>(1)? as u64,
                    source_path: row.get(2)?,
                    source_type: row.get(3)?,
                    size_bytes: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                    acquisition_hash: row.get(5)?,
                    seized_by: row.get(6)?,
                    seized_at: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                    notes: row.get(8)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn get_evidence(&self, id: u64) -> Result<Option<Evidence>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, case_id, source_path, source_type, size_bytes, acquisition_hash, seized_by, seized_at, notes FROM evidence WHERE id = ?1")
            .map_err(|e| e.to_string())?;
        let ev = stmt
            .query_row([id as i64], |row| {
                Ok(Evidence {
                    id: row.get::<_, i64>(0)? as u64,
                    case_id: row.get::<_, i64>(1)? as u64,
                    source_path: row.get(2)?,
                    source_type: row.get(3)?,
                    size_bytes: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                    acquisition_hash: row.get(5)?,
                    seized_by: row.get(6)?,
                    seized_at: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                    notes: row.get(8)?,
                })
            })
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(ev)
    }

    // ------------------------------------------------------------------
    // Custody chain
    // ------------------------------------------------------------------

    pub fn custody_transfer(
        &self,
        evidence_id: u64,
        from_analyst: Option<&str>,
        to_analyst: &str,
        action: &str,
        notes: Option<&str>,
    ) -> Result<u64, String> {
        let now = now_secs();
        self.db
            .execute(
                "INSERT INTO custody_chain (evidence_id, from_analyst, to_analyst, action, timestamp, notes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (evidence_id as i64, from_analyst, to_analyst, action, now, notes),
            )
            .map_err(|e| e.to_string())?;
        Ok(self.db.last_insert_rowid() as u64)
    }

    pub fn custody_chain(&self, evidence_id: u64) -> Result<Vec<CustodyEntry>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, evidence_id, from_analyst, to_analyst, action, timestamp, notes FROM custody_chain WHERE evidence_id = ?1 ORDER BY timestamp")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([evidence_id as i64], |row| {
                Ok(CustodyEntry {
                    id: row.get::<_, i64>(0)? as u64,
                    evidence_id: row.get::<_, i64>(1)? as u64,
                    from_analyst: row.get(2)?,
                    to_analyst: row.get(3)?,
                    action: row.get(4)?,
                    timestamp: row.get::<_, i64>(5)? as u64,
                    notes: row.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    // ------------------------------------------------------------------
    // Scan results
    // ------------------------------------------------------------------

    pub fn record_scan_start(&self, evidence_id: u64) -> Result<u64, String> {
        let now = now_secs();
        self.db
            .execute(
                "INSERT INTO scan_results (evidence_id, scan_started_at, result_json, items_found, items_recovered, bad_sectors) VALUES (?1, ?2, '{}', NULL, NULL, NULL)",
                (evidence_id as i64, now),
            )
            .map_err(|e| e.to_string())?;
        Ok(self.db.last_insert_rowid() as u64)
    }

    pub fn record_scan_finish(
        &self,
        scan_id: u64,
        result_json: &str,
        items_found: u64,
        bad_sectors: u64,
    ) -> Result<(), String> {
        let now = now_secs();
        self.db
            .execute(
                "UPDATE scan_results SET scan_finished_at = ?1, result_json = ?2, items_found = ?3, bad_sectors = ?4 WHERE id = ?5",
                (now, result_json, items_found as i64, bad_sectors as i64, scan_id as i64),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn record_items_recovered(&self, scan_id: u64, count: u64) -> Result<(), String> {
        self.db
            .execute(
                "UPDATE scan_results SET items_recovered = ?1 WHERE id = ?2",
                (count as i64, scan_id as i64),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_scan_results(&self, evidence_id: u64) -> Result<Vec<ScanRecord>, String> {
        let mut stmt = self
            .db
            .prepare("SELECT id, evidence_id, scan_started_at, scan_finished_at, result_json, items_found, items_recovered, bad_sectors FROM scan_results WHERE evidence_id = ?1 ORDER BY scan_started_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([evidence_id as i64], |row| {
                Ok(ScanRecord {
                    id: row.get::<_, i64>(0)? as u64,
                    evidence_id: row.get::<_, i64>(1)? as u64,
                    scan_started_at: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    scan_finished_at: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    result_json: row.get(4)?,
                    items_found: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    items_recovered: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    bad_sectors: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

fn db_path() -> Result<PathBuf, String> {
    let home = home_dir().ok_or("no home directory")?;
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Reperio")
    } else if cfg!(windows) {
        home.join("AppData/Roaming/Reperio")
    } else {
        home.join(".local/share/reperio")
    };
    Ok(dir.join("cases.db"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn status_from_str(s: &str) -> CaseStatus {
    match s {
        "closed" => CaseStatus::Closed,
        "archived" => CaseStatus::Archived,
        _ => CaseStatus::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> CaseManager {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("reperio-case-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_file(&tmp);
        CaseManager::open_at(&tmp).unwrap()
    }

    #[test]
    fn create_and_list_cases() {
        let mgr = temp_db();
        let id = mgr
            .create_case("Test Case", "Alice", Some("A test"))
            .unwrap();
        assert!(id > 0);
        let cases = mgr.list_cases().unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "Test Case");
        assert_eq!(cases[0].investigator, "Alice");
        assert_eq!(cases[0].status, CaseStatus::Open);
    }

    #[test]
    fn update_status_and_delete() {
        let mgr = temp_db();
        let id = mgr.create_case("To Close", "Bob", None).unwrap();
        assert!(mgr.update_case_status(id, CaseStatus::Closed).unwrap());
        let c = mgr.get_case(id).unwrap().unwrap();
        assert_eq!(c.status, CaseStatus::Closed);
        assert!(mgr.delete_case(id).unwrap());
        assert!(mgr.get_case(id).unwrap().is_none());
    }

    #[test]
    fn evidence_and_custody() {
        let mgr = temp_db();
        let case_id = mgr.create_case("Evidence Test", "Carol", None).unwrap();
        let ev_id = mgr
            .add_evidence(
                case_id,
                "/dev/sdb",
                "disk",
                Some(64_000_000_000),
                None,
                Some("Carol"),
                None,
            )
            .unwrap();
        let ev = mgr.get_evidence(ev_id).unwrap().unwrap();
        assert_eq!(ev.source_path, "/dev/sdb");
        assert_eq!(ev.source_type, "disk");

        let chain = mgr.custody_chain(ev_id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].action, "seized");
        assert_eq!(chain[0].to_analyst, "Carol");

        mgr.custody_transfer(
            ev_id,
            Some("Carol"),
            "Dave",
            "analyzed",
            Some("started analysis"),
        )
        .unwrap();
        let chain = mgr.custody_chain(ev_id).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1].action, "analyzed");
    }

    #[test]
    fn scan_results() {
        let mgr = temp_db();
        let case_id = mgr.create_case("Scan Test", "Eve", None).unwrap();
        let ev_id = mgr
            .add_evidence(case_id, "image.img", "image", None, None, Some("Eve"), None)
            .unwrap();
        let scan_id = mgr.record_scan_start(ev_id).unwrap();
        mgr.record_scan_finish(scan_id, "{}", 42, 3).unwrap();
        mgr.record_items_recovered(scan_id, 38).unwrap();
        let results = mgr.list_scan_results(ev_id).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].items_found, Some(42));
        assert_eq!(results[0].items_recovered, Some(38));
        assert_eq!(results[0].bad_sectors, Some(3));
    }
}

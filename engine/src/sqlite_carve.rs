//! Recovering deleted rows from SQLite databases.
//!
//! Almost everything an iPhone app remembers lives in a SQLite database:
//! messages, WhatsApp chats, notes, contacts, call history, Safari history.
//! When a row is deleted SQLite does not erase it. It marks the space free,
//! and the row's bytes stay until something is written over them: in the
//! unused part of the page, in a *freeblock*, on a free page, or in an older
//! copy of the page still in the write-ahead log (`-wal`).
//!
//! This finds those rows and decodes them into columns. Nothing here depends
//! on what the database is for: a candidate record is accepted only if its
//! header is well formed, it fits the page, and its columns agree with the
//! types of some table in the database's own schema. Rows that are still live
//! are left out. What comes back is a best effort: a row can be partly
//! overwritten, and a record can match the wrong table of the same shape.

use std::collections::HashSet;

/// A value in a record.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", content = "value")]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

// Real numbers are compared by their bits, so rows can be collected in sets.
impl Eq for Cell {}

impl std::hash::Hash for Cell {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        std::mem::discriminant(self).hash(h);
        match self {
            Cell::Null => {}
            Cell::Int(v) => v.hash(h),
            Cell::Real(v) => v.to_bits().hash(h),
            Cell::Text(v) => v.hash(h),
            Cell::Blob(v) => v.hash(h),
        }
    }
}

/// A table of the database, as its schema says.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    /// The declared type, upper case (`INTEGER`, `VARCHAR`, empty).
    pub decl: String,
    /// The column is the rowid (`INTEGER PRIMARY KEY`): records store NULL.
    pub rowid_alias: bool,
}

/// A deleted row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Carved {
    pub table: String,
    /// Where it was found: `free page`, `page` (space inside a live page) or `wal`.
    pub from: &'static str,
    pub page: u32,
    pub cells: Vec<Cell>,
}

/// A row, with the rowid column left out, for telling live rows from deleted.
pub fn key(cells: &[Cell], tbl: &Table) -> Vec<Cell> {
    cells
        .iter()
        .zip(&tbl.columns)
        .filter(|(_, c)| !c.rowid_alias)
        .map(|(v, _)| v.clone())
        .collect()
}

/// Reads an SQLite varint: its value and length.
fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let byte = *b.get(i)?;
        v = v << 7 | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    let last = *b.get(8)?;
    Some((v << 8 | u64::from(last), 9))
}

fn body_len(serial: u64) -> Option<usize> {
    Some(match serial {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => return None,
        n if n % 2 == 0 => ((n - 12) / 2) as usize,
        n => ((n - 13) / 2) as usize,
    })
}

fn cell(serial: u64, b: &[u8]) -> Option<Cell> {
    let signed = |n: usize| -> i64 {
        let mut v = if b[0] & 0x80 != 0 { -1i64 } else { 0 };
        for &x in &b[..n] {
            v = v << 8 | i64::from(x);
        }
        v
    };
    Some(match serial {
        0 => Cell::Null,
        1 => Cell::Int(signed(1)),
        2 => Cell::Int(signed(2)),
        3 => Cell::Int(signed(3)),
        4 => Cell::Int(signed(4)),
        5 => Cell::Int(signed(6)),
        6 => Cell::Int(signed(8)),
        7 => Cell::Real(f64::from_be_bytes(b[..8].try_into().ok()?)),
        8 => Cell::Int(0),
        9 => Cell::Int(1),
        n if n % 2 == 0 => Cell::Blob(b.to_vec()),
        _ => Cell::Text(String::from_utf8(b.to_vec()).ok()?),
    })
}

/// Whether a value could sit in a column declared with this type.
fn fits(decl: &str, v: &Cell) -> bool {
    if matches!(v, Cell::Null) {
        return true;
    }
    if decl.contains("INT") {
        return matches!(v, Cell::Int(_));
    }
    if decl.contains("CHAR") || decl.contains("CLOB") || decl.contains("TEXT") {
        return matches!(v, Cell::Text(_));
    }
    if decl.contains("REAL") || decl.contains("FLOA") || decl.contains("DOUB") {
        return matches!(v, Cell::Real(_) | Cell::Int(_));
    }
    if decl.contains("BLOB") {
        return matches!(v, Cell::Blob(_));
    }
    true // NUMERIC, TIMESTAMP, no type: anything
}

/// A record starting at `p` in `page`, if there is a well formed one.
fn record_at(page: &[u8], p: usize) -> Option<(Vec<Cell>, usize)> {
    let rest = page.get(p..)?;
    let (header_len, n) = varint(rest)?;
    let header_len = header_len as usize;
    if !(2..=1000).contains(&header_len) || header_len > rest.len() {
        return None;
    }
    let mut at = n;
    let mut serials = Vec::new();
    while at < header_len {
        let (s, k) = varint(&rest[at..header_len])?;
        serials.push(s);
        at += k;
    }
    if at != header_len || serials.len() < 2 {
        return None;
    }
    let mut pos = header_len;
    let mut cells = Vec::with_capacity(serials.len());
    for s in serials {
        let len = body_len(s)?;
        let bytes = rest.get(pos..pos + len)?;
        cells.push(cell(s, bytes)?);
        pos += len;
    }
    Some((cells, pos))
}

/// A deleted row whose start was overwritten. SQLite turns a deleted cell
/// into a *freeblock* by writing its next pointer and size over the cell's
/// first four bytes, which hold the payload length, rowid and the start of
/// the record header. What is left is the rest of the column types and the
/// data. For a table whose first column is the rowid (stored as NULL, one
/// byte) the lost header bytes can be rebuilt from the schema.
fn damaged_at(page: &[u8], p: usize, tables: &[Table]) -> Option<(usize, Vec<Cell>, usize)> {
    // The four bytes at `p` are the freeblock header (or what an earlier one
    // left behind); what follows must be the record header's other types.
    for (ti, t) in tables.iter().enumerate() {
        let n = t.columns.len();
        if n < 4 || !t.columns[0].rowid_alias {
            continue;
        }
        // The first type byte survives (and says NULL) when the rowid or the
        // payload length took two bytes; otherwise it is gone.
        for (start, surviving) in [(p + 4, false), (p + 5, true)] {
            if surviving && page.get(p + 4) != Some(&0) {
                continue;
            }
            let mut at = start;
            let mut serials = Vec::with_capacity(n - 1);
            for _ in 1..n {
                let Some((v, k)) = page.get(at..).and_then(varint) else {
                    break;
                };
                serials.push(v);
                at += k;
            }
            if serials.len() != n - 1 {
                continue;
            }
            let mut cells = vec![Cell::Null];
            let mut pos = at;
            let mut ok = true;
            for sr in serials {
                let Some(len) = body_len(sr) else {
                    ok = false;
                    break;
                };
                let Some(bytes) = page.get(pos..pos + len) else {
                    ok = false;
                    break;
                };
                let Some(c) = cell(sr, bytes) else {
                    ok = false;
                    break;
                };
                cells.push(c);
                pos += len;
            }
            // Strict about what a row must look like, since there is no
            // header length or freeblock size to check it against.
            let wordy = cells.iter().any(|c| matches!(c, Cell::Text(s) if s.chars().count() >= 4 && s.chars().all(|ch| !ch.is_control() || ch == '\n')));
            if ok
                && wordy
                && cells.iter().zip(&t.columns).all(|(v, c)| {
                    if c.rowid_alias {
                        matches!(v, Cell::Null)
                    } else {
                        fits(&c.decl, v)
                    }
                })
            {
                return Some((ti, cells, pos - p));
            }
        }
    }
    None
}

/// The table a record belongs to, if exactly its columns' types agree.
fn table_for<'a>(cells: &[Cell], tables: &'a [Table]) -> Option<&'a Table> {
    tables.iter().find(|t| {
        t.columns.len() == cells.len()
            && t.columns.iter().zip(cells).all(|(c, v)| {
                if c.rowid_alias {
                    matches!(v, Cell::Null)
                } else {
                    fits(&c.decl, v)
                }
            })
    })
}

/// Records in one page's bytes that match a table. `skip` is the page's
/// leading header (the database header and the b-tree page header).
fn scan_page(
    page: &[u8],
    number: u32,
    from: &'static str,
    tables: &[Table],
    seen: &mut HashSet<Vec<Cell>>,
    live: &HashSet<(String, Vec<Cell>)>,
    out: &mut Vec<Carved>,
) {
    let mut covered = vec![false; page.len()];
    let first_new = out.len();
    let mut keep = |ti: usize, cells: Vec<Cell>, out: &mut Vec<Carved>| {
        let t = &tables[ti];
        let k = key(&cells, t);
        if !live.contains(&(t.name.clone(), k.clone())) && seen.insert(k) && has_content(&cells) {
            out.push(Carved {
                table: t.name.clone(),
                from,
                page: number,
                cells,
            });
        }
    };
    // First the records whose headers are whole. Every position is tried and
    // the matches are taken widest first, so a record with many columns is
    // never cut up by a stray match with few.
    let mut found: Vec<(usize, usize, usize, Vec<Cell>)> = Vec::new();
    for p in 0..page.len().saturating_sub(4) {
        if let Some((cells, used)) = record_at(page, p) {
            if let Some(ti) = table_index(&cells, tables) {
                found.push((p, used, ti, cells));
            }
        }
    }
    found.sort_by(|a, b| {
        b.3.len()
            .cmp(&a.3.len())
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });
    for (p, used, ti, cells) in found {
        if !covered[p..p + used].iter().any(|&c| c) {
            covered[p..p + used].fill(true);
            keep(ti, cells, out);
        }
    }
    // Then rows whose first bytes were overwritten when they were deleted:
    // SQLite writes a freeblock header over the start of every freed cell.
    let mut lost: Vec<(usize, usize, usize, Vec<Cell>)> = Vec::new();
    for (p, &is_covered) in covered
        .iter()
        .enumerate()
        .take(page.len().saturating_sub(8))
    {
        if is_covered {
            continue;
        }
        if let Some((ti, cells, used)) = damaged_at(page, p, tables) {
            lost.push((p, used, ti, cells));
        }
    }
    lost.sort_by(|a, b| {
        b.3.len()
            .cmp(&a.3.len())
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });
    for (p, used, ti, cells) in lost {
        if !covered[p..p + used].iter().any(|&c| c) {
            covered[p..p + used].fill(true);
            keep(ti, cells, out);
        }
    }
    // A row made of the tail of a wider row's bytes is a mistake, not a second
    // row: drop any that shares a piece of text with a row of more columns.
    let words = |c: &Carved| -> Vec<String> {
        c.cells
            .iter()
            .filter_map(|x| {
                if let Cell::Text(t) = x {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .filter(|t| t.chars().count() >= 8)
            .collect()
    };
    let page_rows: Vec<(usize, Vec<String>)> = out[first_new..]
        .iter()
        .map(|c| (c.cells.len(), words(c)))
        .collect();
    let mut i = 0;
    out.retain(|_| {
        let idx = i;
        i += 1;
        if idx < first_new {
            return true;
        }
        let (cols, mine) = &page_rows[idx - first_new];
        !mine.is_empty()
            && !page_rows
                .iter()
                .any(|(c2, w2)| c2 > cols && w2.iter().any(|t| mine.contains(t)))
    });
}

fn table_index(cells: &[Cell], tables: &[Table]) -> Option<usize> {
    table_for(cells, tables).and_then(|t| tables.iter().position(|x| std::ptr::eq(x, t)))
}

/// A row with next to nothing in it (all NULLs and zeros, one stray number)
/// is noise: a real row has some words, or at least two values.
fn has_content(cells: &[Cell]) -> bool {
    let words = cells.iter().any(|c| match c {
        Cell::Text(s) => s.chars().count() >= 3,
        Cell::Blob(b) => b.len() >= 3,
        _ => false,
    });
    let values = cells
        .iter()
        .filter(|c| match c {
            Cell::Text(s) => !s.is_empty(),
            Cell::Blob(b) => !b.is_empty(),
            Cell::Int(n) => *n != 0,
            Cell::Real(r) => *r != 0.0,
            Cell::Null => false,
        })
        .count();
    words || values >= 2
}

/// The page numbers on a database's freelist.
fn freelist(db: &[u8], page_size: usize) -> HashSet<usize> {
    let be32 = |at: usize| {
        db.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
    };
    let mut free = HashSet::new();
    let mut trunk = be32(32).unwrap_or(0);
    let mut guard = 0;
    while trunk > 0 && guard < 100_000 {
        guard += 1;
        let base = (trunk - 1) * page_size;
        let Some(next) = be32(base) else { break };
        free.insert(trunk);
        let count = be32(base + 4).unwrap_or(0);
        for i in 0..count.min(page_size / 4 - 2) {
            if let Some(leaf) = be32(base + 8 + i * 4) {
                free.insert(leaf);
            }
        }
        trunk = next;
    }
    free
}

fn page_size_of(db: &[u8]) -> Option<usize> {
    match db.get(16..18).map(|b| u16::from_be_bytes([b[0], b[1]]))? {
        1 => Some(65_536),
        n if n.is_power_of_two() && n >= 512 => Some(n as usize),
        _ => None,
    }
}

/// Deleted rows of the database `db`, and of its write-ahead log `wal` if it
/// has one. `tables` is the schema; `live` the rows that are not deleted, as
/// `(table, key)`.
pub fn carve(
    db: &[u8],
    wal: Option<&[u8]>,
    tables: &[Table],
    live: &HashSet<(String, Vec<Cell>)>,
) -> Vec<Carved> {
    let Some(ps) = page_size_of(db) else {
        return Vec::new();
    };
    let free = freelist(db, ps);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (i, page) in db.chunks(ps).enumerate() {
        if page.len() < ps {
            break;
        }
        let number = i as u32 + 1;
        let head = if i == 0 { 100 } else { 0 };
        let is_free = free.contains(&(i + 1));
        // Leaf table pages hold rows; free pages hold whatever they held before.
        if is_free || page[head] == 0x0d {
            scan_page(
                &page[head..],
                number,
                if is_free { "free page" } else { "page" },
                tables,
                &mut seen,
                live,
                &mut out,
            );
        }
    }
    if let Some(wal) = wal {
        if wal.len() >= 32
            && matches!(
                wal[..4],
                [0x37, 0x7f, 0x06, 0x82] | [0x37, 0x7f, 0x06, 0x83]
            )
        {
            let wps = u32::from_be_bytes(wal[8..12].try_into().unwrap()) as usize;
            if wps == ps {
                for frame in wal[32..].chunks(24 + ps) {
                    if frame.len() < 24 + ps {
                        break;
                    }
                    let no = u32::from_be_bytes(frame[..4].try_into().unwrap());
                    let page = &frame[24..];
                    let head = if no == 1 { 100 } else { 0 };
                    if page[head] == 0x0d {
                        scan_page(&page[head..], no, "wal", tables, &mut seen, live, &mut out);
                    }
                }
            }
        }
    }
    out
}

/// The tables of an open database, from its own schema.
pub fn schema(db: &rusqlite::Connection) -> Vec<Table> {
    let names: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .and_then(|mut s| s.query_map([], |r| r.get(0)).map(|r| r.flatten().collect()))
        .unwrap_or_default();
    names
        .into_iter()
        .map(|name| {
            let columns = db
                .prepare(&format!(
                    "PRAGMA table_info(\"{}\")",
                    name.replace('"', "\"\"")
                ))
                .and_then(|mut s| {
                    s.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(5)?,
                        ))
                    })
                    .map(|rows| rows.flatten().collect::<Vec<_>>())
                })
                .unwrap_or_default()
                .into_iter()
                .map(|(n, t, pk)| Column {
                    name: n,
                    rowid_alias: pk == 1 && t.eq_ignore_ascii_case("INTEGER"),
                    decl: t.to_ascii_uppercase(),
                })
                .collect();
            Table { name, columns }
        })
        .collect()
}

/// Every live row of every table, for telling deleted rows from live ones.
pub fn live_rows(db: &rusqlite::Connection, tables: &[Table]) -> HashSet<(String, Vec<Cell>)> {
    use rusqlite::types::ValueRef;
    let mut out = HashSet::new();
    for t in tables {
        let Ok(mut stmt) = db.prepare(&format!(
            "SELECT * FROM \"{}\"",
            t.name.replace('"', "\"\"")
        )) else {
            continue;
        };
        let n = t.columns.len();
        let Ok(rows) = stmt.query_map([], |r| {
            (0..n)
                .map(|i| {
                    Ok(match r.get_ref(i)? {
                        ValueRef::Null => Cell::Null,
                        ValueRef::Integer(v) => Cell::Int(v),
                        ValueRef::Real(v) => Cell::Real(v),
                        ValueRef::Text(v) => Cell::Text(String::from_utf8_lossy(v).into()),
                        ValueRef::Blob(v) => Cell::Blob(v.to_vec()),
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        }) else {
            continue;
        };
        for row in rows.flatten() {
            out.insert((t.name.clone(), key(&row, t)));
        }
    }
    out
}

/// Deleted rows of the database file `db`, and of its log `db-wal` if there
/// is one, working on copies in `scratch` so the originals are never opened
/// (opening a database in WAL mode folds the log into it, and with it the
/// old pages this is looking for).
pub fn carve_file(
    db: &std::path::Path,
    scratch: &std::path::Path,
) -> Result<(Vec<Carved>, Vec<Table>), String> {
    let name = db.file_name().and_then(|n| n.to_str()).ok_or("bad name")?;
    let copy = scratch.join(name);
    std::fs::copy(db, &copy).map_err(|e| e.to_string())?;
    let wal_from = db.with_file_name(format!("{name}-wal"));
    let wal_copy = scratch.join(format!("{name}-wal"));
    if wal_from.is_file() {
        let _ = std::fs::copy(&wal_from, &wal_copy);
    }
    // The raw bytes first: opening the copy may checkpoint its log away.
    let bytes = std::fs::read(&copy).map_err(|e| e.to_string())?;
    let wal = std::fs::read(&wal_copy).ok();
    let conn =
        rusqlite::Connection::open_with_flags(&copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| e.to_string())?;
    let tables = schema(&conn);
    let live = live_rows(&conn, &tables);
    drop(conn);
    Ok((carve(&bytes, wal.as_deref(), &tables, &live), tables))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn varints_and_serial_types() {
        assert_eq!(varint(&[0x05]), Some((5, 1)));
        assert_eq!(varint(&[0x81, 0x00]), Some((128, 2)));
        assert_eq!(body_len(13 + 2 * 11), Some(11));
        assert_eq!(body_len(12 + 2 * 3), Some(3));
        assert_eq!(body_len(10), None);
    }

    /// Delete half the rows of a table, then get them back, whole.
    #[test]
    fn deleted_rows_come_back_with_their_columns() {
        let dir = std::env::temp_dir().join(format!("reperio-carve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA secure_delete=OFF; PRAGMA journal_mode=DELETE;
             CREATE TABLE message (ROWID INTEGER PRIMARY KEY, text TEXT, date INTEGER, is_from_me INTEGER, handle VARCHAR);
             CREATE TABLE other (a INTEGER PRIMARY KEY, b BLOB);",
        )
        .unwrap();
        for i in 0..300 {
            db.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    i,
                    format!("message number {i} about the plan for tonight"),
                    700_000_000 + i * 60,
                    i % 2,
                    "+15551230001"
                ],
            )
            .unwrap();
        }
        db.execute("INSERT INTO other VALUES (1, x'0102')", [])
            .unwrap();
        db.execute("DELETE FROM message WHERE ROWID % 3 = 0", [])
            .unwrap();
        let tables = schema(&db);
        let live = live_rows(&db, &tables);
        drop(db);
        let bytes = std::fs::read(&path).unwrap();
        let found = carve(&bytes, None, &tables, &live);
        let deleted: Vec<&Carved> = found.iter().filter(|c| c.table == "message").collect();
        // A deleted row is whole again: text, date, flag and sender.
        let row = deleted.iter().find(|c| {
            c.cells.contains(&Cell::Text(
                "message number 99 about the plan for tonight".into(),
            ))
        });
        let row = row.expect("row 99 was deleted and is recovered");
        assert_eq!(
            row.cells[0],
            Cell::Null,
            "the rowid is not stored in the record"
        );
        assert_eq!(row.cells[2], Cell::Int(700_000_000 + 99 * 60));
        assert_eq!(row.cells[3], Cell::Int(1));
        assert_eq!(row.cells[4], Cell::Text("+15551230001".into()));
        // Live rows are never reported as deleted.
        assert!(!deleted.iter().any(|c| c.cells.contains(&Cell::Text(
            "message number 100 about the plan for tonight".into()
        ))));
        assert!(
            deleted.len() >= 60,
            "most of the 100 deleted rows came back, got {}",
            deleted.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rows deleted but not yet checkpointed: the old pages are still in the
    /// database file, and the new ones in the write-ahead log.
    #[test]
    fn rows_deleted_in_wal_mode_are_found_in_the_log_and_the_old_pages() {
        let dir = std::env::temp_dir().join(format!("reperio-carve-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA secure_delete=OFF; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE note (id INTEGER PRIMARY KEY, title TEXT, body TEXT, modified REAL);",
        )
        .unwrap();
        for i in 0..200 {
            db.execute(
                "INSERT INTO note VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    i,
                    format!("Title {i}"),
                    format!("Remember to buy item {i} before the trip on Friday"),
                    7.0e8 + f64::from(i)
                ],
            )
            .unwrap();
        }
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        db.execute("DELETE FROM note WHERE id % 2 = 0", []).unwrap();
        let tables = schema(&db);
        let live = live_rows(&db, &tables);
        let main = std::fs::read(&path).unwrap();
        let wal = std::fs::read(dir.join("w.db-wal")).unwrap();
        let found = carve(&main, Some(&wal), &tables, &live);
        let gone = found.iter().filter(|c| c.table == "note").count();
        assert!(
            gone >= 90,
            "most of the 100 deleted notes came back, got {gone}"
        );
        assert!(found.iter().any(|c| c.cells.contains(&Cell::Text(
            "Remember to buy item 42 before the trip on Friday".into()
        ))));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! WhatsApp and Notes inside an iPhone backup: what is there, and what was
//! deleted and can still be got back.
//!
//! Both apps keep everything in SQLite databases (`ChatStorage.sqlite`,
//! `NoteStore.sqlite`). Reading them gives what the app still shows, with the
//! apps' own Recently Deleted where it has one. Deleted for good, the rows are
//! recovered from the databases' leftovers by [`crate::sqlite_carve`]: whole
//! rows, marked *recovered*, which may be incomplete.

use std::io::Read;

use rusqlite::Connection;
use serde::Serialize;

use crate::ios::{apple_date, columns, table_exists, Backup};
use crate::sqlite_carve::Cell;

/// Where WhatsApp keeps its data in a backup (the newer group container first).
const WA_DOMAINS: [&str; 2] = [
    "AppDomainGroup-group.net.whatsapp.WhatsApp.shared",
    "AppDomain-net.whatsapp.WhatsApp",
];
const WA_DB: &str = "ChatStorage.sqlite";
const NOTES_DOMAIN: &str = "AppDomainGroup-group.com.apple.notes";
const NOTES_DB: &str = "NoteStore.sqlite";

/// A WhatsApp conversation.
#[derive(Debug, Clone, Serialize)]
pub struct WaChat {
    pub id: i64,
    pub name: String,
    pub jid: String,
    pub group: bool,
    pub last: Option<String>,
    pub messages: usize,
}

/// A WhatsApp message.
#[derive(Debug, Clone, Serialize)]
pub struct WaMessage {
    pub id: i64,
    pub chat: i64,
    pub chat_name: String,
    pub from_me: bool,
    pub who: String,
    pub text: String,
    pub date: Option<String>,
    /// `text`, `photo`, `video`, `audio`, `document`, `contact`, `location`,
    /// `link`, `sticker` or `other`.
    pub kind: String,
    /// The media file in the backup, if the backup holds it.
    pub media: Option<String>,
    pub media_name: Option<String>,
    /// Deleted for good and recovered from the database's leftovers.
    pub recovered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhatsApp {
    pub chats: Vec<WaChat>,
    pub messages: Vec<WaMessage>,
}

/// A note.
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    pub title: String,
    pub folder: String,
    pub modified: Option<String>,
    pub created: Option<String>,
    /// In the Notes app's Recently Deleted.
    pub deleted: bool,
    /// Deleted for good and recovered from the database's leftovers.
    pub recovered: bool,
    pub text: String,
}

fn jid_name(jid: &str) -> String {
    jid.split('@').next().unwrap_or(jid).to_string()
}

fn kind_of(t: i64) -> &'static str {
    match t {
        0 => "text",
        1 => "photo",
        2 => "video",
        3 => "audio",
        4 => "contact",
        5 => "location",
        7 => "link",
        8 => "document",
        15 => "sticker",
        _ => "other",
    }
}

fn date(v: Option<f64>) -> Option<String> {
    v.filter(|d| *d > 0.0).map(apple_date)
}

impl Backup {
    fn whatsapp_db(&self) -> Option<(Connection, &'static str)> {
        WA_DOMAINS
            .iter()
            .find_map(|d| self.database(d, WA_DB).map(|c| (c, *d)))
    }

    /// Every conversation and message WhatsApp still has, with their photos,
    /// videos and other media where the backup holds the files.
    pub fn whatsapp(&self) -> Result<WhatsApp, String> {
        let (db, domain) = self
            .whatsapp_db()
            .ok_or("This backup has no WhatsApp data (ChatStorage.sqlite).")?;
        if !table_exists(&db, "ZWAMESSAGE") {
            return Err("WhatsApp's database has no messages table.".into());
        }
        let mut chats: Vec<WaChat> = Vec::new();
        if let Ok(mut st) = db
            .prepare("SELECT Z_PK, ZCONTACTJID, ZPARTNERNAME, ZLASTMESSAGEDATE FROM ZWACHATSESSION")
        {
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                ))
            });
            for (id, jid, name, last) in rows.map_err(|e| e.to_string())?.flatten() {
                let jid = jid.unwrap_or_default();
                chats.push(WaChat {
                    id,
                    name: name
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| jid_name(&jid)),
                    group: jid.ends_with("@g.us"),
                    jid,
                    last: date(last),
                    messages: 0,
                });
            }
        }
        let names: std::collections::HashMap<i64, String> =
            chats.iter().map(|c| (c.id, c.name.clone())).collect();
        let have = columns(&db, "ZWAMESSAGE");
        let col = |c: &str| {
            if have.iter().any(|h| h == c) {
                format!("m.{c}")
            } else {
                "NULL".into()
            }
        };
        let sql = format!(
            "SELECT m.Z_PK, {chat}, {mine}, {when}, {text}, {from}, {kind}, mi.ZMEDIALOCALPATH, mi.ZTITLE
             FROM ZWAMESSAGE m LEFT JOIN ZWAMEDIAITEM mi ON mi.Z_PK = m.ZMEDIAITEM ORDER BY m.ZMESSAGEDATE",
            chat = col("ZCHATSESSION"),
            mine = col("ZISFROMME"),
            when = col("ZMESSAGEDATE"),
            text = col("ZTEXT"),
            from = col("ZFROMJID"),
            kind = col("ZMESSAGETYPE"),
        );
        let mut st = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut messages = Vec::new();
        for (id, chat, mine, when, text, from, kind, media_path, media_title) in rows.flatten() {
            let chat = chat.unwrap_or(0);
            // The media file: WhatsApp stores the path under Message/ in its container.
            let media = media_path
                .as_deref()
                .filter(|p| !p.is_empty())
                .and_then(|p| {
                    let p = p.trim_start_matches('/');
                    self.lookup(domain, &format!("Message/{p}"))
                        .or_else(|| self.lookup(domain, p))
                        .filter(|f| self.stored(f).is_some())
                });
            let text = text.unwrap_or_default();
            if text.is_empty() && media.is_none() && media_title.as_deref().unwrap_or("").is_empty()
            {
                continue; // system notices and the like
            }
            let from_me = mine == Some(1);
            let chat_name = names.get(&chat).cloned().unwrap_or_default();
            messages.push(WaMessage {
                id,
                chat,
                who: if from_me {
                    "Me".into()
                } else {
                    from.as_deref()
                        .map(jid_name)
                        .filter(|w| !w.is_empty())
                        .unwrap_or_else(|| chat_name.clone())
                },
                chat_name,
                from_me,
                text,
                date: date(when),
                kind: kind_of(kind.unwrap_or(0)).into(),
                media,
                media_name: media_title.filter(|t| !t.is_empty()),
                recovered: false,
            });
        }
        for c in &mut chats {
            c.messages = messages.iter().filter(|m| m.chat == c.id).count();
        }
        chats.retain(|c| c.messages > 0);
        chats.sort_by(|a, b| b.last.cmp(&a.last));
        Ok(WhatsApp { chats, messages })
    }

    /// WhatsApp messages deleted for good, rebuilt from the database's
    /// leftovers: text, sender, chat and date where they survive.
    pub fn whatsapp_deleted(&self) -> Result<Vec<WaMessage>, String> {
        let (_, domain) = self
            .whatsapp_db()
            .ok_or("This backup has no WhatsApp data (ChatStorage.sqlite).")?;
        let names: std::collections::HashMap<i64, String> = self
            .whatsapp()
            .map(|w| w.chats.into_iter().map(|c| (c.id, c.name)).collect())
            .unwrap_or_default();
        let (rows, tables) = self.deleted_rows(domain, WA_DB)?;
        let Some(t) = tables.iter().find(|t| t.name == "ZWAMESSAGE") else {
            return Ok(Vec::new());
        };
        let at = |n: &str| t.columns.iter().position(|c| c.name == n);
        let (text, when, mine, from, chat, kind) = (
            at("ZTEXT"),
            at("ZMESSAGEDATE"),
            at("ZISFROMME"),
            at("ZFROMJID"),
            at("ZCHATSESSION"),
            at("ZMESSAGETYPE"),
        );
        let int = |c: &Cell| if let Cell::Int(n) = c { Some(*n) } else { None };
        let mut out = Vec::new();
        for (n, r) in rows.iter().filter(|r| r.table == "ZWAMESSAGE").enumerate() {
            let Some(Cell::Text(text)) = text
                .map(|i| &r.cells[i])
                .filter(|c| matches!(c, Cell::Text(t) if !t.is_empty()))
            else {
                continue;
            };
            let chat_id = chat.and_then(|i| int(&r.cells[i])).unwrap_or(0);
            let from_me = mine.and_then(|i| int(&r.cells[i])) == Some(1);
            let when = when.map(|i| &r.cells[i]).and_then(|c| match c {
                Cell::Real(v) => Some(*v),
                Cell::Int(v) => Some(*v as f64),
                _ => None,
            });
            let chat_name = names.get(&chat_id).cloned().unwrap_or_default();
            out.push(WaMessage {
                id: -(n as i64) - 1,
                chat: chat_id,
                who: if from_me {
                    "Me".into()
                } else {
                    from.map(|i| &r.cells[i])
                        .and_then(|c| {
                            if let Cell::Text(t) = c {
                                Some(jid_name(t))
                            } else {
                                None
                            }
                        })
                        .filter(|w| !w.is_empty())
                        .unwrap_or_else(|| chat_name.clone())
                },
                chat_name,
                from_me,
                text: text.clone(),
                date: date(when),
                kind: kind_of(kind.and_then(|i| int(&r.cells[i])).unwrap_or(0)).into(),
                media: None,
                media_name: None,
                recovered: true,
            });
        }
        out.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(out)
    }

    /// Notes the Notes app still has, including those in its Recently Deleted.
    pub fn notes(&self) -> Result<Vec<Note>, String> {
        let db = self
            .database(NOTES_DOMAIN, NOTES_DB)
            .ok_or("This backup has no Notes data (NoteStore.sqlite).")?;
        if !table_exists(&db, "ZICCLOUDSYNCINGOBJECT") || !table_exists(&db, "ZICNOTEDATA") {
            return Err("The Notes database has no notes table.".into());
        }
        let have = columns(&db, "ZICCLOUDSYNCINGOBJECT");
        let col = |c: &str| {
            if have.iter().any(|h| h == c) {
                format!("n.{c}")
            } else {
                "NULL".into()
            }
        };
        let created = ["ZCREATIONDATE1", "ZCREATIONDATE3", "ZCREATIONDATE"]
            .into_iter()
            .find(|c| have.iter().any(|h| h == c))
            .map_or("NULL".into(), |c| format!("n.{c}"));
        let folder_title = if have.iter().any(|h| h == "ZTITLE2") {
            "f.ZTITLE2"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT n.Z_PK, {title}, {snippet}, {modified}, {created}, {marked}, {folder_title}, d.ZDATA
             FROM ZICCLOUDSYNCINGOBJECT n
             LEFT JOIN ZICNOTEDATA d ON d.Z_PK = n.ZNOTEDATA
             LEFT JOIN ZICCLOUDSYNCINGOBJECT f ON f.Z_PK = {folder}
             WHERE n.ZNOTEDATA IS NOT NULL",
            title = col("ZTITLE1"),
            snippet = col("ZSNIPPET"),
            modified = col("ZMODIFICATIONDATE1"),
            marked = col("ZMARKEDFORDELETION"),
            folder = col("ZFOLDER"),
        );
        let mut st = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, Option<f64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<Vec<u8>>>(7)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (id, title, snippet, modified, created, marked, folder, data) in rows.flatten() {
            let text = data
                .as_deref()
                .and_then(note_text)
                .or(snippet)
                .unwrap_or_default();
            let title = title
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| first_line(&text));
            if title.is_empty() && text.is_empty() {
                continue;
            }
            let folder = folder.unwrap_or_default();
            out.push(Note {
                id,
                title,
                deleted: marked == Some(1) || folder == "Recently Deleted",
                folder,
                modified: date(modified),
                created: date(created),
                recovered: false,
                text,
            });
        }
        out.sort_by(|a, b| b.modified.cmp(&a.modified));
        Ok(out)
    }

    /// Notes deleted for good, rebuilt from the database's leftovers: their
    /// text, and the title where it survives.
    pub fn notes_deleted(&self) -> Result<Vec<Note>, String> {
        let (rows, tables) = self.deleted_rows(NOTES_DOMAIN, NOTES_DB)?;
        let at = |tbl: &str, c: &str| {
            tables
                .iter()
                .find(|t| t.name == tbl)
                .and_then(|t| t.columns.iter().position(|x| x.name == c))
        };
        let mut out: Vec<Note> = Vec::new();
        let real = |c: &Cell| match c {
            Cell::Real(v) => Some(*v),
            Cell::Int(v) => Some(*v as f64),
            _ => None,
        };
        // The text of a note is in its ZICNOTEDATA row.
        if let Some(data) = at("ZICNOTEDATA", "ZDATA") {
            for r in rows.iter().filter(|r| r.table == "ZICNOTEDATA") {
                if let Cell::Blob(b) = &r.cells[data] {
                    if let Some(text) = note_text(b).filter(|t| !t.trim().is_empty()) {
                        out.push(Note {
                            id: -(out.len() as i64) - 1,
                            title: first_line(&text),
                            folder: String::new(),
                            modified: None,
                            created: None,
                            deleted: true,
                            recovered: true,
                            text,
                        });
                    }
                }
            }
        }
        // The title and dates are in the ZICCLOUDSYNCINGOBJECT row.
        if let (Some(title), Some(note)) = (
            at("ZICCLOUDSYNCINGOBJECT", "ZTITLE1"),
            at("ZICCLOUDSYNCINGOBJECT", "ZNOTEDATA"),
        ) {
            let (snip, modi) = (
                at("ZICCLOUDSYNCINGOBJECT", "ZSNIPPET"),
                at("ZICCLOUDSYNCINGOBJECT", "ZMODIFICATIONDATE1"),
            );
            for r in rows.iter().filter(|r| r.table == "ZICCLOUDSYNCINGOBJECT") {
                let Cell::Text(t) = &r.cells[title] else {
                    continue;
                };
                if matches!(r.cells[note], Cell::Null) || t.is_empty() {
                    continue;
                }
                let snippet = snip
                    .and_then(|i| {
                        if let Cell::Text(s) = &r.cells[i] {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let modified = modi
                    .and_then(|i| real(&r.cells[i]))
                    .and_then(|v| date(Some(v)));
                // The same note as one found above: give it its title and date.
                match out
                    .iter_mut()
                    .find(|n| n.title == *t || (!snippet.is_empty() && n.text.contains(&snippet)))
                {
                    Some(n) => {
                        n.title = t.clone();
                        n.modified = n.modified.clone().or(modified);
                    }
                    None => out.push(Note {
                        id: -(out.len() as i64) - 1,
                        title: t.clone(),
                        folder: String::new(),
                        modified,
                        created: None,
                        deleted: true,
                        recovered: true,
                        text: snippet,
                    }),
                }
            }
        }
        out.sort_by(|a, b| b.modified.cmp(&a.modified));
        Ok(out)
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

/// The text of a note from `ZICNOTEDATA.ZDATA`: gzip, then a protobuf in which
/// the text is field 2 of field 3 of field 2.
pub fn note_text(blob: &[u8]) -> Option<String> {
    let data: Vec<u8> = if blob.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(blob)
            .take(64 << 20)
            .read_to_end(&mut out)
            .ok()?;
        out
    } else {
        blob.to_vec()
    };
    let doc = pb_field(&data, 2)?;
    let note = pb_field(doc, 3)?;
    let text = pb_field(note, 2)?;
    let s = String::from_utf8_lossy(text).replace('\u{fffc}', "");
    (!s.trim().is_empty()).then_some(s)
}

/// The first length-delimited field numbered `want` in a protobuf message.
fn pb_field(mut buf: &[u8], want: u64) -> Option<&[u8]> {
    fn varint(b: &[u8]) -> Option<(u64, usize)> {
        let mut v = 0u64;
        for (i, &x) in b.iter().enumerate().take(10) {
            v |= u64::from(x & 0x7f) << (7 * i);
            if x & 0x80 == 0 {
                return Some((v, i + 1));
            }
        }
        None
    }
    while !buf.is_empty() {
        let (key, n) = varint(buf)?;
        buf = &buf[n..];
        let (field, wire) = (key >> 3, key & 7);
        let skip = match wire {
            0 => varint(buf)?.1,
            1 => 8,
            5 => 4,
            2 => {
                let (len, n) = varint(buf)?;
                let end = n.checked_add(len as usize)?;
                let body = buf.get(n..end)?;
                if field == want {
                    return Some(body);
                }
                end
            }
            _ => return None,
        };
        buf = buf.get(skip..)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ld(field: u64, data: &[u8]) -> Vec<u8> {
        let mut v = vec![(field << 3 | 2) as u8, data.len() as u8];
        assert!(data.len() < 128);
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn note_text_comes_out_of_the_protobuf() {
        let body = ld(
            2,
            &ld(
                3,
                &[ld(1, b"x"), ld(2, "Hello\nSecond line".as_bytes())].concat(),
            ),
        );
        assert_eq!(note_text(&body).as_deref(), Some("Hello\nSecond line"));
        assert_eq!(note_text(b"not a note"), None);
        assert_eq!(first_line("\n  Title here \nmore"), "Title here");
    }

    #[test]
    fn jids_and_message_kinds() {
        assert_eq!(jid_name("4915551234@s.whatsapp.net"), "4915551234");
        assert_eq!(kind_of(1), "photo");
        assert_eq!(kind_of(99), "other");
    }
}

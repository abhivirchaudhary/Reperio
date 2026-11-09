//! `reperio ios`: iPhones and iPads. Reperio backs the phone up (with the
//! `libimobiledevice` tools: `brew install libimobiledevice`) or opens a
//! backup Finder already made, and recovers from it: photos, including those
//! in Recently Deleted, messages, and any file. Nothing is written to the
//! phone. The work is in `reperio_engine::ios`, shared with the app.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};
use clap::Subcommand;
use reperio_engine::ios::{self, Backup, Tools};

#[derive(Subcommand)]
pub enum IosCmd {
    /// List connected iPhones and iPads, and whether each trusts this computer.
    Devices,
    /// Ask the device to trust this computer (tap Trust on it).
    Pair {
        /// From `reperio ios devices`.
        udid: String,
    },
    /// Back the device up into a folder. The backup can then be read with the
    /// other commands. Unencrypted backups only.
    Backup {
        udid: String,
        /// Folder to back up into. Made if it does not exist.
        dest: PathBuf,
    },
    /// List the backups on this computer (Finder's, and any folders given).
    Backups {
        /// More folders that hold backups.
        folders: Vec<PathBuf>,
    },
    /// List the photos and videos in a backup.
    Photos {
        /// A backup folder (from `reperio ios backups`).
        backup: PathBuf,
        /// Only those in Recently Deleted.
        #[arg(long)]
        deleted: bool,
    },
    /// List text messages in a backup.
    Messages {
        backup: PathBuf,
        /// Only messages containing this, or from this contact.
        #[arg(long, default_value = "")]
        search: String,
        /// Only messages deleted on the phone.
        #[arg(long)]
        deleted: bool,
        /// Also print text left in the message database's free pages.
        #[arg(long)]
        fragments: bool,
    },
    /// Recover deleted rows from any database in a backup: messages, notes,
    /// contacts, WhatsApp, Safari history. Whole rows, with their columns.
    Deleted {
        backup: PathBuf,
        /// The database, as its path ends: `sms.db`, `NoteStore.sqlite`,
        /// `ChatStorage.sqlite`, `AddressBook.sqlitedb` ... (see `--files`).
        #[arg(long, default_value = "sms.db")]
        db: String,
    },
    /// List WhatsApp chats and messages in a backup.
    Whatsapp {
        backup: PathBuf,
        /// Only this chat (part of its name).
        #[arg(long)]
        chat: Option<String>,
        /// Also recover messages deleted for good, from the database's leftovers.
        #[arg(long)]
        deleted: bool,
    },
    /// List notes in a backup, including Recently Deleted.
    Notes {
        backup: PathBuf,
        /// Also recover notes deleted for good, from the database's leftovers.
        #[arg(long)]
        deleted: bool,
        /// Print each note's text.
        #[arg(long)]
        text: bool,
    },
    /// Run analyzer plug-ins on a backup (plug-ins whose targets include
    /// `ios-backup`). Their reports go in DEST/<plug-in name>/.
    Analyze {
        backup: PathBuf,
        dest: PathBuf,
        /// Only this plug-in.
        #[arg(long)]
        plugin: Option<String>,
    },
    /// Copy photos or files out of a backup.
    Export {
        backup: PathBuf,
        /// Folder to copy into.
        dest: PathBuf,
        /// Copy photos and videos (named as on the phone).
        #[arg(long)]
        photos: bool,
        /// With --photos: only those in Recently Deleted.
        #[arg(long)]
        deleted: bool,
        /// Copy files whose path contains this, with their folders.
        #[arg(long)]
        files: Option<String>,
    },
}

pub fn run(cmd: IosCmd) -> Result<()> {
    let tools = Tools::default();
    match cmd {
        IosCmd::Devices => {
            let missing = tools.missing();
            if !missing.is_empty() {
                println!(
                    "Missing: {}. In Terminal: brew install libimobiledevice\n",
                    missing.join(", ")
                );
            }
            let found = ios::devices(&tools).map_err(anyhow::Error::msg)?;
            if found.is_empty() {
                println!("No iPhone or iPad found. Connect it with a cable and unlock it.");
            }
            for d in found {
                println!("{}  ({})", d.name, d.udid);
                if let Some(p) = &d.problem {
                    println!("  {p}");
                    continue;
                }
                println!("  {} · iOS {} · serial {}", d.model, d.ios, d.serial);
                if let (Some(t), Some(f)) = (d.storage_total, d.storage_free) {
                    println!(
                        "  storage: {:.1} GB free of {:.1} GB",
                        f as f64 / 1e9,
                        t as f64 / 1e9
                    );
                }
                if let Some(b) = d.battery {
                    println!("  battery: {b}%");
                }
                if d.encrypts_backups == Some(true) {
                    println!("  backups are encrypted: Reperio can only read unencrypted ones");
                }
            }
            Ok(())
        }
        IosCmd::Pair { udid } => {
            println!("Tap Trust on the device, and enter its passcode if asked…");
            ios::pair(&tools, &udid).map_err(anyhow::Error::msg)?;
            println!("This computer is trusted.");
            Ok(())
        }
        IosCmd::Backup { udid, dest } => {
            let mut last = 255;
            let dir = ios::backup(&tools, &udid, &dest, &AtomicBool::new(false), &mut |p| {
                if p.percent != last && p.percent > 0 {
                    last = p.percent;
                    eprint!("\rBacking up: {}%  ", p.percent);
                }
            })
            .map_err(anyhow::Error::msg)?;
            eprintln!();
            println!("Backup saved to {}", dir.display());
            Ok(())
        }
        IosCmd::Backups { folders } => {
            let mut all = folders;
            all.extend(ios::finder_backups_folder());
            let list = ios::backups(&all);
            if list.is_empty() {
                println!("No backups found. (On a Mac, Finder's folder needs Full Disk Access for this program.)");
            }
            for b in list {
                println!(
                    "{}\n  {} · iOS {} · {}{}",
                    b.path,
                    b.name,
                    b.ios,
                    b.date,
                    if b.encrypted {
                        " · ENCRYPTED (cannot be read)"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        IosCmd::Photos { backup, deleted } => {
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            let photos: Vec<_> = b
                .photos()
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .filter(|p| !deleted || p.deleted)
                .collect();
            for p in &photos {
                println!(
                    "{}{}  {}  {}{}",
                    if p.deleted { "[recently deleted] " } else { "" },
                    p.name,
                    p.date.as_deref().unwrap_or("undated"),
                    if p.file_id.is_some() {
                        format!("{:.1} KB", p.size as f64 / 1e3)
                    } else {
                        "not in the backup".into()
                    },
                    if p.video { " (video)" } else { "" }
                );
            }
            println!("{} photos and videos.", photos.len());
            Ok(())
        }
        IosCmd::Messages {
            backup,
            search,
            deleted,
            fragments,
        } => {
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            let msgs: Vec<_> = b
                .messages(&search, 100_000)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .filter(|m| !deleted || m.deleted)
                .collect();
            for m in &msgs {
                println!(
                    "{} {} {}: {}{}",
                    m.date.as_deref().unwrap_or("undated"),
                    if m.from_me { "to" } else { "from" },
                    m.with,
                    m.text,
                    if m.deleted {
                        "  [recently deleted]"
                    } else {
                        ""
                    }
                );
            }
            println!("{} messages.", msgs.len());
            if fragments {
                let f = b.message_fragments();
                println!(
                    "\nText left in free pages (deleted, may be partial): {} pieces",
                    f.len()
                );
                for x in f.iter().take(200) {
                    println!(
                        "  [page {}] {}",
                        x.page,
                        x.text.chars().take(160).collect::<String>()
                    );
                }
            }
            Ok(())
        }
        IosCmd::Whatsapp {
            backup,
            chat,
            deleted,
        } => {
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            let wa = b.whatsapp().map_err(anyhow::Error::msg)?;
            println!("{} chats, {} messages", wa.chats.len(), wa.messages.len());
            let wanted = chat.as_deref().map(str::to_lowercase);
            let keep = |name: &str| {
                wanted
                    .as_ref()
                    .is_none_or(|w| name.to_lowercase().contains(w))
            };
            for c in &wa.chats {
                println!(
                    "  {}{}  {} messages  {}",
                    c.name,
                    if c.group { " (group)" } else { "" },
                    c.messages,
                    c.last.as_deref().unwrap_or("")
                );
            }
            for m in wa.messages.iter().filter(|m| keep(&m.chat_name)) {
                println!(
                    "{} [{}] {}: {}{}",
                    m.date.as_deref().unwrap_or("undated"),
                    m.chat_name,
                    m.who,
                    if m.text.is_empty() {
                        format!("<{}>", m.kind)
                    } else {
                        m.text.clone()
                    },
                    if m.media.is_some() {
                        "  (media in backup)"
                    } else {
                        ""
                    }
                );
            }
            if deleted {
                let lost = b.whatsapp_deleted().map_err(anyhow::Error::msg)?;
                println!(
                    "\nRecovered, deleted for good: {} messages (may be incomplete)",
                    lost.len()
                );
                for m in lost.iter().filter(|m| keep(&m.chat_name)) {
                    println!(
                        "{} [{}] {}: {}",
                        m.date.as_deref().unwrap_or("undated"),
                        m.chat_name,
                        m.who,
                        m.text
                    );
                }
            }
            Ok(())
        }
        IosCmd::Notes {
            backup,
            deleted,
            text,
        } => {
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            let notes = b.notes().map_err(anyhow::Error::msg)?;
            for n in &notes {
                println!(
                    "{}{}  [{}]  {}",
                    n.title,
                    if n.deleted {
                        "  (Recently Deleted)"
                    } else {
                        ""
                    },
                    n.folder,
                    n.modified.as_deref().unwrap_or("")
                );
                if text {
                    println!("{}\n", n.text);
                }
            }
            println!("{} notes.", notes.len());
            if deleted {
                let lost = b.notes_deleted().map_err(anyhow::Error::msg)?;
                println!(
                    "\nRecovered, deleted for good: {} notes (may be incomplete)",
                    lost.len()
                );
                for n in &lost {
                    println!("{}  {}", n.title, n.modified.as_deref().unwrap_or(""));
                    if text {
                        println!("{}\n", n.text);
                    }
                }
            }
            Ok(())
        }
        IosCmd::Analyze {
            backup,
            dest,
            plugin,
        } => {
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            crate::android::run_analyzers(
                "ios-backup",
                &reperio_engine::analyzer::Target {
                    device: None,
                    image: None,
                    backup: Some(&b),
                },
                &dest,
                plugin.as_deref(),
                &[],
            )
        }
        IosCmd::Deleted { backup, db } => {
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            let found: Vec<_> = b
                .files(None, &db, 100)
                .into_iter()
                .filter(|f| f.path.ends_with(&db) || f.path.contains(&db))
                .collect();
            let Some(file) = found.first() else {
                bail!("No database matching \"{db}\" in the backup.")
            };
            println!("{} ({})", file.path, file.domain);
            let (rows, _) = b
                .deleted_rows(&file.domain, &file.path)
                .map_err(anyhow::Error::msg)?;
            for r in &rows {
                let cells: Vec<String> = r
                    .cells
                    .iter()
                    .map(|c| match c {
                        reperio_engine::sqlite_carve::Cell::Null => "·".into(),
                        reperio_engine::sqlite_carve::Cell::Int(n) => n.to_string(),
                        reperio_engine::sqlite_carve::Cell::Real(n) => n.to_string(),
                        reperio_engine::sqlite_carve::Cell::Text(t) => {
                            format!("{:?}", t.chars().take(80).collect::<String>())
                        }
                        reperio_engine::sqlite_carve::Cell::Blob(x) => {
                            format!("<{} bytes>", x.len())
                        }
                    })
                    .collect();
                println!(
                    "{} [{} p{}]  {}",
                    r.table,
                    r.from,
                    r.page,
                    cells.join(" | ")
                );
            }
            println!("{} deleted rows.", rows.len());
            Ok(())
        }
        IosCmd::Export {
            backup,
            dest,
            photos,
            deleted,
            files,
        } => {
            if !photos && files.is_none() {
                bail!("Say what to copy: --photos, or --files TEXT.");
            }
            let b = Backup::open(&backup).map_err(anyhow::Error::msg)?;
            if photos {
                let list: Vec<_> = b
                    .photos()
                    .map_err(anyhow::Error::msg)?
                    .into_iter()
                    .filter(|p| !deleted || p.deleted)
                    .collect();
                let n = b.export_photos(&list, &dest).map_err(anyhow::Error::msg)?;
                println!(
                    "Copied {n} of {} photos and videos to {}.",
                    list.len(),
                    dest.display()
                );
            }
            if let Some(text) = files {
                let ids: Vec<String> = b
                    .files(None, &text, 1_000_000)
                    .into_iter()
                    .map(|f| f.id)
                    .collect();
                let n = b.export_files(&ids, &dest).map_err(anyhow::Error::msg)?;
                println!("Copied {n} files to {}.", dest.display());
            }
            Ok(())
        }
    }
}

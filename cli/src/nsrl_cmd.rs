//! `reperio nsrl`: manage the Known File Filter (NSRL) hash database.

use std::path::PathBuf;

use clap::Subcommand;
use reperio_engine::nsrl::NsrlFilter;

#[derive(Subcommand)]
pub enum NsrlCmd {
    /// Show the status of the NSRL database: path, hash count.
    Status {
        /// Path to the NSRL database (default: auto-locate).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Import hashes from a text file (one SHA-1 per line).
    Import {
        /// Path to the NSRL database to create or append to.
        #[arg(long)]
        db: PathBuf,
        /// File to import: one SHA-1 hex string per line.
        file: PathBuf,
        /// The file is an NIST NSRL RDS pipe-delimited export.
        #[arg(long)]
        rds: bool,
    },
    /// Check whether a SHA-1 hash is known.
    Check {
        /// Path to the NSRL database.
        #[arg(long)]
        db: PathBuf,
        /// SHA-1 hex string to check.
        hash: String,
    },
    /// Build a lightweight "common OS" filter with a handful of known hashes.
    BuildLightweight {
        /// Where to write the lightweight database.
        output: PathBuf,
    },
}

pub fn run(cmd: NsrlCmd) -> anyhow::Result<()> {
    match cmd {
        NsrlCmd::Status { db } => {
            let path = db.unwrap_or_else(default_nsrl_path);
            if !path.exists() {
                println!("No NSRL database at {}.", path.display());
                println!(
                    "Create one with: reperio nsrl import --db {} <file>",
                    path.display()
                );
                return Ok(());
            }
            let filter = NsrlFilter::open(&path).map_err(anyhow::Error::msg)?;
            println!("Database: {}", path.display());
            println!("Hashes: {}", filter.len());
        }
        NsrlCmd::Import { db, file, rds } => {
            let mut filter = if db.exists() {
                NsrlFilter::open(&db).map_err(anyhow::Error::msg)?
            } else {
                NsrlFilter::create(&db).map_err(anyhow::Error::msg)?
            };
            let n = if rds {
                filter.import_nsrl_rds(&file).map_err(anyhow::Error::msg)?
            } else {
                filter.import_text(&file).map_err(anyhow::Error::msg)?
            };
            println!("Imported {n} hashes into {}.", db.display());
            println!("Total: {}", filter.len());
        }
        NsrlCmd::Check { db, hash } => {
            let filter = NsrlFilter::open(&db).map_err(anyhow::Error::msg)?;
            if filter.is_known(&hash) {
                println!("KNOWN: {hash} is in the NSRL database.");
            } else {
                println!("UNKNOWN: {hash} is not in the NSRL database.");
            }
        }
        NsrlCmd::BuildLightweight { output } => {
            let filter = NsrlFilter::build_lightweight(&output).map_err(anyhow::Error::msg)?;
            println!(
                "Built lightweight NSRL filter at {} with {} hashes.",
                output.display(),
                filter.len()
            );
        }
    }
    Ok(())
}

fn default_nsrl_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Reperio")
    } else if cfg!(windows) {
        home.join("AppData/Roaming/Reperio")
    } else {
        home.join(".local/share/reperio")
    };
    dir.join("nsrl.sqlite")
}

//! `reperio case`: manage forensic cases, evidence, and chain of custody.

use clap::Subcommand;
use reperio_engine::case::{CaseManager, CaseStatus};

#[derive(Subcommand)]
pub enum CaseCmd {
    /// Create a new case.
    Create {
        /// Case name.
        name: String,
        /// Lead investigator.
        #[arg(long)]
        investigator: String,
        /// Optional description.
        #[arg(long)]
        description: Option<String>,
    },
    /// List all cases.
    List,
    /// Show details of a case and its evidence.
    Show {
        /// Case ID.
        id: u64,
    },
    /// Close a case.
    Close {
        /// Case ID.
        id: u64,
    },
    /// Delete a case and all its evidence (irreversible).
    Delete {
        /// Case ID.
        id: u64,
    },
    /// Add an evidence item to a case.
    AddEvidence {
        /// Case ID.
        case_id: u64,
        /// Path to the source (disk, image, etc.).
        source: String,
        /// Type of source: disk, image, memory, phone, raid.
        #[arg(long, value_name = "TYPE")]
        source_type: String,
        /// Who seized the evidence.
        #[arg(long)]
        seized_by: Option<String>,
        /// Size in bytes (optional).
        #[arg(long)]
        size: Option<u64>,
        /// Acquisition hash (optional).
        #[arg(long)]
        hash: Option<String>,
        /// Notes.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Show evidence items for a case.
    Evidence {
        /// Case ID.
        case_id: u64,
    },
    /// Show the chain of custody for an evidence item.
    Custody {
        /// Evidence ID.
        evidence_id: u64,
    },
    /// Record a custody transfer for an evidence item.
    Transfer {
        /// Evidence ID.
        evidence_id: u64,
        /// Analyst handing it over (optional for initial seizure).
        #[arg(long)]
        from: Option<String>,
        /// Analyst receiving it.
        #[arg(long)]
        to: String,
        /// Action: seized, imaged, analyzed, returned, transferred.
        #[arg(long, value_name = "ACTION")]
        action: String,
        /// Notes.
        #[arg(long)]
        notes: Option<String>,
    },
}

pub fn run(cmd: CaseCmd) -> anyhow::Result<()> {
    let mgr = CaseManager::open().map_err(anyhow::Error::msg)?;
    match cmd {
        CaseCmd::Create {
            name,
            investigator,
            description,
        } => {
            let id = mgr
                .create_case(&name, &investigator, description.as_deref())
                .map_err(anyhow::Error::msg)?;
            println!("Created case {id}: {name}");
        }
        CaseCmd::List => {
            let cases = mgr.list_cases().map_err(anyhow::Error::msg)?;
            if cases.is_empty() {
                println!("No cases.");
            } else {
                println!("{:<6} {:<12} {:<20} Name", "ID", "Status", "Investigator");
                for c in cases {
                    println!(
                        "{:<6} {:<12} {:<20} {}",
                        c.id, c.status, c.investigator, c.name
                    );
                }
            }
        }
        CaseCmd::Show { id } => {
            let case_opt = mgr.get_case(id).map_err(anyhow::Error::msg)?;
            let c = case_opt.ok_or_else(|| anyhow::anyhow!("case {id} not found"))?;
            println!("Case {}: {}", c.id, c.name);
            println!("  Status: {}", c.status);
            println!("  Investigator: {}", c.investigator);
            if let Some(ref d) = c.description {
                println!("  Description: {d}");
            }
            println!("  Created: {}", format_time(c.created_at));
            let evs = mgr.list_evidence(id).map_err(anyhow::Error::msg)?;
            println!("  Evidence items: {}", evs.len());
            for ev in evs {
                println!("    {}: {} ({})", ev.id, ev.source_path, ev.source_type);
            }
        }
        CaseCmd::Close { id } => {
            if mgr
                .update_case_status(id, CaseStatus::Closed)
                .map_err(anyhow::Error::msg)?
            {
                println!("Closed case {id}.");
            } else {
                anyhow::bail!("case {id} not found");
            }
        }
        CaseCmd::Delete { id } => {
            if mgr.delete_case(id).map_err(anyhow::Error::msg)? {
                println!("Deleted case {id}.");
            } else {
                anyhow::bail!("case {id} not found");
            }
        }
        CaseCmd::AddEvidence {
            case_id,
            source,
            source_type,
            seized_by,
            size,
            hash,
            notes,
        } => {
            let ev_id = mgr
                .add_evidence(
                    case_id,
                    &source,
                    &source_type,
                    size,
                    hash.as_deref(),
                    seized_by.as_deref(),
                    notes.as_deref(),
                )
                .map_err(anyhow::Error::msg)?;
            println!("Added evidence {ev_id} to case {case_id}.");
        }
        CaseCmd::Evidence { case_id } => {
            let evs = mgr.list_evidence(case_id).map_err(anyhow::Error::msg)?;
            if evs.is_empty() {
                println!("No evidence for case {case_id}.");
            } else {
                for ev in evs {
                    println!(
                        "{}: {} (type: {}, seized_by: {}, hash: {})",
                        ev.id,
                        ev.source_path,
                        ev.source_type,
                        ev.seized_by.as_deref().unwrap_or("?"),
                        ev.acquisition_hash.as_deref().unwrap_or("?")
                    );
                }
            }
        }
        CaseCmd::Custody { evidence_id } => {
            let chain = mgr.custody_chain(evidence_id).map_err(anyhow::Error::msg)?;
            if chain.is_empty() {
                println!("No custody entries for evidence {evidence_id}.");
            } else {
                for entry in chain {
                    let from = entry.from_analyst.as_deref().unwrap_or("(initial)");
                    println!(
                        "{}: {} → {} | {} | {}",
                        format_time(entry.timestamp),
                        from,
                        entry.to_analyst,
                        entry.action,
                        entry.notes.as_deref().unwrap_or("")
                    );
                }
            }
        }
        CaseCmd::Transfer {
            evidence_id,
            from,
            to,
            action,
            notes,
        } => {
            mgr.custody_transfer(evidence_id, from.as_deref(), &to, &action, notes.as_deref())
                .map_err(anyhow::Error::msg)?;
            println!("Recorded custody transfer for evidence {evidence_id}.");
        }
    }
    Ok(())
}

fn format_time(ts: u64) -> String {
    let dt = std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts);
    let datetime: chrono::DateTime<chrono::Utc> = dt.into();
    datetime.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

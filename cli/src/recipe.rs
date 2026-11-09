//! `reperio recipe`: a whole job in one file. A source (a disk, an image, or a
//! device a source plug-in reaches), the reader plug-ins to use, the
//! analyzers to run on it afterwards, and where recovered files go. The
//! plug-ins chain: a source plug-in provides the bytes, readers read the file
//! systems on them, analyzers look at what is left and offer files of their
//! own, and everything is recovered together.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use reperio_engine::session::{Layout, ScanOptions};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Subcommand)]
pub enum RecipeCmd {
    /// Run a recipe.
    Run {
        /// The recipe file (JSON).
        file: PathBuf,
    },
    /// Print an example recipe to start from.
    Example,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    /// What to scan: a disk, an image file, or `plugin:NAME:DEVICE`.
    source: String,
    /// Reader plug-ins to use; leave out for all installed, `[]` for none.
    #[serde(default)]
    readers: Option<Vec<String>>,
    /// Analyzer plug-ins to run on the source after the scan.
    #[serde(default)]
    analyzers: Vec<String>,
    /// Settings for plug-ins in this run only: {"plug-in": {"option": value}}.
    #[serde(default)]
    options: Map<String, Value>,
    /// Search the free space for files (default true).
    #[serde(default = "yes")]
    deep: bool,
    /// Also list files still on the card.
    #[serde(default)]
    existing: bool,
    /// Also recover files that are damaged.
    #[serde(default)]
    damaged: bool,
    /// `original` (default), `by-date` or `by-type`.
    #[serde(default)]
    layout: Option<String>,
    /// Only look for these formats in the deep scan.
    #[serde(default)]
    types: Vec<String>,
    /// Where recovered files go. Leave out to only list what is found.
    #[serde(default)]
    recover: Option<PathBuf>,
    /// Where analyzer reports go.
    #[serde(default)]
    reports: Option<PathBuf>,
    /// Save the scan's results to a file to open later in the app.
    #[serde(default)]
    save: Option<PathBuf>,
}

fn yes() -> bool {
    true
}

const EXAMPLE: &str = r#"{
  "source": "plugin:ios-usb:00008030-0012",
  "readers": ["myfs"],
  "analyzers": ["whatsapp-media"],
  "options": { "whatsapp-media": { "deep": true } },
  "deep": true,
  "layout": "original",
  "recover": "./recovered",
  "reports": "./reports"
}
"#;

pub fn run(cmd: RecipeCmd) -> Result<()> {
    match cmd {
        RecipeCmd::Example => {
            print!("{EXAMPLE}");
            Ok(())
        }
        RecipeCmd::Run { file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("could not read {}", file.display()))?;
            let r: Recipe = serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is not a valid recipe (see `reperio recipe example`)",
                    file.display()
                )
            })?;
            execute(r)
        }
    }
}

fn execute(r: Recipe) -> Result<()> {
    let layout = match r.layout.as_deref().unwrap_or("original") {
        "original" => Layout::Original,
        "by-date" => Layout::ByDate,
        "by-type" => Layout::ByType,
        other => bail!("layout must be original, by-date or by-type, not {other}"),
    };
    // What the recipe sets applies to this run only.
    reperio_engine::plugin::set_option_overrides(Some(r.options));
    let list = r.recover.is_none();
    let args = crate::ScanArgs {
        analyzers: r.analyzers,
        reports: r.reports,
        output: r.recover.unwrap_or_else(|| PathBuf::from(".")),
        list,
        existing: r.existing,
        damaged: r.damaged,
        layout,
        save: r.save,
        vault: Vec::new(),
    };
    let opts = ScanOptions {
        plugins: r.readers,
        deep: r.deep,
        existing: r.existing,
        formats: r.types,
        resume: None,
        find_partitions: false,
        case_id: None,
        evidence_id: None,
        nsrl_db: None,
    };
    let source = PathBuf::from(&r.source);
    let src = crate::open_source(&source)?;
    crate::scan(&r.source, &[source], &src, &opts, &args)
}

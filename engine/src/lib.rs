//! Reperio's recovery engine.
//!
//! The engine only ever reads from the source it scans. Recovered files are
//! carved from raw sectors by walking each format's structure, so they come
//! back whole and at their exact size even after the filesystem has
//! forgotten them, including after a quick format.

pub mod analyzer;
pub mod android;
pub mod audiofix;
pub mod camera;
pub mod case;
pub mod devices;
pub mod docfix;
pub mod extent;
pub mod fixfile;
pub mod forensic;
pub mod formats;
pub mod fs;
pub mod health;
pub mod imaging;
pub mod ios;
pub mod ios_apps;
pub mod nsrl;
pub mod pdftext;
pub mod photofix;
pub mod plugin;
pub mod plugin_check;
pub mod plugin_install;
pub mod preview;
pub mod raid;
pub mod reader;
pub mod remote;
pub mod repair;
pub mod safety;
pub mod scan;
pub mod session;
pub mod similar;
pub mod source;
pub mod sqlite_carve;
pub mod surface;
pub mod vault;
pub mod videofix;

pub use formats::{DateTime, Kind};
pub use scan::{extract, Hit, Progress, Scanner};
pub use source::{DeviceSource, Source};

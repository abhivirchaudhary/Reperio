//! Your own scanners. Drop a file in this directory and it is built and used
//! on the next `cargo build`; nothing else needs editing. See `_template.rs`
//! and `docs/custom-scanners.md`.

include!(concat!(env!("OUT_DIR"), "/custom_formats.rs"));

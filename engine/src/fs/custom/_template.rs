//! Template for your own file-system reader (for example an NTFS recovery
//! tool of your own). Copy to `my_ntfs.rs` (lower-case letters, digits and
//! `_`), fill in `read`, and `cargo build`.
//!
//! It does not replace Reperio's reader. Files, names and snapshots you
//! return are added to what the built-in reader found (a file with the same
//! folder, name, size and deleted flag is counted once), and if the built-in
//! reader finds nothing, your volume is used on its own. A panic in your
//! code is caught and your reader is skipped for that volume.
//!
//! Everything is read-only: `src` can only be read from.

#[allow(unused_imports)]
use crate::extent::Extent;
#[allow(unused_imports)]
use crate::fs::FsFile;
use crate::fs::{FsType, Volume, VolumeReader};
use crate::source::Source;

pub struct MyReader;

impl VolumeReader for MyReader {
    fn name(&self) -> &'static str {
        "my-ntfs"
    }

    /// Runs on volumes the built-in code recognises as this type.
    fn fs(&self) -> FsType {
        FsType::Ntfs
    }

    fn read(&self, src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
        // Read the boot sector (or whatever you need) from `offset`.
        let mut boot = [0u8; 512];
        if src.read_at(offset, &mut boot).ok()? != 512 || &boot[3..11] != b"NTFS    " {
            return None;
        }
        let cluster_size =
            u64::from(u16::from_le_bytes([boot[11], boot[12]])) * u64::from(boot[13]);
        let volume = Volume::empty(FsType::Ntfs, offset, len, cluster_size);

        // Add each file you find. `extents` are byte offsets in `src`, in
        // the order of the file's data.
        //
        // volume.files.push(FsFile {
        //     dir: "Users/Mom/Pictures".into(),
        //     name: "IMG_0001.JPG".into(),
        //     size: 123_456,
        //     modified: None,
        //     created: None,
        //     deleted: true,
        //     extents: vec![Extent::new(offset + 4096 * 100, 123_456)],
        //     overwritten: false,
        //     alternatives: Vec::new(),
        // });
        Some(volume)
    }
}

/// Called by `build.rs`.
pub fn reader() -> Box<dyn VolumeReader> {
    Box::new(MyReader)
}

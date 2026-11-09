//! Shared by the targets: what the app does with a volume.
#![allow(dead_code)]

use reperio_engine::extent::MappedSource;
use reperio_engine::fs;
use reperio_engine::source::Source;

/// Opens the volume at the start of `data` and reads a little of every
/// file it lists, as recovery would.
pub fn open_and_touch(data: &Vec<u8>) {
    let src: &dyn Source = data;
    if let Some(v) = fs::open(src, 0, src.len()) {
        let mut buf = [0u8; 512];
        for f in v.files.iter().take(1000) {
            for layout in std::iter::once(&f.extents).chain(&f.alternatives) {
                let _ = MappedSource::new(src, layout.clone()).read_at(0, &mut buf);
            }
        }
        let free = MappedSource::new(src, v.free.clone());
        let _ = free.read_at(free.len() / 2, &mut buf);
    }
}

/// `data` with `sig` written at `at`, padded so it fits: steers the fuzzer
/// straight to one filesystem's parser.
pub fn with_signature(data: &[u8], at: usize, sig: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.len() < at + sig.len() {
        v.resize(at + sig.len(), 0);
    }
    v[at..at + sig.len()].copy_from_slice(sig);
    v
}

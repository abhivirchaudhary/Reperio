#![no_main]
//! MBR, extended and GPT partition tables, and filesystem detection.
use libfuzzer_sys::fuzz_target;
use reperio_engine::fs::{self, partition};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    for p in partition::partitions(&data) {
        let _ = fs::detect(&data, p.offset);
    }
});

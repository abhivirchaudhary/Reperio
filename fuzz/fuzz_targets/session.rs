#![no_main]
//! A whole scan, filesystem then deep, as the app runs it.
use std::sync::atomic::AtomicBool;

use libfuzzer_sys::fuzz_target;
use reperio_engine::session::{self, ScanOptions};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    let _ = session::scan(
        &data,
        "fuzz",
        &ScanOptions::default(),
        &AtomicBool::new(false),
        |_| {},
    );
});

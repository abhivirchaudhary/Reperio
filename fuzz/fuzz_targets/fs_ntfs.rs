#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::open_and_touch(&common::with_signature(data, 3, b"NTFS    "));
});

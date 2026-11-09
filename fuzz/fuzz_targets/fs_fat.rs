#![no_main]
//! FAT12/16/32. No signature to plant: the boot sector's fields decide.
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::open_and_touch(&data.to_vec());
});

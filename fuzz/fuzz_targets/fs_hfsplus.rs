#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    // The volume header is 1024 bytes in; a FAT-looking boot sector would
    // take precedence, so clear its signature.
    let mut v = common::with_signature(data, 1024, b"H+");
    v[0] = 0;
    common::open_and_touch(&v);
});

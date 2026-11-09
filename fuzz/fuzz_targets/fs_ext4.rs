#![no_main]
//! ext2, ext3 and ext4: the superblock's magic is 1080 bytes in.
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let mut v = common::with_signature(data, 1080, &[0x53, 0xEF]);
    v[0] = 0;
    common::open_and_touch(&v);
});

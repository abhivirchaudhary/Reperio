#![no_main]
//! APFS. Objects are checksummed, so the fuzzer mostly needs seeds made
//! from real containers (scripts/make_test_cards.py builds one).
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let mut v = common::with_signature(data, 32, b"NXSB");
    v[0] = 0;
    common::open_and_touch(&v);
});

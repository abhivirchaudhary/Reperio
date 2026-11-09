#![no_main]
//! Every format's structure walk on the input, as the deep scan runs it,
//! and the scan loop itself.
use libfuzzer_sys::fuzz_target;
use reperio_engine::formats::{self, HEAD_LEN};
use reperio_engine::reader::Reader;
use reperio_engine::Scanner;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    let mut head = vec![0u8; HEAD_LEN];
    let n = data.len().min(HEAD_LEN);
    head[..n].copy_from_slice(&data[..n]);
    for f in formats::all() {
        if f.matches(&head) {
            let limit = (data.len() as u64).min(f.max_len());
            if let Some(found) = f.carve(&mut Reader::new(&data, 0), 0, limit) {
                assert!(found.len <= limit, "{} carved past its limit", f.name());
            }
        }
    }
    Scanner::default().scan(&data, |_| true, |_| true);
});

#![no_main]
//! The ISO media box walk (MP4, MOV, HEIC, CR3) and the sample tables
//! that video repair reads from a `moov` box.
use libfuzzer_sys::fuzz_target;
use reperio_engine::formats;
use reperio_engine::reader::Reader;
use reperio_engine::repair;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    let mp4 = formats::all()
        .into_iter()
        .find(|f| f.name() == "mp4")
        .unwrap();
    let _ = mp4.carve(&mut Reader::new(&data, 0), 0, data.len() as u64);
    // Repair is handed the `moov` boxes the scan saw.
    let indexes: Vec<u64> = data
        .windows(4)
        .enumerate()
        .filter(|&(i, w)| w == b"moov" && i >= 4)
        .map(|(i, _)| i as u64 - 4)
        .collect();
    let _ = repair::repair_mp4(&data, 0, &[], &indexes, 512);
});

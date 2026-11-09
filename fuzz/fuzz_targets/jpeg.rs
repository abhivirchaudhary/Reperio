#![no_main]
//! The JPEG marker walk (carving, dates from EXIF), its fragment repair,
//! and thumbnail decoding, on input that always starts like a JPEG.
use libfuzzer_sys::fuzz_target;
use reperio_engine::extent::Extent;
use reperio_engine::formats::{self, HEAD_LEN};
use reperio_engine::reader::Reader;
use reperio_engine::session::{Health, Item, Origin};
use reperio_engine::{preview, repair, Kind};

fuzz_target!(|data: &[u8]| {
    let mut v = vec![0xFF, 0xD8, 0xFF];
    v.extend_from_slice(data);
    v.resize(v.len().max(HEAD_LEN), 0);
    let jpeg = formats::all()
        .into_iter()
        .find(|f| f.name() == "jpeg")
        .unwrap();
    let _ = jpeg.carve(&mut Reader::new(&v, 0), 0, v.len() as u64);
    let _ = repair::repair_jpeg(&v, 0, &[], 512);
    let item = Item {
        id: 0,
        name: "x.jpg".into(),
        dir: String::new(),
        ext: "jpg".into(),
        kind: Kind::Photo,
        origin: Origin::Carved,
        health: Health::Good,
        size: v.len() as u64,
        taken: None,
        modified: None,
        extents: vec![Extent::new(0, v.len() as u64)],
        volume: None,
        fix: None,
    };
    let _ = preview::thumbnail(&v, &item, 64);
});

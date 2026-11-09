//! Files kept by APFS snapshots, on a container built by
//! `scripts/apfs_card.py --snapshot-demo`: two Time Machine snapshots, one
//! file deleted after both were taken and one after the second. Skipped
//! when python3 is not available.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use reperio_engine::extent::MappedSource;
use reperio_engine::fs;
use reperio_engine::session::{self, Health, Origin, ScanOptions};
use reperio_engine::source::Source;

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Builds the demo container; None without python3.
fn demo(dir: &Path) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let kept = pattern(30_000, 1);
    let gone = pattern(50_000, 2);
    std::fs::write(dir.join("kept"), &kept).unwrap();
    std::fs::write(dir.join("gone"), &gone).unwrap();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/apfs_card.py");
    let image = dir.join("snap.img");
    let status = Command::new("python3")
        .arg("-I")
        .arg(&script)
        .arg("--snapshot-demo")
        .arg(&image)
        .arg(dir.join("kept"))
        .arg(dir.join("gone"))
        .status()
        .ok()?;
    assert!(status.success(), "apfs_card.py failed");
    Some((std::fs::read(&image).unwrap(), kept, gone))
}

fn read_all(src: &dyn Source, extents: &[reperio_engine::extent::Extent]) -> Vec<u8> {
    let view = MappedSource::new(src, extents.to_vec());
    let mut buf = vec![0u8; view.len() as usize];
    assert_eq!(view.read_at(0, &mut buf).unwrap(), buf.len());
    buf
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reperio-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn apfs_snapshot_files_come_back_whole() {
    let dir = scratch("apfs-snap");
    let Some((img, kept, gone)) = demo(&dir) else {
        std::fs::remove_dir_all(&dir).ok();
        return;
    };
    std::fs::remove_dir_all(&dir).ok();

    let vol = fs::open(&img, 0, img.len() as u64).unwrap();
    let live: Vec<&str> = vol.files.iter().map(|f| f.name.as_str()).collect();
    assert!(!live.contains(&"gone.bin"), "{live:?}");
    // gone.bin is in both snapshots (newest first), later.bin in the newer.
    let mut found: Vec<(&str, &str)> = vol
        .snapshots
        .iter()
        .map(|s| (s.snapshot.as_str(), s.file.name.as_str()))
        .collect();
    // Newest snapshot first; within one, any order.
    found.sort_by(|a, b| b.0.cmp(a.0).then(a.1.cmp(b.1)));
    assert!(vol.snapshots[0].snapshot.contains("05-02"));
    assert!(vol.snapshots[2].snapshot.contains("05-01"));
    assert_eq!(
        found,
        [
            ("com.apple.TimeMachine.2024-05-02-120000.local", "gone.bin"),
            ("com.apple.TimeMachine.2024-05-02-120000.local", "later.bin"),
            ("com.apple.TimeMachine.2024-05-01-120000.local", "gone.bin"),
        ],
        "{:#?}",
        vol.snapshots
    );
    let named = |n: &str| {
        &vol.snapshots
            .iter()
            .find(|s| s.file.name == n)
            .unwrap()
            .file
    };
    let snap = named("gone.bin");
    assert_eq!(snap.dir, "Documents");
    assert_eq!(read_all(&img, &snap.extents), gone);
    assert_eq!(read_all(&img, &named("later.bin").extents), kept);
    // The snapshot holds those blocks: they are not free space.
    for e in &snap.extents {
        assert!(
            !vol.free
                .iter()
                .any(|f| f.offset < e.end() && e.offset < f.end()),
            "{e:?} is listed as free"
        );
    }

    // The scan lists each once, from the newest snapshot, and recovers
    // them byte for byte.
    let result = session::scan(
        &img,
        "snap.img",
        &ScanOptions::default(),
        &AtomicBool::new(false),
        |_| {},
    );
    let mut items: Vec<_> = result
        .items
        .iter()
        .filter(|i| i.origin == Origin::Snapshot)
        .collect();
    items.sort_by_key(|i| i.path());
    let paths: Vec<String> = items.iter().map(|i| i.path()).collect();
    assert_eq!(
        paths,
        [
            "Snapshots/com.apple.TimeMachine.2024-05-02-120000.local/Documents/gone.bin",
            "Snapshots/com.apple.TimeMachine.2024-05-02-120000.local/Documents/later.bin",
        ],
        "{:#?}",
        result.items
    );
    assert!(items.iter().all(|i| i.health == Health::Good));
    // Nothing else (old directory nodes, the deep scan) lists them again.
    let starts: Vec<u64> = result.items.iter().map(|i| i.extents[0].offset).collect();
    assert_eq!(
        starts
            .iter()
            .filter(|&&s| s == items[0].extents[0].offset)
            .count(),
        1,
        "{:#?}",
        result.items
    );
    let mut out = Vec::new();
    session::write_item(&img, items[0], &mut out, |_| {}).unwrap();
    assert_eq!(out, gone);
}

/// Fletcher-64, so corrupted objects still pass the checksum and reach
/// the parsers.
fn seal(obj: &mut [u8]) {
    const M: u64 = 0xFFFF_FFFF;
    let (mut s1, mut s2) = (0u64, 0u64);
    for w in obj[8..].chunks_exact(4) {
        s1 = (s1 + u64::from(u32::from_le_bytes(w.try_into().unwrap()))) % M;
        s2 = (s2 + s1) % M;
    }
    let c1 = M - (s1 + s2) % M;
    let c2 = M - (s1 + c1) % M;
    obj[..8].copy_from_slice(&((c2 << 32) | c1).to_le_bytes());
}

fn checksum_ok(obj: &[u8]) -> bool {
    let mut copy = obj.to_vec();
    seal(&mut copy);
    copy[..8] == obj[..8] && obj.iter().any(|&b| b != 0)
}

/// Corrupt snapshot trees, superblocks and object maps: nothing panics or
/// hangs, and every extent found is inside the container.
#[test]
fn corrupt_apfs_snapshots_do_not_panic() {
    let dir = scratch("apfs-snap-fuzz");
    let Some((img, _, _)) = demo(&dir) else {
        std::fs::remove_dir_all(&dir).ok();
        return;
    };
    std::fs::remove_dir_all(&dir).ok();
    let objects: Vec<usize> = (0..img.len() / 4096)
        .filter(|&b| checksum_ok(&img[b * 4096..(b + 1) * 4096]))
        .collect();
    assert!(objects.len() > 10);
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let len = img.len() as u64;
    for round in 0..400 {
        let mut d = img.clone();
        let b = objects[(next() % objects.len() as u64) as usize];
        let obj = &mut d[b * 4096..(b + 1) * 4096];
        for _ in 0..1 + round % 6 {
            let at = 32 + (next() % (4096 - 40)) as usize;
            let v = next();
            match next() % 3 {
                0 => obj[at] = v as u8,
                1 => obj[at..at + 8].copy_from_slice(&v.to_le_bytes()),
                _ => obj[at..at + 8].copy_from_slice(&(v % 2048).to_le_bytes()),
            }
        }
        seal(obj);
        if let Some(vol) = fs::open(&d, 0, len) {
            for s in &vol.snapshots {
                assert!(s.file.extents.iter().all(|e| e.end() <= len));
            }
        }
    }
}

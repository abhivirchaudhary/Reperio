//! End-to-end checks against card images built by
//! `scripts/make_test_cards.py`. Run with:
//!
//!     REPERIO_TEST_CARDS=/path/to/cards cargo test --test cards -- --nocapture
//!
//! Skipped when the variable is unset.

use std::path::PathBuf;

use reperio_engine::extent::MappedSource;
use reperio_engine::fs::{self, partition};
use reperio_engine::source::{DeviceSource, Source};
use sha2::{Digest, Sha256};

fn cards() -> Option<PathBuf> {
    std::env::var_os("REPERIO_TEST_CARDS").map(PathBuf::from)
}

fn sha256(src: &dyn Source) -> String {
    let mut buf = vec![0u8; src.len() as usize];
    src.read_at(0, &mut buf).unwrap();
    format!("{:x}", Sha256::digest(&buf))
}

#[test]
fn filesystem_recovery_matches_originals() {
    let Some(dir) = cards() else { return };
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let mut failures = Vec::new();
    for (image, expected) in manifest.as_object().unwrap() {
        let src = DeviceSource::open(&dir.join(image)).unwrap();
        // Mac-formatted cards also have an empty EFI partition.
        let parts = partition::partitions(&src);
        let vol = parts
            .iter()
            .find_map(|p| fs::open(&src, p.offset, p.len))
            .unwrap_or_else(|| panic!("{image}: no volume in {parts:?}"));
        println!(
            "{image}: {} label={:?} cluster={} files={}",
            vol.fs.name(),
            vol.label,
            vol.cluster_size,
            vol.files.len()
        );
        for f in &vol.files {
            let got = sha256(&MappedSource::new(&src, f.extents.clone()));
            println!(
                "  {:<8} {:<40} {:>8} {:?} extents={} alt={} overwritten={} {}",
                if f.deleted { "deleted" } else { "live" },
                fs::join_path(&f.dir, &f.name),
                f.size,
                f.modified.map(|d| d.to_string()),
                f.extents.len(),
                f.alternatives.len(),
                f.overwritten,
                &got[..12]
            );
        }
        for f in vol
            .files
            .iter()
            .filter(|f| f.extents.len() > 1 || !f.alternatives.is_empty())
        {
            println!(
                "  layout {}: {:?} alt {:?}",
                f.name, f.extents, f.alternatives
            );
        }
        for e in expected.as_array().unwrap() {
            // APFS names its deleted files from old directory nodes the deep
            // scan finds; the session test below covers those.
            if vol.fs.name() == "APFS" && e["deleted"].as_bool().unwrap() {
                continue;
            }
            let (name, dir, sha) = (
                e["name"].as_str().unwrap(),
                e["dir"].as_str().unwrap(),
                e["sha256"].as_str().unwrap(),
            );
            let found = vol.files.iter().find(|f| {
                f.name == name && f.dir == dir && f.deleted == e["deleted"].as_bool().unwrap()
            });
            match found {
                None => failures.push(format!("{image}: {dir}/{name} not listed")),
                Some(f) => {
                    let layouts = std::iter::once(&f.extents).chain(f.alternatives.iter());
                    if !layouts
                        .into_iter()
                        .any(|x| sha256(&MappedSource::new(&src, x.clone())) == sha)
                    {
                        failures.push(format!("{image}: {dir}/{name} content differs"))
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn session_finds_and_previews_everything() {
    use reperio_engine::preview;
    use reperio_engine::session::{self, Health, Origin, ScanOptions};
    use std::sync::atomic::AtomicBool;

    let Some(dir) = cards() else { return };
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    for image in manifest.as_object().unwrap().keys().map(String::as_str) {
        let src = DeviceSource::open(&dir.join(image)).unwrap();
        let result = session::scan(
            &src,
            image,
            &ScanOptions::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        // Every file deleted in the test must be found and intact. Others
        // may be listed too (NTFS keeps records FAT reuses).
        for e in manifest[image]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["deleted"].as_bool().unwrap())
        {
            let (dir_name, name) = (e["dir"].as_str().unwrap(), e["name"].as_str().unwrap());
            let item = result
                .items
                .iter()
                .find(|i| i.origin == Origin::Deleted && i.dir == dir_name && i.name == name)
                .unwrap_or_else(|| panic!("{image}: {dir_name}/{name} not found"));
            assert_eq!(item.health, Health::Good, "{image}: {name}");
        }
        // Files whose space was reused have nothing left to show.
        for item in result
            .items
            .iter()
            .filter(|i| preview::supported(i) && i.health != Health::Overwritten)
        {
            let thumb = preview::thumbnail(&src, item, 256)
                .unwrap_or_else(|| panic!("{image}: {}", item.name));
            let img = image::load_from_memory(&thumb).unwrap();
            assert!(img.width().max(img.height()) == 256, "{}", item.name);
        }
        // Round-trips through a saved session.
        let path = std::env::temp_dir().join(format!("{image}.json"));
        result.save(&path).unwrap();
        let back = session::ScanResult::load(&path).unwrap();
        assert_eq!(back.items.len(), result.items.len());
        assert_eq!(back.items[0].taken, result.items[0].taken);
    }
}

/// Disks whose partitions are lost, built by `scripts/make_lost_cards.py`:
/// every volume must be found where it was, and every file on it (deleted
/// ones by name) must come back byte for byte. Run with:
///
///     REPERIO_LOST_CARDS=/path/to/lost-cards cargo test --test cards -- --nocapture
#[test]
fn lost_partitions_are_found_and_read() {
    use reperio_engine::session::{self, Origin, ScanOptions};
    use std::sync::atomic::AtomicBool;

    let Some(dir) = std::env::var_os("REPERIO_LOST_CARDS").map(PathBuf::from) else {
        return;
    };
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for (image, disk) in manifest.as_object().unwrap() {
        let src = DeviceSource::open(&dir.join(image)).unwrap();
        let opts = ScanOptions {
            deep: false,
            find_partitions: disk["find_partitions"].as_bool().unwrap(),
            ..ScanOptions::default()
        };
        let result = session::scan(&src, image, &opts, &AtomicBool::new(false), |_| {});
        for v in &result.volumes {
            println!(
                "{image}: {} at {} ({} bytes) readable={} files={} deleted={} {}",
                v.fs, v.offset, v.len, v.readable, v.files, v.deleted, v.found
            );
        }
        for want in disk["volumes"].as_array().unwrap() {
            let offset = want["offset"].as_u64().unwrap();
            let fs_name = want["fs"].as_str().unwrap();
            let Some(index) = result
                .volumes
                .iter()
                .position(|v| v.offset == offset && v.readable && v.fs == fs_name)
            else {
                failures.push(format!("{image}: {fs_name} at {offset} not found"));
                continue;
            };
            for e in want["files"].as_array().unwrap() {
                let (name, dir, sha) = (
                    e["name"].as_str().unwrap(),
                    e["dir"].as_str().unwrap(),
                    e["sha256"].as_str().unwrap(),
                );
                let origin = if e["deleted"].as_bool().unwrap() {
                    Origin::Deleted
                } else {
                    Origin::Existing
                };
                checked += 1;
                let item = result.items.iter().find(|i| {
                    i.volume == Some(index as u32)
                        && i.name == name
                        && i.dir == dir
                        && i.origin == origin
                });
                match item {
                    None => {
                        failures.push(format!("{image} {fs_name}@{offset}: {dir}/{name} missing"))
                    }
                    Some(i) if sha256(&MappedSource::new(&src, i.extents.clone())) != sha => {
                        failures.push(format!("{image} {fs_name}@{offset}: {dir}/{name} differs"))
                    }
                    Some(_) => {}
                }
            }
        }
    }
    println!("{checked} files checked");
    assert!(failures.is_empty(), "{failures:#?}");
}

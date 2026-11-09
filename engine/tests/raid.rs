//! RAID sets built by `scripts/make_raid_sets.py`: each set's layout must
//! be detected (from md superblocks where the set has them, from the data
//! where not), the combined drive must equal the original volume byte for
//! byte, and every file, deleted ones included, must come back intact.
//!
//!     scripts/make_raid_sets.py /tmp/raid
//!     REPERIO_TEST_RAID=/tmp/raid cargo test --release -p reperio-engine --test raid -- --nocapture
//!
//! Skipped when the variable is unset. `REPERIO_RAID_ONLY=substring`
//! runs only the sets whose name contains it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use reperio_engine::extent::MappedSource;
use reperio_engine::fs::{self, partition};
use reperio_engine::raid::detect::{detect, Hints};
use reperio_engine::raid::{Level, Parity};
use reperio_engine::session::{self, Origin, ScanOptions};
use reperio_engine::source::{DeviceSource, Source};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn dir() -> Option<PathBuf> {
    std::env::var_os("REPERIO_TEST_RAID").map(PathBuf::from)
}

fn sha(src: &dyn Source) -> String {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0;
    while at < src.len() {
        let n = src.read_at(at, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        at += n as u64;
    }
    format!("{:x}", h.finalize())
}

/// sha256 of each recoverable file, keyed by (dir, name, deleted).
type Hashes = HashMap<(String, String, bool), String>;

fn files(src: &dyn Source) -> HashMap<(String, String, bool), String> {
    let mut out = HashMap::new();
    for p in partition::partitions(src) {
        if let Some(vol) = fs::open(src, p.offset, p.len) {
            for f in &vol.files {
                let got = sha(&MappedSource::new(src, f.extents.clone()));
                out.insert((f.dir.clone(), f.name.clone(), f.deleted), got);
            }
        }
    }
    out
}

fn expected_level(set: &Value) -> Level {
    match set["level"].as_str().unwrap() {
        "raid0" => Level::Raid0,
        "raid1" => Level::Raid1,
        "jbod" => Level::Jbod,
        "raid10" => Level::Raid10 {
            near: 2,
            far: 1,
            offset: false,
        },
        "raid5" => Level::Raid5 {
            parity: Parity::parse(set["parity"].as_str().unwrap()).unwrap(),
        },
        l => panic!("unknown level {l}"),
    }
}

#[test]
fn raid_sets_detected_and_recovered() {
    let Some(dir) = dir() else { return };
    let only = std::env::var("REPERIO_RAID_ONLY").unwrap_or_default();
    let sets: serde_json::Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("sets.json")).unwrap()).unwrap();
    let mut volumes: HashMap<String, (Vec<u8>, Hashes)> = HashMap::new();
    let mut failures = Vec::new();
    let mut detected = 0;
    let mut recovered = (0, 0);
    for (name, set) in &sets {
        if !name.contains(&only) {
            continue;
        }
        let volume = set["volume"].as_str().unwrap().to_string();
        let (vol, original) = volumes.entry(volume.clone()).or_insert_with(|| {
            let data = std::fs::read(dir.join(&volume)).unwrap();
            let files = files(&data);
            (data, files)
        });
        let manifest: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.join(set["manifest"].as_str().unwrap())).unwrap(),
        )
        .unwrap();

        let names: Vec<String> = set["drives"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect();
        let drives: Vec<Arc<dyn Source>> = names
            .iter()
            .map(|n| Arc::new(DeviceSource::open(&dir.join(name).join(n)).unwrap()) as _)
            .collect();
        let metadata = !set["metadata"].is_null();
        let hints = Hints {
            missing: if metadata {
                0
            } else {
                set["missing"].as_u64().unwrap() as usize
            },
            ..Hints::default()
        };
        let began = Instant::now();
        let found = detect(&drives, &hints, &AtomicBool::new(false));
        let took = began.elapsed().as_secs_f64();
        let Some(top) = found.candidates.first() else {
            failures.push(format!("{name}: nothing detected ({:?})", found.notes));
            continue;
        };
        println!(
            "{name}: {} [{}] confidence {:.2}, {:.1}s; runner-up: {}",
            top.description,
            top.found.join("; "),
            top.confidence,
            took,
            found
                .candidates
                .get(1)
                .map(|c| format!("{} ({:.2})", c.description, c.confidence))
                .unwrap_or_else(|| "none".into())
        );

        // The layout.
        let level = expected_level(set);
        let mut wrong = Vec::new();
        if top.layout.level != level {
            wrong.push(format!("level {:?}, expected {level:?}", top.layout.level));
        }
        if level.striped() && top.layout.chunk != set["chunk"].as_u64().unwrap() {
            wrong.push(format!("chunk {}", top.layout.chunk));
        }
        if top.from_metadata != metadata {
            wrong.push("metadata use".into());
        }
        let order: Vec<Option<String>> = top
            .layout
            .order
            .iter()
            .map(|o| o.map(|i| names[i].clone()))
            .collect();
        let want: Vec<Option<String>> = set["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_str().map(String::from))
            .collect();
        // Mirrors read the same in any order, RAID 10 with partners swapped.
        let order_matters = !matches!(level, Level::Raid1 | Level::Raid10 { .. });
        if order_matters && order != want {
            wrong.push(format!("order {order:?}, expected {want:?}"));
        }
        let offsets: Vec<u64> = set["offsets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_u64().unwrap())
            .collect();
        for (pos, o) in want.iter().enumerate() {
            if o.is_some() && order_matters && top.layout.offsets[pos] != offsets[pos] {
                wrong.push(format!("offset of position {pos}"));
            }
        }
        if !metadata && top.confidence < 0.5 {
            wrong.push(format!("low confidence {:.2}", top.confidence));
        }
        if !wrong.is_empty() {
            failures.push(format!("{name}: {}", wrong.join(", ")));
            continue;
        }
        detected += 1;

        // The combined drive is the volume, byte for byte.
        let src = top.layout.build(&drives).unwrap();
        assert!(src.len() >= vol.len() as u64, "{name}: too short");
        let mut got = vec![0u8; vol.len()];
        src.read_at(0, &mut got).unwrap();
        if got != *vol {
            let at = got
                .iter()
                .zip(vol.iter())
                .position(|(a, b)| a != b)
                .unwrap();
            failures.push(format!("{name}: differs from the volume at byte {at}"));
            continue;
        }

        // Every file the volume's own reader recovers from the original
        // comes back the same through the array, and matches what was
        // written.
        let through = files(&src);
        for e in &manifest {
            let key = (
                e["dir"].as_str().unwrap().to_string(),
                e["name"].as_str().unwrap().to_string(),
                e["deleted"].as_bool().unwrap(),
            );
            let want = e["sha256"].as_str().unwrap();
            let orig_ok = original.get(&key).is_some_and(|s| s == want);
            let raid_ok = through.get(&key).is_some_and(|s| s == want);
            if !key.2 && !raid_ok {
                failures.push(format!("{name}: {}/{} not recovered", key.0, key.1));
            }
            if orig_ok != raid_ok {
                failures.push(format!(
                    "{name}: {}/{} recovered differently through the array",
                    key.0, key.1
                ));
            }
            if raid_ok {
                recovered.0 += 1;
                if key.2 {
                    recovered.1 += 1;
                }
            }
        }
    }
    println!(
        "{detected} sets detected right; {} files recovered intact, {} of them deleted",
        recovered.0, recovered.1
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A whole scan runs on a combined drive, finding deleted files by name.
#[test]
fn scan_runs_on_a_degraded_array() {
    let Some(dir) = dir() else { return };
    let set = dir.join("ntfs-raid5-4x64k-la-missing");
    if !set.exists() {
        return;
    }
    let mut names: Vec<_> = std::fs::read_dir(&set)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    let drives: Vec<Arc<dyn Source>> = names
        .iter()
        .map(|p| Arc::new(DeviceSource::open(p).unwrap()) as _)
        .collect();
    let hints = Hints {
        missing: 1,
        ..Hints::default()
    };
    let found = detect(&drives, &hints, &AtomicBool::new(false));
    let src = found.candidates[0].layout.build(&drives).unwrap();
    let opts = ScanOptions {
        deep: true,
        existing: false,
        formats: Vec::new(),
        resume: None,
        find_partitions: false,
        plugins: None,
        case_id: None,
        evidence_id: None,
        nsrl_db: None,
    };
    let result = session::scan(&src, "array", &opts, &AtomicBool::new(false), |_| {});
    let deleted: Vec<String> = result
        .items
        .iter()
        .filter(|i| i.origin == Origin::Deleted)
        .map(|i| i.path())
        .collect();
    println!("deleted files found by name: {deleted:?}");
    assert!(deleted.iter().any(|p| p.ends_with("IMG_0012.JPG")));
}

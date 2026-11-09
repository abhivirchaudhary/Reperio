//! Deep-scan checks against real files made by `scripts/make_format_samples.py`:
//! every sample, laid out on a simulated card between leftover data, must
//! be found at its exact size with the right type.
//!
//!     REPERIO_TEST_SAMPLES=/path/to/samples cargo test -p reperio-engine --test samples
//!
//! Skipped when the variable is unset.

use reperio_engine::Scanner;

#[test]
fn every_sample_is_carved_exactly() {
    let Some(dir) = std::env::var_os("REPERIO_TEST_SAMPLES") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();

    // Leftover data between files: zeros and pseudo-random bytes.
    let mut x = 0x2545F491u32;
    let mut junk = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    };
    let mut card = junk(10_000);
    let mut expected = Vec::new();
    for (i, path) in files.iter().enumerate() {
        let at = card.len().div_ceil(4096) * 4096 + if i % 2 == 0 { 0 } else { 8192 };
        card.resize(at, 0);
        let data = std::fs::read(path).unwrap();
        expected.push((
            at as u64,
            data.len() as u64,
            path.extension().unwrap().to_string_lossy().to_lowercase(),
        ));
        card.extend(&data);
        let gap = junk(3000 + i * 517);
        card.extend(gap);
    }
    card.resize(card.len().div_ceil(4096) * 4096 + 65536, 0);

    let mut hits = Vec::new();
    Scanner::default().scan(
        &card,
        |h| {
            hits.push((h.offset, h.len, h.ext.to_string()));
            true
        },
        |_| true,
    );

    let mut problems = Vec::new();
    for (path, want) in files.iter().zip(&expected) {
        match hits.iter().find(|h| h.0 == want.0) {
            None => problems.push(format!("{}: not found", path.display())),
            Some(got) if got.1 != want.1 || got.2 != want.2 => problems.push(format!(
                "{}: got {} bytes as .{}, want {} bytes as .{}",
                path.display(),
                got.1,
                got.2,
                want.1,
                want.2
            )),
            Some(_) => {}
        }
    }
    let extra: Vec<_> = hits
        .iter()
        .filter(|h| !expected.iter().any(|e| e.0 == h.0))
        .collect();
    assert!(
        problems.is_empty() && extra.is_empty(),
        "{problems:#?}\nunexpected finds: {extra:?}"
    );
    println!("{} samples carved exactly", files.len());
}

/// Damaged copies of every sample, cut short or with flipped bits (mostly
/// in the first few kilobytes, where the headers are), must scan without
/// a panic and give only extents inside the data.
#[test]
fn damaged_samples_never_panic() {
    let Some(dir) = std::env::var_os("REPERIO_TEST_SAMPLES") else {
        return;
    };
    let mut x = 0x1234_5678u32;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x as usize
    };
    for entry in std::fs::read_dir(&dir).unwrap() {
        let good = std::fs::read(entry.unwrap().path()).unwrap();
        let len = good.len();
        let mut variants = Vec::new();
        for i in 1..8 {
            variants.push(good[..len * i / 8].to_vec());
        }
        for i in 0..24 {
            let mut bad = good.clone();
            for _ in 0..1 + rand() % 8 {
                let span = if i % 3 == 0 { len } else { len.min(4096) };
                bad[rand() % span] ^= 1 << (rand() % 8);
            }
            variants.push(bad);
        }
        for data in variants {
            Scanner::default().scan(
                &data,
                |h| {
                    assert!(h.len > 0 && h.offset + h.len <= data.len() as u64);
                    true
                },
                |_| true,
            );
        }
    }
}

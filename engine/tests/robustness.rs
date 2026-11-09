//! Mutation robustness tests. Reperio reads untrusted data from damaged
//! cards, so no corruption may make it panic, hang or allocate wildly.
//!
//! Every case corrupts an image or a file deterministically from a seed
//! (bit flips, zeroed, random, swapped and duplicated sectors, truncation,
//! and boundary values written into the metadata the parsers read), then
//! runs the engine on it in its own thread with a time budget, under an
//! allocator that records the largest single allocation. Failures are
//! collected rather than stopping the run, and each is reported with its
//! seed so it can be replayed alone.
//!
//! Always runs, on images and files built here:
//!
//!     cargo test -p reperio-engine --test robustness
//!
//! Also runs on the generated cards and format samples when pointed at
//! them. Release with overflow checks finds arithmetic bugs quickly:
//!
//!     REPERIO_TEST_CARDS=cards REPERIO_TEST_SAMPLES=samples \
//!     REPERIO_ROBUST_CASES=2000 CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true \
//!     cargo test --release -p reperio-engine --test robustness -- --nocapture
//!
//! `REPERIO_ROBUST_CASES` is the number of cases per image or file;
//! `REPERIO_ROBUST_REPLAY=name:seed[,name:seed...]` runs only the
//! reported cases given.

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex, Once};
use std::time::{Duration, Instant};

use reperio_engine::extent::{Extent, MappedSource};
use reperio_engine::formats::{self, HEAD_LEN};
use reperio_engine::fs::{self, partition};
use reperio_engine::reader::Reader;
use reperio_engine::session::{self, Health, Item, Origin, ScanOptions};
use reperio_engine::source::Source;
use reperio_engine::{preview, repair, videofix, Kind, Scanner};

// ---------------------------------------------------------------------------
// Allocation guard

/// Records each thread's largest single allocation and its peak of live
/// bytes. Requests too big to be real are served from a lazily backed
/// mapping (on Linux), so a parser asking for terabytes is reported as a
/// failing case instead of aborting the whole run.
mod guard {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    const LAZY_ABOVE: usize = 1 << 30;

    thread_local! {
        static BIGGEST: Cell<usize> = const { Cell::new(0) };
        static LIVE: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    pub struct Guard;

    fn note_alloc(size: usize) {
        let _ = BIGGEST.try_with(|b| b.set(b.get().max(size)));
        let _ = LIVE.try_with(|l| {
            let v = l.get() + size as isize;
            l.set(v);
            let _ = PEAK.try_with(|p| p.set(p.get().max(v)));
        });
    }

    fn note_free(size: usize) {
        let _ = LIVE.try_with(|l| l.set(l.get() - size as isize));
    }

    fn lazy(l: &Layout) -> bool {
        cfg!(target_os = "linux") && l.size() > LAZY_ABOVE && l.align() <= 4096
    }

    #[cfg(target_os = "linux")]
    mod map {
        extern "C" {
            fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
                -> *mut u8;
            fn munmap(addr: *mut u8, len: usize) -> i32;
        }
        const PROT_READ_WRITE: i32 = 0x3;
        const MAP_PRIVATE_ANON_NORESERVE: i32 = 0x02 | 0x20 | 0x4000;

        pub unsafe fn alloc(size: usize) -> *mut u8 {
            let p = mmap(
                std::ptr::null_mut(),
                size,
                PROT_READ_WRITE,
                MAP_PRIVATE_ANON_NORESERVE,
                -1,
                0,
            );
            if p as isize == -1 {
                std::ptr::null_mut()
            } else {
                p
            }
        }

        pub unsafe fn free(p: *mut u8, size: usize) {
            munmap(p, size);
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod map {
        pub unsafe fn alloc(_: usize) -> *mut u8 {
            unreachable!()
        }
        pub unsafe fn free(_: *mut u8, _: usize) {
            unreachable!()
        }
    }

    unsafe impl GlobalAlloc for Guard {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            note_alloc(l.size());
            if lazy(&l) {
                map::alloc(l.size())
            } else {
                System.alloc(l)
            }
        }

        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            note_alloc(l.size());
            if lazy(&l) {
                map::alloc(l.size()) // mappings start zeroed
            } else {
                System.alloc_zeroed(l)
            }
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            note_free(l.size());
            if lazy(&l) {
                map::free(p, l.size())
            } else {
                System.dealloc(p, l)
            }
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let nl = Layout::from_size_align_unchecked(new, l.align());
            if !lazy(&l) && !lazy(&nl) {
                note_free(l.size());
                note_alloc(new);
                return System.realloc(p, l, new);
            }
            let q = self.alloc(nl);
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                self.dealloc(p, l);
            }
            q
        }
    }

    pub fn reset() {
        BIGGEST.with(|b| b.set(0));
        LIVE.with(|l| l.set(0));
        PEAK.with(|p| p.set(0));
    }

    /// (largest single allocation, peak live bytes) on this thread.
    pub fn stats() -> (usize, usize) {
        (
            BIGGEST.with(Cell::get),
            PEAK.with(Cell::get).max(0) as usize,
        )
    }
}

#[global_allocator]
static GUARD: guard::Guard = guard::Guard;

// ---------------------------------------------------------------------------
// Deterministic randomness

/// xorshift64*: tiny, fast and plenty for choosing corruptions.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Mix the seed so nearby seeds diverge immediately.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Self((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn hash_name(name: &str) -> u64 {
    name.bytes().fold(0xCBF2_9CE4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01B3)
    })
}

// ---------------------------------------------------------------------------
// Mutations

/// Places the parsers read from the clean image: corruption there reaches
/// the code under test, where corruption elsewhere mostly hits file data.
#[derive(Clone, Default)]
struct Hot(Vec<(u64, u64)>);

impl Hot {
    /// Records the reads made by `f` on `data`.
    fn record(data: &[u8], f: impl FnOnce(&dyn Source)) -> Self {
        struct Recorder<'a> {
            data: &'a [u8],
            reads: Mutex<Vec<(u64, u64)>>,
        }
        impl Source for Recorder<'_> {
            fn len(&self) -> u64 {
                self.data.len() as u64
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads.lock().unwrap().push((offset, buf.len() as u64));
                if offset >= self.data.len() as u64 {
                    return Ok(0);
                }
                let n = buf.len().min(self.data.len() - offset as usize);
                buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
                Ok(n)
            }
        }
        let rec = Recorder {
            data,
            reads: Mutex::new(Vec::new()),
        };
        f(&rec);
        let mut reads = rec.reads.into_inner().unwrap();
        reads.retain(|&(o, n)| n > 0 && o < data.len() as u64);
        reads.sort_unstable();
        reads.dedup();
        // The start of the image always matters: partition tables and
        // superfloppy boot sectors.
        reads.push((0, 64 << 10));
        Hot(reads)
    }

    /// An offset inside a hot range, near its start more often than not.
    fn pick(&self, rng: &mut Rng, len: usize) -> usize {
        if self.0.is_empty() || len == 0 {
            return rng.below(len as u64) as usize;
        }
        let (o, n) = self.0[rng.below(self.0.len() as u64) as usize];
        let span = if rng.chance(70) { n.min(4096) } else { n };
        (o + rng.below(span)) as usize % len
    }
}

/// Values that sit on the boundaries parsers get wrong.
const INTERESTING: &[u64] = &[
    0,
    1,
    2,
    7,
    8,
    0x10,
    0x7F,
    0x80,
    0xFF,
    0x100,
    0x1000,
    0x7FFF,
    0x8000,
    0xFFFF,
    0x1_0000,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFE,
    0xFFFF_FFFF,
    0x1_0000_0000,
    0x7FFF_FFFF_FFFF_FFFF,
    0x8000_0000_0000_0000,
    0xFFFF_FFFF_FFFF_FFFE,
    u64::MAX,
];

const SECTOR: usize = 512;

/// Applies one to four random corruptions and describes them.
fn mutate(rng: &mut Rng, data: &mut Vec<u8>, hot: &Hot) -> Vec<String> {
    let mut log = Vec::new();
    for _ in 0..1 + rng.below(4) {
        if data.is_empty() {
            break;
        }
        let len = data.len();
        let sector_at = |rng: &mut Rng, hot_first: bool| {
            let at = if hot_first && rng.chance(80) {
                hot.pick(rng, len)
            } else {
                rng.below(len as u64) as usize
            };
            at / SECTOR * SECTOR
        };
        match rng.below(11) {
            0 => {
                let k = 1 + rng.below(64);
                for _ in 0..k {
                    let at = rng.below(len as u64) as usize;
                    data[at] ^= 1 << rng.below(8);
                }
                log.push(format!("flip {k} bits anywhere"));
            }
            1 => {
                let k = 1 + rng.below(16);
                for _ in 0..k {
                    let at = hot.pick(rng, len);
                    data[at] ^= 1 << rng.below(8);
                }
                log.push(format!("flip {k} bits in metadata"));
            }
            2 | 3 => {
                let width = [1usize, 2, 4, 8][rng.below(4) as usize];
                let at = hot.pick(rng, len) / width * width;
                let mut v = INTERESTING[rng.below(INTERESTING.len() as u64) as usize];
                if rng.chance(25) {
                    v = v.wrapping_add(rng.below(3)).wrapping_sub(1);
                }
                let bytes = if rng.chance(50) {
                    v.to_le_bytes()
                } else {
                    (v << (64 - 8 * width as u32).min(63)).to_be_bytes()
                };
                let end = (at + width).min(len);
                let n = end - at;
                data[at..end].copy_from_slice(&bytes[..n]);
                log.push(format!("write {v:#x} ({width} bytes) at {at:#x}"));
            }
            4 => {
                let at = sector_at(rng, true);
                let end = (at + SECTOR).min(len);
                data[at..end].fill(0);
                log.push(format!("zero sector at {at:#x}"));
            }
            5 => {
                let at = sector_at(rng, true);
                let end = (at + SECTOR).min(len);
                let junk = rng.bytes(end - at);
                data[at..end].copy_from_slice(&junk);
                log.push(format!("random sector at {at:#x}"));
            }
            6 => {
                let at = if rng.chance(50) {
                    hot.pick(rng, len)
                } else {
                    rng.below(len as u64) as usize
                };
                data.truncate(at);
                log.push(format!("truncate to {at:#x}"));
            }
            7 => {
                let a = sector_at(rng, true);
                let b = sector_at(rng, false);
                if a != b && a + SECTOR <= len && b + SECTOR <= len {
                    for i in 0..SECTOR {
                        data.swap(a + i, b + i);
                    }
                }
                log.push(format!("swap sectors {a:#x} and {b:#x}"));
            }
            8 => {
                // Duplicated metadata makes loops: a directory listing
                // itself, a B-tree node pointing back at its parent.
                let a = sector_at(rng, true);
                let b = sector_at(rng, true);
                let n = SECTOR * (1 + rng.below(8) as usize);
                if a + n <= len && b + n <= len {
                    data.copy_within(a..a + n, b);
                }
                log.push(format!("copy {n} bytes from {a:#x} to {b:#x}"));
            }
            9 => {
                let at = hot.pick(rng, len);
                let end = (at + 1 + rng.below(64) as usize).min(len);
                let junk = rng.bytes(end - at);
                data[at..end].copy_from_slice(&junk);
                log.push(format!("random bytes at {at:#x}..{end:#x}"));
            }
            _ => {
                let at = hot.pick(rng, len);
                let end = (at + 1 + rng.below(32) as usize).min(len);
                data[at..end].fill(0xFF);
                log.push(format!("0xFF bytes at {at:#x}..{end:#x}"));
            }
        }
    }
    log
}

/// APFS checks every object's checksum, so random corruption of its
/// metadata is mostly turned away at the door. Re-sealing the objects that
/// were valid in the clean image lets corruption reach the parsers.
fn reseal_apfs(data: &mut [u8], sealed: &[usize], block: usize) {
    for &at in sealed {
        if at + block <= data.len() {
            let sum = fletcher64(&data[at + 8..at + block]);
            data[at..at + 8].copy_from_slice(&sum.to_le_bytes());
        }
    }
}

fn fletcher64(words: &[u8]) -> u64 {
    const M: u64 = 0xFFFF_FFFF;
    let (mut s1, mut s2) = (0u64, 0u64);
    for w in words.chunks_exact(4) {
        s1 = (s1 + u64::from(u32::from_le_bytes(w.try_into().unwrap()))) % M;
        s2 = (s2 + s1) % M;
    }
    let c1 = M - (s1 + s2) % M;
    let c2 = M - (s1 + c1) % M;
    (c2 << 32) | c1
}

/// Offsets of 4096-byte blocks whose APFS checksum is valid.
fn apfs_sealed(data: &[u8]) -> Vec<usize> {
    (0..data.len() / 4096)
        .map(|i| i * 4096)
        .filter(|&at| {
            let b = &data[at..at + 4096];
            b[..8] != [0; 8]
                && u64::from_le_bytes(b[..8].try_into().unwrap()) == fletcher64(&b[8..])
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Running cases

static HOOK: Once = Once::new();

thread_local! {
    static IN_CASE: Cell<bool> = const { Cell::new(false) };
    static LAST_PANIC: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Keeps panics inside cases quiet; they are reported with their seed.
fn install_hook() {
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if IN_CASE.with(Cell::get) {
                LAST_PANIC.with(|p| *p.borrow_mut() = info.to_string());
            } else {
                default(info);
            }
        }));
    });
}

#[derive(Clone, Copy)]
struct Budget {
    time: Duration,
    /// Largest single allocation allowed.
    alloc: usize,
}

impl Budget {
    /// Scales with the image: some structures (a FAT, a bitmap) are read
    /// whole, but nothing should need much more than the image itself.
    fn for_len(len: usize) -> Self {
        let slow = cfg!(debug_assertions);
        Budget {
            time: Duration::from_secs(if slow { 60 } else { 15 }),
            alloc: (64 << 20).max(len * 2),
        }
    }
}

struct CaseStats {
    elapsed: Duration,
    biggest: usize,
}

/// Runs `f` on its own thread under the budget. Returns what went wrong.
fn run_case(budget: Budget, f: impl FnOnce() + Send + 'static) -> Result<CaseStats, String> {
    install_hook();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("robustness case".into())
        .spawn(move || {
            IN_CASE.with(|c| c.set(true));
            guard::reset();
            let began = Instant::now();
            let ok = catch_unwind(AssertUnwindSafe(f)).is_ok();
            let elapsed = began.elapsed();
            let (biggest, _) = guard::stats();
            let panic = (!ok).then(|| LAST_PANIC.with(|p| p.take()));
            let _ = tx.send((panic, elapsed, biggest));
        })
        .expect("spawn case thread");
    match rx.recv_timeout(budget.time) {
        Ok((Some(panic), ..)) => Err(format!("panicked: {}", panic.replace('\n', " "))),
        Ok((None, elapsed, biggest)) if biggest > budget.alloc => Err(format!(
            "allocated {} MiB at once (limit {} MiB) in {elapsed:?}",
            biggest >> 20,
            budget.alloc >> 20
        )),
        Ok((None, elapsed, biggest)) => Ok(CaseStats { elapsed, biggest }),
        // The thread cannot be stopped; it is left to finish on its own.
        Err(_) => Err(format!("still running after {:?}", budget.time)),
    }
}

/// Totals for one test, printed at the end.
#[derive(Default)]
struct Report {
    cases: usize,
    failures: Vec<String>,
    slowest: (Duration, String),
    biggest: (usize, String),
    began: Option<Instant>,
}

impl Report {
    fn new() -> Self {
        Report {
            began: Some(Instant::now()),
            ..Default::default()
        }
    }

    fn add(&mut self, label: String, result: Result<CaseStats, String>) {
        self.cases += 1;
        match result {
            Ok(s) => {
                if s.elapsed > self.slowest.0 {
                    self.slowest = (s.elapsed, label.clone());
                }
                if s.biggest > self.biggest.0 {
                    self.biggest = (s.biggest, label);
                }
            }
            Err(e) => self.failures.push(format!("{label}: {e}")),
        }
    }

    fn finish(self, what: &str) {
        println!(
            "{what}: {} cases in {:.1?}; slowest {:.2?} ({}); biggest allocation {} KiB ({})",
            self.cases,
            self.began.map(|b| b.elapsed()).unwrap_or_default(),
            self.slowest.0,
            self.slowest.1,
            self.biggest.0 >> 10,
            self.biggest.1
        );
        if !self.failures.is_empty() {
            let shown: Vec<&String> = self.failures.iter().take(60).collect();
            panic!(
                "{} of {} cases failed{}:\n{}",
                self.failures.len(),
                self.cases,
                if self.failures.len() > shown.len() {
                    " (first 60 shown)"
                } else {
                    ""
                },
                shown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

fn cases_per_target(default: usize) -> usize {
    std::env::var("REPERIO_ROBUST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The cases to replay, from `name:seed[,name:seed...]`.
fn replay() -> Option<Vec<(String, u64)>> {
    let v = std::env::var("REPERIO_ROBUST_REPLAY").ok()?;
    v.split(',')
        .map(|case| {
            let (name, seed) = case.trim().rsplit_once(':')?;
            let seed = seed.trim_start_matches("0x");
            Some((name.to_string(), u64::from_str_radix(seed, 16).ok()?))
        })
        .collect()
}

/// What a case does with its corrupted bytes.
#[derive(Clone, Copy)]
enum Target {
    /// A whole card or disk image.
    Image { deep: bool },
    /// A single file of one format.
    Sample,
}

/// One clean image or file and what is known about it.
struct Subject {
    name: String,
    data: Arc<Vec<u8>>,
    hot: Hot,
    /// APFS objects to re-seal after corruption.
    sealed: Vec<usize>,
}

impl Subject {
    fn image(name: &str, data: Vec<u8>) -> Self {
        let hot = Hot::record(&data, |src| {
            for p in partition::partitions(src) {
                let _ = fs::open(src, p.offset, p.len);
            }
        });
        let sealed = apfs_sealed(&data);
        Subject {
            name: name.to_string(),
            data: Arc::new(data),
            hot,
            sealed,
        }
    }

    fn sample(name: &str, data: Vec<u8>) -> Self {
        // The header and the structure right after it matter most, but a
        // file's index can be anywhere (MP4 `moov`, ZIP directory).
        let len = data.len() as u64;
        let mut hot = vec![(0, len.min(64 << 10))];
        hot.push((len.saturating_sub(64 << 10), len.min(64 << 10)));
        Subject {
            name: name.to_string(),
            data: Arc::new(data),
            hot: Hot(hot),
            sealed: Vec::new(),
        }
    }
}

/// Runs `cases` corrupted variants of every subject.
fn run_mutations(what: &str, subjects: &[Subject], target: Target, cases: usize) {
    let replaying = replay();
    let mut report = Report::new();
    let mut jobs: Vec<(&Subject, u64)> = Vec::new();
    for s in subjects {
        match &replaying {
            Some(list) => jobs.extend(
                list.iter()
                    .filter(|(name, _)| *name == s.name)
                    .map(|&(_, seed)| (s, seed)),
            ),
            None => jobs.extend(
                (0..cases as u64).map(|i| (s, hash_name(&s.name) ^ i.wrapping_mul(0x9E37_79B9))),
            ),
        }
    }
    // Cases are independent; run several at once. Results are sorted
    // afterwards so the report does not depend on scheduling.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&(s, seed)) = jobs.get(i) else { break };
                let mut rng = Rng::new(seed);
                let mut data = s.data.as_ref().clone();
                let log = mutate(&mut rng, &mut data, &s.hot);
                if !s.sealed.is_empty() && rng.chance(75) {
                    reseal_apfs(&mut data, &s.sealed, 4096);
                }
                // Deep scans read everything; run them on a share of cases.
                let target = match target {
                    Target::Image { deep } => Target::Image {
                        deep: deep && rng.chance(25),
                    },
                    t => t,
                };
                let label = format!("{}:{seed:#x} [{}]", s.name, log.join("; "));
                let budget = Budget::for_len(s.data.len());
                let result = run_case(budget, move || exercise(&data, target));
                results.lock().unwrap().push((i, label, result));
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.0);
    for (_, label, result) in results {
        report.add(label, result);
    }
    report.finish(what);
}

/// Everything the engine does with a source, as the app and CLI do it.
fn exercise(data: &Vec<u8>, target: Target) {
    match target {
        Target::Image { deep } => exercise_image(data, deep),
        Target::Sample => exercise_sample(data),
    }
}

fn exercise_image(data: &Vec<u8>, deep: bool) {
    let src: &dyn Source = data;
    for p in partition::partitions(src) {
        if let Some(v) = fs::open(src, p.offset, p.len) {
            touch_volume(src, &v);
        }
    }
    if let Some(v) = fs::open(src, 0, src.len()) {
        touch_volume(src, &v);
    }
    let opts = ScanOptions {
        deep,
        existing: true,
        formats: Vec::new(),
        resume: None,
        find_partitions: false,
        plugins: None,
        case_id: None,
        evidence_id: None,
        nsrl_db: None,
    };
    let result = session::scan(src, "case", &opts, &AtomicBool::new(false), |_| {});
    for item in result
        .items
        .iter()
        .filter(|i| preview::supported(i))
        .take(24)
    {
        let _ = preview::thumbnail(src, item, 128);
    }
    // What recovery reads: every item's data, through its extents.
    read_items(src, &result.items);
}

/// Reads a bounded amount of every file a volume lists.
fn touch_volume(src: &dyn Source, v: &fs::Volume) {
    let mut buf = vec![0u8; 4096];
    for f in v.files.iter().take(10_000) {
        for layout in std::iter::once(&f.extents).chain(&f.alternatives).take(4) {
            let m = MappedSource::new(src, layout.clone());
            let _ = m.read_at(0, &mut buf);
            let _ = m.read_at(m.len().saturating_sub(100), &mut buf);
        }
    }
    let free = MappedSource::new(src, v.free.clone());
    let _ = free.read_at(free.len() / 2, &mut buf);
}

fn read_items(src: &dyn Source, items: &[Item]) {
    let mut buf = vec![0u8; 64 << 10];
    let mut budget: u64 = 256 << 20;
    for item in items {
        let view = MappedSource::new(src, item.extents.clone());
        let mut pos = 0;
        while pos < view.len() && budget > 0 {
            let n = view.read_at(pos, &mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            pos += n as u64;
            budget = budget.saturating_sub(n as u64);
        }
    }
}

/// Carves, scans, previews and repairs one file.
fn exercise_sample(data: &Vec<u8>) {
    let src: &dyn Source = data;
    let mut head = vec![0u8; HEAD_LEN];
    let n = data.len().min(HEAD_LEN);
    head[..n].copy_from_slice(&data[..n]);
    for f in formats::all() {
        if !f.matches(&head) {
            continue;
        }
        let limit = src.len().min(f.max_len());
        if let Some(found) = f.carve(&mut Reader::new(src, 0), 0, limit) {
            assert!(
                found.len <= limit,
                "{} carved {} bytes from {limit}",
                f.name(),
                found.len
            );
        }
    }

    // On a simulated card, between leftover data, at an odd sector.
    let mut card = Rng::new(data.len() as u64).bytes(6000);
    card.resize(8192 + 512, 0);
    let at = card.len() as u64;
    card.extend_from_slice(data);
    card.extend(Rng::new(7).bytes(5000));
    card.resize(card.len().div_ceil(4096) * 4096 + 8192, 0);
    let card_src: &dyn Source = &card;
    let mut hits = Vec::new();
    let outcome = Scanner::default().scan(
        card_src,
        |h| {
            hits.push(h.clone());
            true
        },
        |_| true,
    );
    for h in &hits {
        assert!(
            h.offset + h.len <= card.len() as u64,
            "{} hit at {} runs {} bytes past the end",
            h.format,
            h.offset,
            h.offset + h.len - card.len() as u64
        );
        let item = Item {
            id: 0,
            name: format!("x.{}", h.ext),
            dir: String::new(),
            ext: h.ext.to_string(),
            kind: h.kind,
            origin: Origin::Carved,
            health: Health::Good,
            size: h.len,
            taken: h.taken,
            modified: None,
            extents: vec![Extent::new(h.offset, h.len)],
            volume: None,
            found_by: None,
            fix: None,
            nsrl_status: None,
        };
        if preview::supported(&item) {
            let _ = preview::thumbnail(card_src, &item, 128);
            let _ = preview::preview(card_src, &item, 1024);
        }
    }
    // A thumbnail of the corrupted file itself, as when the filesystem
    // names a damaged file.
    for ext in ["jpg", "png", "gif", "bmp", "tif", "webp", "nef", "cr2"] {
        let item = Item {
            id: 0,
            name: format!("x.{ext}"),
            dir: String::new(),
            ext: ext.into(),
            kind: Kind::Photo,
            origin: Origin::Deleted,
            health: Health::Damaged,
            size: data.len() as u64,
            taken: None,
            modified: None,
            extents: vec![Extent::new(0, data.len() as u64)],
            volume: None,
            found_by: None,
            fix: None,
            nsrl_status: None,
        };
        let _ = preview::thumbnail(src, &item, 96);
    }
    let claimed: Vec<(u64, u64)> = hits.iter().map(|h| (h.offset, h.offset + h.len)).collect();
    for cluster in [512, 4096] {
        let _ = repair::repair_jpeg(card_src, at, &claimed, cluster);
        let _ = repair::repair_mp4(card_src, at, &claimed, &outcome.indexes, cluster);
    }
    // Video repair: learn from the corrupted file as if it were a good
    // one, and rebuild it from a good reference as if it were broken.
    if let Some(r) = videofix::Reference::from_video(src) {
        let _ = videofix::rebuild(src, &r);
    }
    if let Some(r) = videofix::Reference::from_stream(src) {
        let _ = videofix::rebuild(src, &r);
    }
    // Photo and document repair read the same bytes as a damaged file.
    if src.len() <= 8 << 20 {
        if let Ok(data) = reperio_engine::fixfile::read_all(src) {
            let _ = reperio_engine::photofix::repair(&data, &[&data[..data.len().min(4096)]]);
            let _ = reperio_engine::docfix::repair(&data, "docx");
            let _ = reperio_engine::docfix::repair(&data, "pdf");
            let _ = reperio_engine::docfix::repair(&data, "doc");
        }
    }
    if videofix::needs_index(src) {
        let good = camera_mp4(12);
        if let Some(r) = videofix::Reference::from_video(&good) {
            let _ = videofix::rebuild(src, &r);
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic images and files, so the robustness checks always run

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn put16be(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_be_bytes());
}
fn put32be(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64be(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// A TIFF IFD at `at`, with values longer than four bytes stored after it.
fn ifd(entries: &[(u16, u16, u32, Vec<u8>)], at: u32, next: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let mut data = Vec::new();
    let data_at = at + 2 + 12 * entries.len() as u32 + 4;
    v.extend((entries.len() as u16).to_le_bytes());
    for (tag, typ, count, value) in entries {
        v.extend(tag.to_le_bytes());
        v.extend(typ.to_le_bytes());
        v.extend(count.to_le_bytes());
        if value.len() <= 4 {
            let mut field = value.clone();
            field.resize(4, 0);
            v.extend(field);
        } else {
            v.extend((data_at + data.len() as u32).to_le_bytes());
            data.extend(value);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
    }
    v.extend(next.to_le_bytes());
    v.extend(data);
    v
}

fn noise_image(w: u32, h: u32, seed: u64) -> image::RgbImage {
    let mut rng = Rng::new(seed);
    let tint = [rng.next() as u8, rng.next() as u8, rng.next() as u8];
    image::RgbImage::from_fn(w, h, |x, y| {
        let n = (rng.next() % 64) as u8;
        image::Rgb([
            tint[0].wrapping_add(n).wrapping_add(x as u8),
            tint[1].wrapping_add(n),
            tint[2].wrapping_add(y as u8),
        ])
    })
}

fn encode(img: &image::RgbImage, format: image::ImageFormat) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img.clone())
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

/// A camera-style JPEG: EXIF with make, date, orientation and an embedded
/// thumbnail, then the image.
fn camera_jpeg(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let main = encode(&noise_image(w, h, seed), image::ImageFormat::Jpeg);
    let thumb = encode(&noise_image(32, 24, seed + 1), image::ImageFormat::Jpeg);
    let ifd0 = |exif_at: u32, ifd1_at: u32| {
        ifd(
            &[
                (271, 2, 6, b"Canon\0".to_vec()),
                (274, 3, 1, 6u16.to_le_bytes().to_vec()),
                (34665, 4, 1, exif_at.to_le_bytes().to_vec()),
            ],
            8,
            ifd1_at,
        )
    };
    let exif_ifd = |at: u32| ifd(&[(36867, 2, 20, b"2004:07:15 14:03:59\0".to_vec())], at, 0);
    let ifd1 = |at: u32, thumb_at: u32| {
        ifd(
            &[
                (513, 4, 1, thumb_at.to_le_bytes().to_vec()),
                (514, 4, 1, (thumb.len() as u32).to_le_bytes().to_vec()),
            ],
            at,
            0,
        )
    };
    let exif_at = 8 + ifd0(0, 0).len() as u32;
    let ifd1_at = exif_at + exif_ifd(0).len() as u32;
    let thumb_at = ifd1_at + ifd1(0, 0).len() as u32;
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend(ifd0(exif_at, ifd1_at));
    tiff.extend(exif_ifd(exif_at));
    tiff.extend(ifd1(ifd1_at, thumb_at));
    tiff.extend(&thumb);
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend(tiff);
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend(((app1.len() + 2) as u16).to_be_bytes());
    out.extend(app1);
    out.extend(&main[2..]);
    out
}

fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend(typ);
    v.extend(body);
    v
}

/// A camera-style MP4: ftyp, mdat of length-prefixed H.264 frames, then a
/// moov whose sample tables list them, as video repair reads them.
fn camera_mp4(frames: usize) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomavc1");
    let mut media = Vec::new();
    let mut sizes = Vec::new();
    let mut rng = Rng::new(frames as u64);
    for i in 0..frames {
        let n = 300 + (i % 7) * 50;
        let mut frame = ((n - 4) as u32).to_be_bytes().to_vec();
        frame.push(if i == 0 { 0x65 } else { 0x41 });
        frame.extend(rng.bytes(n - 5));
        sizes.push(frame.len() as u32);
        media.extend(frame);
    }
    let mdat_at = ftyp.len() as u32;
    let mdat = bx(b"mdat", &media);
    let full = |version_flags: u32, body: &[u8]| {
        let mut v = version_flags.to_be_bytes().to_vec();
        v.extend(body);
        v
    };
    let mut stsd_entry = vec![0u8; 8];
    stsd_entry.extend(bx(b"avc1", &[0; 78]));
    let stsd = bx(
        b"stsd",
        &full(0, &[&1u32.to_be_bytes()[..], &stsd_entry[8..]].concat()),
    );
    let mut stco_body = (frames as u32).to_be_bytes().to_vec();
    let mut off = mdat_at + 8;
    for s in &sizes {
        stco_body.extend(off.to_be_bytes());
        off += s;
    }
    let stco = bx(b"stco", &full(0, &stco_body));
    let stsc = bx(
        b"stsc",
        &full(
            0,
            &[
                1u32.to_be_bytes(),
                1u32.to_be_bytes(),
                1u32.to_be_bytes(),
                1u32.to_be_bytes(),
            ]
            .concat(),
        ),
    );
    let mut stsz_body = vec![0u8; 4];
    stsz_body.extend((frames as u32).to_be_bytes());
    for s in &sizes {
        stsz_body.extend(s.to_be_bytes());
    }
    let stsz = bx(b"stsz", &full(0, &stsz_body));
    let stbl = bx(b"stbl", &[stsd, stco, stsc, stsz].concat());
    let trak = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
    let mut mvhd = vec![0u8; 4];
    mvhd.extend((1_433_160_000u32.wrapping_add(2_082_844_800)).to_be_bytes());
    mvhd.extend([0; 92]);
    let moov = bx(b"moov", &[bx(b"mvhd", &mvhd), trak].concat());
    [ftyp, mdat, moov].concat()
}

fn raw_tiff(make: &str, strip: usize) -> Vec<u8> {
    let make_z = format!("{make}\0").into_bytes();
    let entries = |strip_at: u32| {
        vec![
            (271u16, 2u16, make_z.len() as u32, make_z.clone()),
            (273, 4, 1, strip_at.to_le_bytes().to_vec()),
            (279, 4, 1, (strip as u32).to_le_bytes().to_vec()),
            (306, 2, 20, b"2011:12:25 09:30:00\0".to_vec()),
            (330, 4, 1, 8u32.to_le_bytes().to_vec()),
        ]
    };
    let strip_at = 8 + ifd(&entries(0), 8, 0).len() as u32;
    let mut v = b"II*\0\x08\0\0\0".to_vec();
    v.extend(ifd(&entries(strip_at), 8, 0));
    v.extend((0..strip).map(|i| (i % 200) as u8));
    v
}

fn zip_file(names: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for name in names {
        let at = out.len() as u32;
        let body = format!("contents of {name}").into_bytes();
        let mut lfh = vec![0u8; 30];
        lfh[..4].copy_from_slice(b"PK\x03\x04");
        put16(&mut lfh, 4, 20);
        put32(&mut lfh, 18, body.len() as u32);
        put32(&mut lfh, 22, body.len() as u32);
        put16(&mut lfh, 26, name.len() as u16);
        out.extend(lfh);
        out.extend(name.as_bytes());
        out.extend(&body);
        let mut cdh = vec![0u8; 46];
        cdh[..4].copy_from_slice(b"PK\x01\x02");
        put32(&mut cdh, 20, body.len() as u32);
        put32(&mut cdh, 24, body.len() as u32);
        put16(&mut cdh, 28, name.len() as u16);
        put32(&mut cdh, 42, at);
        central.extend(cdh);
        central.extend(name.as_bytes());
    }
    let cd_at = out.len() as u32;
    out.extend(&central);
    let mut end = vec![0u8; 22];
    end[..4].copy_from_slice(b"PK\x05\x06");
    put16(&mut end, 8, names.len() as u16);
    put16(&mut end, 10, names.len() as u16);
    put32(&mut end, 12, central.len() as u32);
    put32(&mut end, 16, cd_at);
    out.extend(end);
    out
}

fn mp3_file() -> Vec<u8> {
    let mut v = b"ID3\x03\0\0\0\0\0\x20".to_vec();
    v.extend([0u8; 32]);
    for _ in 0..20 {
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz: 417 bytes.
        v.extend([0xFF, 0xFB, 0x90, 0x00]);
        v.extend([0x55; 413]);
    }
    v.extend(b"TAG");
    v.extend([0x20; 125]);
    v
}

fn webm_file() -> Vec<u8> {
    let el = |id: &[u8], body: &[u8]| {
        let mut v = id.to_vec();
        v.extend((0x0100_0000_0000_0000u64 | body.len() as u64).to_be_bytes());
        v.extend(body);
        v
    };
    let header = el(&[0x1A, 0x45, 0xDF, 0xA3], &el(&[0x42, 0x82], b"webm"));
    let info = el(&[0x15, 0x49, 0xA9, 0x66], &[0x2A; 40]);
    let cluster = el(&[0x1F, 0x43, 0xB6, 0x75], &[0x33; 3000]);
    let segment = el(&[0x18, 0x53, 0x80, 0x67], &[info, cluster].concat());
    [header, segment].concat()
}

fn psd_file() -> Vec<u8> {
    let mut v = b"8BPS\0\x01\0\0\0\0\0\0\0\x03".to_vec();
    v.extend(16u32.to_be_bytes()); // height
    v.extend(16u32.to_be_bytes()); // width
    v.extend(8u16.to_be_bytes());
    v.extend(3u16.to_be_bytes());
    v.extend(0u32.to_be_bytes()); // colour mode data
    v.extend(0u32.to_be_bytes()); // resources
    v.extend(0u32.to_be_bytes()); // layers
    v.extend(0u16.to_be_bytes()); // raw
    v.extend(vec![0x80; 3 * 16 * 16]);
    v
}

fn raf_file() -> Vec<u8> {
    let jpeg = camera_jpeg(64, 48, 9);
    let mut raf = b"FUJIFILMCCD-RAW ".to_vec();
    raf.resize(84, b'0');
    let data_at = 160 + jpeg.len() as u32;
    for v in [
        160u32,
        jpeg.len() as u32,
        data_at,
        200,
        data_at + 200,
        20_000,
    ] {
        raf.extend(v.to_be_bytes());
    }
    raf.resize(160, 0);
    raf.extend(&jpeg);
    raf.resize((data_at + 200 + 20_000) as usize, 7);
    raf
}

fn riff(form: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = form.to_vec();
    for (id, data) in chunks {
        body.extend(*id);
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut v = b"RIFF".to_vec();
    v.extend((body.len() as u32).to_le_bytes());
    v.extend(body);
    v
}

fn mts_file() -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..1100u32 {
        let mut p = vec![0u8; 192];
        p[..4].copy_from_slice(&i.to_be_bytes());
        p[4] = 0x47;
        p[5..]
            .iter_mut()
            .enumerate()
            .for_each(|(j, b)| *b = (j as u8) ^ (i as u8));
        v.extend(p);
    }
    v
}

fn pdf_file() -> Vec<u8> {
    let mut v = b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\n".to_vec();
    v.extend(b"xref\n0 2\ntrailer << /Root 1 0 R >>\nstartxref\n9\n%%EOF\n");
    v
}

fn synthetic_samples() -> Vec<Subject> {
    let img = noise_image(48, 32, 3);
    vec![
        Subject::sample("jpeg", camera_jpeg(160, 120, 1)),
        Subject::sample("mp4", camera_mp4(40)),
        Subject::sample("nef", raw_tiff("NIKON CORPORATION", 3000)),
        Subject::sample("png", encode(&img, image::ImageFormat::Png)),
        Subject::sample("gif", encode(&img, image::ImageFormat::Gif)),
        Subject::sample("bmp", encode(&img, image::ImageFormat::Bmp)),
        Subject::sample("tif", encode(&img, image::ImageFormat::Tiff)),
        Subject::sample("webp", encode(&img, image::ImageFormat::WebP)),
        Subject::sample(
            "docx",
            zip_file(&["[Content_Types].xml", "word/document.xml"]),
        ),
        Subject::sample("zip", zip_file(&["a.txt", "b/c.txt"])),
        Subject::sample("pdf", pdf_file()),
        Subject::sample("mp3", mp3_file()),
        Subject::sample("webm", webm_file()),
        Subject::sample("psd", psd_file()),
        Subject::sample("raf", raf_file()),
        Subject::sample("mts", mts_file()),
        Subject::sample(
            "wav",
            riff(
                b"WAVE",
                &[(b"fmt ", vec![1; 16]), (b"data", vec![0x10; 4001])],
            ),
        ),
        Subject::sample(
            "avi",
            riff(
                b"AVI ",
                &[(b"LIST", vec![2; 200]), (b"LIST", vec![3; 3000])],
            ),
        ),
    ]
}

/// FAT16 directory entry.
fn dirent(name: &[u8; 11], attr: u8, first: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    put16(&mut e, 14, 0x6000);
    put16(&mut e, 16, 0x30EF);
    put16(&mut e, 20, (first >> 16) as u16);
    put16(&mut e, 22, 0x7000);
    put16(&mut e, 24, 0x30EF);
    put16(&mut e, 26, first as u16);
    put32(&mut e, 28, size);
    e
}

/// Long-name entries for `name`, last part first, as stored.
fn lfn(name: &str, short: &[u8; 11], deleted: bool) -> Vec<u8> {
    let sum = short
        .iter()
        .fold(0u8, |s, &b| s.rotate_right(1).wrapping_add(b));
    let mut units: Vec<u16> = name.encode_utf16().collect();
    units.push(0);
    while !units.len().is_multiple_of(13) {
        units.push(0xFFFF);
    }
    let parts: Vec<&[u16]> = units.chunks(13).collect();
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate().rev() {
        let mut e = [0u8; 32];
        e[0] = if deleted {
            0xE5
        } else {
            (i as u8 + 1) | if i == parts.len() - 1 { 0x40 } else { 0 }
        };
        e[11] = 0x0F;
        e[13] = sum;
        for (j, &u) in part.iter().enumerate() {
            let at = match j {
                0..=4 => 1 + j * 2,
                5..=10 => 14 + (j - 5) * 2,
                _ => 28 + (j - 11) * 2,
            };
            put16(&mut e, at, u);
        }
        out.extend(e);
    }
    out
}

/// An MBR card with a FAT16 volume: live and deleted photos, long names,
/// a folder and a deleted folder.
fn fat16_card() -> Vec<u8> {
    const PART: usize = 2048 * 512;
    const SECTORS: usize = 8192;
    let mut vol = vec![0u8; SECTORS * 512];
    let b = &mut vol[..512];
    b[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    b[3..11].copy_from_slice(b"MSDOS5.0");
    put16(b, 11, 512);
    b[13] = 1; // sectors per cluster
    put16(b, 14, 4); // reserved
    b[16] = 2; // FATs
    put16(b, 17, 512); // root entries
    put16(b, 19, SECTORS as u16);
    b[21] = 0xF8;
    put16(b, 22, 32); // sectors per FAT
    b[54..62].copy_from_slice(b"FAT16   ");
    b[510] = 0x55;
    b[511] = 0xAA;
    let fat_at = 4 * 512;
    let root_at = (4 + 64) * 512;
    let data_at = root_at + 32 * 512;
    let cluster_at = |c: usize| data_at + (c - 2) * 512;

    let photo1 = camera_jpeg(120, 90, 11);
    let photo2 = camera_jpeg(100, 80, 12);
    let photo3 = camera_jpeg(90, 60, 13);
    let mut fat = vec![0u16; 8192];
    fat[0] = 0xFFF8;
    fat[1] = 0xFFFF;
    let place = |vol: &mut Vec<u8>, fat: &mut Vec<u16>, first: usize, data: &[u8], live: bool| {
        let n = data.len().div_ceil(512).max(1);
        vol[cluster_at(first)..cluster_at(first) + data.len()].copy_from_slice(data);
        if live {
            for (c, next) in fat.iter_mut().enumerate().skip(first).take(n) {
                *next = if c == first + n - 1 {
                    0xFFFF
                } else {
                    c as u16 + 1
                };
            }
        }
    };
    let mut root = Vec::new();
    root.extend(dirent(b"CARD       ", 0x08, 0, 0));
    root.extend(dirent(b"DCIM       ", 0x10, 2, 0));
    let mut dcim = Vec::new();
    dcim.extend(dirent(b".          ", 0x10, 2, 0));
    dcim.extend(dirent(b"..         ", 0x10, 0, 0));
    dcim.extend(dirent(b"IMG_0001JPG", 0x20, 10, photo1.len() as u32));
    let short = *b"\xE5MG_0002JPG";
    dcim.extend(lfn("Holiday photo.jpg", &short, true));
    dcim.extend(dirent(&short, 0x20, 100, photo2.len() as u32));
    dcim.extend(dirent(b"\xE5ELETED    ", 0x10, 300, 0));
    place(&mut vol, &mut fat, 2, &[0; 512], true);
    vol[cluster_at(2)..cluster_at(2) + dcim.len()].copy_from_slice(&dcim);
    place(&mut vol, &mut fat, 10, &photo1, true);
    place(&mut vol, &mut fat, 100, &photo2, false);
    let mut gone = Vec::new();
    gone.extend(dirent(b".          ", 0x10, 300, 0));
    gone.extend(dirent(b"..         ", 0x10, 0, 0));
    gone.extend(dirent(b"\xE5MG_0003JPG", 0x20, 302, photo3.len() as u32));
    vol[cluster_at(300)..cluster_at(300) + gone.len()].copy_from_slice(&gone);
    place(&mut vol, &mut fat, 302, &photo3, false);
    let notes_short = *b"NOTES   TXT";
    root.extend(lfn("Shopping notes.txt", &notes_short, false));
    root.extend(dirent(&notes_short, 0x20, 500, 26));
    place(
        &mut vol,
        &mut fat,
        500,
        b"eggs, milk, bread, coffee\n",
        true,
    );
    vol[root_at..root_at + root.len()].copy_from_slice(&root);
    for copy in 0..2 {
        let at = fat_at + copy * 32 * 512;
        for (i, v) in fat.iter().enumerate() {
            put16(&mut vol, at + i * 2, *v);
        }
    }

    let mut disk = vec![0u8; PART];
    disk[446 + 4] = 0x06;
    put32(&mut disk, 446 + 8, 2048);
    put32(&mut disk, 446 + 12, SECTORS as u32);
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk.extend(vol);
    disk
}

/// exFAT entry set: file, stream extension and names.
fn exfat_set(
    name: &str,
    attrs: u16,
    first: u32,
    len: u64,
    contiguous: bool,
    deleted: bool,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let names = units.len().div_ceil(15);
    let mut out = vec![0u8; 32 * (2 + names)];
    out[0] = 0x85;
    out[1] = (1 + names) as u8;
    put16(&mut out, 4, attrs);
    put32(&mut out, 8, 0x30EF_6000);
    put32(&mut out, 12, 0x30EF_7000);
    let s = &mut out[32..64];
    s[0] = 0xC0;
    s[1] = 0x01 | if contiguous { 0x02 } else { 0 };
    s[3] = units.len() as u8;
    put64(s, 8, len);
    put32(s, 20, first);
    put64(s, 24, len);
    for (i, chunk) in units.chunks(15).enumerate() {
        let e = &mut out[64 + i * 32..96 + i * 32];
        e[0] = 0xC1;
        for (j, &u) in chunk.iter().enumerate() {
            put16(e, 2 + j * 2, u);
        }
    }
    if deleted {
        for e in out.chunks_mut(32) {
            e[0] &= 0x7F;
        }
    }
    out
}

/// A GPT disk with an exFAT volume.
fn exfat_card() -> Vec<u8> {
    const CLUSTER: usize = 4096;
    const CLUSTERS: usize = 1000;
    let heap = 64 * 512;
    let mut vol = vec![0u8; heap + CLUSTERS * CLUSTER];
    let b = &mut vol[..512];
    b[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(b"EXFAT   ");
    put64(b, 72, ((heap + CLUSTERS * CLUSTER) / 512) as u64);
    put32(b, 80, 32); // FAT offset, sectors
    put32(b, 84, 16); // FAT length
    put32(b, 88, 64); // heap offset
    put32(b, 92, CLUSTERS as u32);
    put32(b, 96, 4); // root directory cluster
    b[108] = 9;
    b[109] = 3;
    b[110] = 1;
    b[510] = 0x55;
    b[511] = 0xAA;
    let at = |c: usize| heap + (c - 2) * CLUSTER;
    let photo1 = camera_jpeg(140, 100, 21);
    let photo2 = camera_jpeg(120, 90, 22);
    let video = camera_mp4(30);
    let mut bitmap = vec![0u8; CLUSTERS.div_ceil(8)];
    let mut fat = vec![0u32; CLUSTERS + 2];
    fat[0] = 0xFFFF_FFF8;
    fat[1] = 0xFFFF_FFFF;
    let used = |c: usize, n: usize, bitmap: &mut Vec<u8>| {
        for k in c..c + n {
            bitmap[(k - 2) / 8] |= 1 << ((k - 2) % 8);
        }
    };
    used(2, 1, &mut bitmap); // bitmap
    used(3, 1, &mut bitmap); // up-case table
    used(4, 1, &mut bitmap); // root
    used(5, 1, &mut bitmap); // DCIM
    fat[4] = 0xFFFF_FFFF;
    fat[5] = 0xFFFF_FFFF;
    let mut root = Vec::new();
    let mut e = [0u8; 32];
    e[0] = 0x83;
    e[1] = 4;
    for (i, u) in "CARD".encode_utf16().enumerate() {
        put16(&mut e, 2 + i * 2, u);
    }
    root.extend(e);
    let mut e = [0u8; 32];
    e[0] = 0x81;
    put32(&mut e, 20, 2);
    put64(&mut e, 24, bitmap.len() as u64);
    root.extend(e);
    root.extend(exfat_set("DCIM", 0x10, 5, CLUSTER as u64, false, false));
    let mut dcim = Vec::new();
    let n1 = photo1.len().div_ceil(CLUSTER);
    dcim.extend(exfat_set(
        "IMG_0001.JPG",
        0x20,
        10,
        photo1.len() as u64,
        true,
        false,
    ));
    used(10, n1, &mut bitmap);
    vol[at(10)..at(10) + photo1.len()].copy_from_slice(&photo1);
    dcim.extend(exfat_set(
        "IMG_0002.JPG",
        0x20,
        40,
        photo2.len() as u64,
        true,
        true,
    ));
    vol[at(40)..at(40) + photo2.len()].copy_from_slice(&photo2);
    // A deleted video stored with a FAT chain, which deletion cleared.
    dcim.extend(exfat_set(
        "MVI_0003.MP4",
        0x20,
        80,
        video.len() as u64,
        false,
        true,
    ));
    vol[at(80)..at(80) + video.len()].copy_from_slice(&video);
    vol[at(4)..at(4) + root.len()].copy_from_slice(&root);
    vol[at(5)..at(5) + dcim.len()].copy_from_slice(&dcim);
    vol[at(2)..at(2) + bitmap.len()].copy_from_slice(&bitmap);
    for (i, v) in fat.iter().enumerate() {
        put32(&mut vol, 32 * 512 + i * 4, *v);
    }
    gpt_disk(&vol, [0xA2, 0xA0, 0xD0, 0xEB])
}

/// Wraps a volume in a protective MBR and a GPT with one partition.
fn gpt_disk(vol: &[u8], type_prefix: [u8; 4]) -> Vec<u8> {
    let first = 2048u64;
    let mut disk = vec![0u8; first as usize * 512];
    disk[446 + 4] = 0xEE;
    put32(&mut disk, 446 + 8, 1);
    put32(
        &mut disk,
        446 + 12,
        (first + vol.len() as u64 / 512 + 33) as u32,
    );
    disk[510] = 0x55;
    disk[511] = 0xAA;
    let h = &mut disk[512..1024];
    h[..8].copy_from_slice(b"EFI PART");
    put64(h, 72, 2); // entries LBA
    put32(h, 80, 128); // entry count
    put32(h, 84, 128); // entry size
    let e = &mut disk[1024..1152];
    e[..4].copy_from_slice(&type_prefix);
    e[16] = 1;
    put64(e, 32, first);
    put64(e, 40, first + vol.len() as u64 / 512 - 1);
    for (i, u) in "Card".encode_utf16().enumerate() {
        put16(e, 56 + i * 2, u);
    }
    disk.extend(vol);
    disk.extend(vec![0u8; 34 * 512]);
    disk
}

/// One NTFS attribute.
fn ntfs_attr(typ: u32, resident_value: Option<&[u8]>, runs: Option<(&[u8], u64)>) -> Vec<u8> {
    let mut a = match (resident_value, runs) {
        (Some(v), _) => {
            let mut a = vec![0u8; 24 + v.len().div_ceil(8) * 8];
            put32(&mut a, 16, v.len() as u32);
            put16(&mut a, 20, 24);
            a[24..24 + v.len()].copy_from_slice(v);
            a
        }
        (None, Some((r, size))) => {
            let mut a = vec![0u8; 64 + (r.len() + 1).div_ceil(8) * 8];
            a[8] = 1;
            put16(&mut a, 32, 64);
            put64(&mut a, 40, size.div_ceil(4096) * 4096);
            put64(&mut a, 48, size);
            put64(&mut a, 56, size);
            a[64..64 + r.len()].copy_from_slice(r);
            a
        }
        _ => unreachable!(),
    };
    put32(&mut a, 0, typ);
    let len = a.len() as u32;
    put32(&mut a, 4, len);
    a
}

fn ntfs_name(name: &str, parent: u64) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut v = vec![0u8; 66 + units.len() * 2];
    put64(&mut v, 0, parent | (1 << 48));
    // 2004-07-15 in Windows time.
    let t = (1_089_900_239u64 + 11_644_473_600) * 10_000_000;
    put64(&mut v, 8, t);
    put64(&mut v, 16, t);
    v[64] = units.len() as u8;
    v[65] = 1;
    for (i, u) in units.iter().enumerate() {
        put16(&mut v, 66 + i * 2, *u);
    }
    v
}

/// A 1 KiB MFT record with update sequence fixups applied.
fn ntfs_record(flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut r = vec![0u8; 1024];
    r[..4].copy_from_slice(b"FILE");
    put16(&mut r, 4, 48);
    put16(&mut r, 6, 3);
    put16(&mut r, 20, 56);
    put16(&mut r, 22, flags);
    put32(&mut r, 28, 1024);
    let mut at = 56;
    for a in attrs {
        r[at..at + a.len()].copy_from_slice(a);
        at += a.len();
    }
    put32(&mut r, at, 0xFFFF_FFFF);
    put32(&mut r, 24, at as u32 + 8);
    let usn = [0x2A, 0x00];
    r[48..50].copy_from_slice(&usn);
    for i in 1..3 {
        let end = i * 512;
        r[48 + i * 2] = r[end - 2];
        r[49 + i * 2] = r[end - 1];
        r[end - 2..end].copy_from_slice(&usn);
    }
    r
}

/// A run list of (cluster, count) pairs.
fn runs(list: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(lcn, n) in list {
        let delta = lcn as i64 - prev;
        prev = lcn as i64;
        out.push(0x44);
        out.extend(&(n as u32).to_le_bytes());
        out.extend(&(delta as i32).to_le_bytes());
    }
    out.push(0);
    out
}

/// An NTFS volume with no partition table (as Windows formats large
/// sticks): a live photo, a deleted fragmented photo, a resident file and
/// a folder.
fn ntfs_card() -> Vec<u8> {
    const CLUSTER: usize = 4096;
    const CLUSTERS: usize = 1024;
    let mut vol = vec![0u8; CLUSTERS * CLUSTER];
    let b = &mut vol[..512];
    b[..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    put16(b, 11, 512);
    b[13] = 8;
    b[21] = 0xF8;
    put64(b, 40, (CLUSTERS * 8 - 1) as u64);
    put64(b, 48, 4); // MFT cluster
    put64(b, 56, 2);
    b[64] = 0xF6; // 1 KiB records
    b[68] = 1;
    b[510] = 0x55;
    b[511] = 0xAA;
    let photo1 = camera_jpeg(150, 100, 31);
    let photo2 = camera_jpeg(130, 100, 32);
    let mut bitmap = vec![0u8; CLUSTERS / 8];
    let mut used = |lcn: usize, n: usize| {
        for c in lcn..lcn + n {
            bitmap[c / 8] |= 1 << (c % 8);
        }
    };
    used(0, 13);
    let n1 = photo1.len().div_ceil(CLUSTER);
    used(20, n1);
    vol[20 * CLUSTER..20 * CLUSTER + photo1.len()].copy_from_slice(&photo1);
    // The deleted photo in two pieces.
    let split = CLUSTER;
    vol[100 * CLUSTER..100 * CLUSTER + split].copy_from_slice(&photo2[..split]);
    let rest = photo2.len() - split;
    vol[200 * CLUSTER..200 * CLUSTER + rest].copy_from_slice(&photo2[split..]);
    let p2_runs = runs(&[(100, 1), (200, rest.div_ceil(CLUSTER) as u64)]);
    let p1_runs = runs(&[(20, n1 as u64)]);
    let mft_runs = runs(&[(4, 8)]);
    let bitmap_runs = runs(&[(12, 1)]);

    let mut records = vec![vec![0u8; 1024]; 32];
    records[0] = ntfs_record(
        1,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("$MFT", 5)), None),
            ntfs_attr(0x80, None, Some((&mft_runs, 32 * 1024))),
        ],
    );
    let label: Vec<u8> = "Card".encode_utf16().flat_map(u16::to_le_bytes).collect();
    records[3] = ntfs_record(
        1,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("$Volume", 5)), None),
            ntfs_attr(0x60, Some(&label), None),
        ],
    );
    records[5] = ntfs_record(3, &[ntfs_attr(0x30, Some(&ntfs_name(".", 5)), None)]);
    records[6] = ntfs_record(
        1,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("$Bitmap", 5)), None),
            ntfs_attr(0x80, None, Some((&bitmap_runs, bitmap.len() as u64))),
        ],
    );
    records[16] = ntfs_record(3, &[ntfs_attr(0x30, Some(&ntfs_name("DCIM", 5)), None)]);
    records[17] = ntfs_record(
        1,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("IMG_0001.JPG", 16)), None),
            ntfs_attr(0x80, None, Some((&p1_runs, photo1.len() as u64))),
        ],
    );
    records[18] = ntfs_record(
        0,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("IMG_0002.JPG", 16)), None),
            ntfs_attr(0x80, None, Some((&p2_runs, photo2.len() as u64))),
        ],
    );
    records[19] = ntfs_record(
        0,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("notes.txt", 5)), None),
            ntfs_attr(0x80, Some(b"remember the milk"), None),
        ],
    );
    for (i, r) in records.iter().enumerate() {
        let at = 4 * CLUSTER + i * 1024;
        vol[at..at + 1024].copy_from_slice(r);
    }
    vol[12 * CLUSTER..12 * CLUSTER + bitmap.len()].copy_from_slice(&bitmap);
    vol
}

/// HFS+ catalog key and record.
fn hfs_record(parent: u32, name: &str, data: &[u8]) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut v = Vec::new();
    v.extend(((6 + 2 * units.len()) as u16).to_be_bytes());
    v.extend(parent.to_be_bytes());
    v.extend((units.len() as u16).to_be_bytes());
    for u in units {
        v.extend(u.to_be_bytes());
    }
    v.extend(data);
    v
}

fn hfs_file(id: u32, size: u64, start: u32, blocks: u32) -> Vec<u8> {
    let mut r = vec![0u8; 248];
    put16be(&mut r, 0, 2);
    put32be(&mut r, 8, id);
    put32be(&mut r, 12, 3_172_744_239); // 2004
    put32be(&mut r, 16, 3_172_744_239);
    put64be(&mut r, 88, size);
    put32be(&mut r, 88 + 12, blocks);
    put32be(&mut r, 88 + 16, start);
    put32be(&mut r, 88 + 20, blocks);
    r
}

fn hfs_folder(id: u32) -> Vec<u8> {
    let mut r = vec![0u8; 88];
    put16be(&mut r, 0, 1);
    put32be(&mut r, 8, id);
    r
}

fn hfs_thread(parent: u32, name: &str) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut r = vec![0u8; 10 + 2 * units.len()];
    put16be(&mut r, 0, 3);
    put32be(&mut r, 4, parent);
    put16be(&mut r, 8, units.len() as u16);
    for (i, u) in units.iter().enumerate() {
        put16be(&mut r, 10 + i * 2, *u);
    }
    r
}

/// A GPT disk with an HFS+ volume: one live photo, and a deleted one whose
/// record survives in the leftover bytes of the catalog leaf.
fn hfs_card() -> Vec<u8> {
    const BLOCK: usize = 4096;
    const BLOCKS: usize = 1024;
    let mut vol = vec![0u8; BLOCK * BLOCKS];
    let vh = &mut vol[1024..1536];
    vh[..2].copy_from_slice(b"H+");
    put16be(vh, 2, 4);
    put32be(vh, 40, BLOCK as u32);
    put32be(vh, 44, BLOCKS as u32);
    // Allocation file at block 1, catalog at blocks 2..4.
    put64be(vh, 112, (BLOCKS / 8) as u64);
    put32be(vh, 112 + 12, 1);
    put32be(vh, 112 + 16, 1);
    put32be(vh, 112 + 20, 1);
    put64be(vh, 272, 2 * BLOCK as u64);
    put32be(vh, 272 + 12, 2);
    put32be(vh, 272 + 16, 2);
    put32be(vh, 272 + 20, 2);
    let photo1 = camera_jpeg(140, 100, 41);
    let photo2 = camera_jpeg(110, 90, 42);
    let (b1, b2) = (10usize, 60usize);
    let n1 = photo1.len().div_ceil(BLOCK);
    let n2 = photo2.len().div_ceil(BLOCK);
    vol[b1 * BLOCK..b1 * BLOCK + photo1.len()].copy_from_slice(&photo1);
    vol[b2 * BLOCK..b2 * BLOCK + photo2.len()].copy_from_slice(&photo2);
    let mut bitmap = vec![0u8; BLOCKS / 8];
    for b in (0..4).chain(b1..b1 + n1) {
        bitmap[b / 8] |= 0x80 >> (b % 8);
    }
    vol[BLOCK..BLOCK + bitmap.len()].copy_from_slice(&bitmap);

    // Header node.
    let cat = 2 * BLOCK;
    let h = &mut vol[cat..cat + BLOCK];
    h[8] = 1;
    put16be(h, 10, 3);
    put16be(h, 14, 1); // depth
    put32be(h, 16, 1); // root
    put32be(h, 20, 3); // leaf records
    put32be(h, 24, 1); // first leaf
    put32be(h, 28, 1); // last leaf
    put16be(h, 32, BLOCK as u16);
    put16be(h, 34, 516);
    put32be(h, 36, 2); // total nodes
                       // Leaf node: root folder, its thread, a live file; then the leftover
                       // record of a deleted file after the free-space offset.
    let leaf = cat + BLOCK;
    let recs = [
        hfs_record(1, "Card", &hfs_folder(2)),
        hfs_record(2, "", &hfs_thread(1, "Card")),
        hfs_record(
            2,
            "IMG_0001.JPG",
            &hfs_file(20, photo1.len() as u64, b1 as u32, n1 as u32),
        ),
    ];
    let n = &mut vol[leaf..leaf + BLOCK];
    n[8] = 0xFF;
    n[9] = 1;
    put16be(n, 10, recs.len() as u16);
    let mut at = 14;
    let mut offsets = Vec::new();
    for r in &recs {
        offsets.push(at);
        n[at..at + r.len()].copy_from_slice(r);
        at += r.len();
    }
    offsets.push(at);
    let deleted = hfs_record(
        2,
        "IMG_0002.JPG",
        &hfs_file(21, photo2.len() as u64, b2 as u32, n2 as u32),
    );
    n[at..at + deleted.len()].copy_from_slice(&deleted);
    for (i, o) in offsets.iter().enumerate() {
        put16be(n, BLOCK - 2 * (i + 1), *o as u16);
    }
    gpt_disk(&vol, [0x00, 0x53, 0x46, 0x48])
}

/// A B-tree node with variable-size entries (`fixed` false) or the
/// object map's fixed 16-byte keys and values.
fn apfs_node(
    oid: u64,
    xid: u64,
    subtype: u32,
    flags: u16,
    entries: &[(Vec<u8>, Vec<u8>)],
    fixed: bool,
) -> Vec<u8> {
    let mut n = vec![0u8; 4096];
    put64(&mut n, 8, oid);
    put64(&mut n, 16, xid);
    put32(&mut n, 24, 0x4000_0000 | if flags & 1 != 0 { 2 } else { 3 });
    put32(&mut n, 28, subtype);
    put16(&mut n, 32, flags);
    put32(&mut n, 36, entries.len() as u32);
    let toc_len = entries.len() * if fixed { 4 } else { 8 };
    put16(&mut n, 40, 0);
    put16(&mut n, 42, toc_len as u16);
    let keys = 56 + toc_len;
    let vals_end = if flags & 1 != 0 { 4096 - 40 } else { 4096 };
    let (mut k_off, mut v_off) = (0usize, 0usize);
    for (i, (k, v)) in entries.iter().enumerate() {
        n[keys + k_off..keys + k_off + k.len()].copy_from_slice(k);
        v_off += v.len();
        n[vals_end - v_off..vals_end - v_off + v.len()].copy_from_slice(v);
        if fixed {
            put16(&mut n, 56 + i * 4, k_off as u16);
            put16(&mut n, 58 + i * 4, v_off as u16);
        } else {
            put16(&mut n, 56 + i * 8, k_off as u16);
            put16(&mut n, 58 + i * 8, k.len() as u16);
            put16(&mut n, 60 + i * 8, v_off as u16);
            put16(&mut n, 62 + i * 8, v.len() as u16);
        }
        k_off += k.len().div_ceil(8) * 8;
    }
    let sum = fletcher64(&n[8..]);
    put64(&mut n, 0, sum);
    n
}

fn seal(mut obj: Vec<u8>) -> Vec<u8> {
    let sum = fletcher64(&obj[8..]);
    put64(&mut obj, 0, sum);
    obj
}

fn apfs_key(kind: u64, id: u64, rest: &[u8]) -> Vec<u8> {
    let mut k = ((kind << 60) | id).to_le_bytes().to_vec();
    k.extend(rest);
    k
}

fn apfs_inode(parent: u64, private: u64, size: u64) -> Vec<u8> {
    let mut v = vec![0u8; 92];
    put64(&mut v, 0, parent);
    put64(&mut v, 8, private);
    let t = 1_089_900_239u64 * 1_000_000_000;
    put64(&mut v, 16, t);
    put64(&mut v, 24, t);
    put16(&mut v, 80, 0o100_644);
    // One extended field: the data stream, 40 bytes.
    v.extend(1u16.to_le_bytes());
    v.extend(40u16.to_le_bytes());
    v.extend([8, 0]);
    v.extend(40u16.to_le_bytes());
    let mut ds = vec![0u8; 40];
    put64(&mut ds, 0, size);
    v.extend(ds);
    v
}

fn apfs_drec(parent: u64, name: &str, id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut z = name.as_bytes().to_vec();
    z.push(0);
    let mut rest = ((z.len() as u32) | (0x1234 << 10)).to_le_bytes().to_vec();
    rest.extend(&z);
    let mut v = vec![0u8; 18];
    put64(&mut v, 0, id);
    put16(&mut v, 16, 8);
    (apfs_key(9, parent, &rest), v)
}

fn apfs_extent(owner: u64, len: u64, block: u64) -> (Vec<u8>, Vec<u8>) {
    let mut v = vec![0u8; 24];
    put64(&mut v, 0, len);
    put64(&mut v, 8, block);
    (apfs_key(8, owner, &0u64.to_le_bytes()), v)
}

/// A GPT disk with an APFS container: one live photo, and an older copy
/// of the directory node in free space listing a deleted one, as the deep
/// scan finds it.
fn apfs_card() -> Vec<u8> {
    const BLOCK: usize = 4096;
    const BLOCKS: usize = 1024;
    let mut vol = vec![0u8; BLOCK * BLOCKS];
    let photo1 = camera_jpeg(140, 100, 51);
    let photo2 = camera_jpeg(120, 80, 52);
    let (b1, b2) = (100u64, 300u64);
    vol[b1 as usize * BLOCK..b1 as usize * BLOCK + photo1.len()].copy_from_slice(&photo1);
    vol[b2 as usize * BLOCK..b2 as usize * BLOCK + photo2.len()].copy_from_slice(&photo2);
    let xid = 10;

    // Blocks: 0 superblock, 1-4 checkpoint descriptors, 10 container
    // omap, 11 its tree, 20 volume superblock, 21 volume omap, 22 its
    // tree, 23 the directory tree root; 500 an old directory node.
    let mut nx = vec![0u8; BLOCK];
    put64(&mut nx, 8, 1);
    put64(&mut nx, 16, xid);
    put32(&mut nx, 24, 0x8000_0001);
    nx[32..36].copy_from_slice(b"NXSB");
    put32(&mut nx, 36, BLOCK as u32);
    put64(&mut nx, 40, BLOCKS as u64);
    put32(&mut nx, 104, 4);
    put64(&mut nx, 112, 1);
    put32(&mut nx, 108, 4);
    put64(&mut nx, 120, 5);
    put64(&mut nx, 160, 10);
    put32(&mut nx, 180, 1);
    put64(&mut nx, 184, 1026);
    let nx = seal(nx);
    let mut omap = vec![0u8; BLOCK];
    put32(&mut omap, 24, 0x4000_000B);
    put64(&mut omap, 48, 11);
    let omap_entry = |oid: u64, block: u64| {
        let mut k = oid.to_le_bytes().to_vec();
        k.extend(xid.to_le_bytes());
        let mut v = vec![0u8; 16];
        put32(&mut v, 4, BLOCK as u32);
        put64(&mut v, 8, block);
        (k, v)
    };
    let omap_tree = apfs_node(11, xid, 0x0B, 1 | 2 | 4, &[omap_entry(1026, 20)], true);
    let mut sb = vec![0u8; BLOCK];
    put64(&mut sb, 8, 1026);
    put64(&mut sb, 16, xid);
    put32(&mut sb, 24, 0x0D);
    sb[32..36].copy_from_slice(b"APSB");
    put32(&mut sb, 116, 0); // virtual directory tree
    put64(&mut sb, 128, 21);
    put64(&mut sb, 136, 1028);
    put64(&mut sb, 264, 1); // unencrypted
    sb[704..708].copy_from_slice(b"Card");
    let sb = seal(sb);
    let mut vomap = vec![0u8; BLOCK];
    put32(&mut vomap, 24, 0x4000_000B);
    put64(&mut vomap, 48, 22);
    let vomap_tree = apfs_node(22, xid, 0x0B, 1 | 2 | 4, &[omap_entry(1028, 23)], true);
    let live = [
        apfs_drec(2, "IMG_0001.JPG", 20),
        (apfs_key(3, 20, &[]), apfs_inode(2, 20, photo1.len() as u64)),
        apfs_extent(20, photo1.len().div_ceil(BLOCK) as u64 * BLOCK as u64, b1),
    ];
    let tree = apfs_node(1028, xid, 0x0E, 1 | 2, &live, false);
    let old = [
        apfs_drec(2, "IMG_0001.JPG", 20),
        apfs_drec(2, "IMG_0002.JPG", 21),
        (apfs_key(3, 20, &[]), apfs_inode(2, 20, photo1.len() as u64)),
        (apfs_key(3, 21, &[]), apfs_inode(2, 21, photo2.len() as u64)),
        apfs_extent(20, photo1.len().div_ceil(BLOCK) as u64 * BLOCK as u64, b1),
        apfs_extent(21, photo2.len().div_ceil(BLOCK) as u64 * BLOCK as u64, b2),
    ];
    let old_tree = apfs_node(1028, xid - 1, 0x0E, 1 | 2, &old, false);
    for (block, obj) in [
        (0, &nx),
        (1, &nx),
        (10, &seal(omap)),
        (11, &omap_tree),
        (20, &sb),
        (21, &seal(vomap)),
        (22, &vomap_tree),
        (23, &tree),
        (500, &old_tree),
    ] {
        vol[block * BLOCK..(block + 1) * BLOCK].copy_from_slice(obj);
    }
    gpt_disk(&vol, [0xEF, 0x57, 0x34, 0x7C])
}

fn synthetic_images() -> Vec<Subject> {
    vec![
        Subject::image("fat16", fat16_card()),
        Subject::image("exfat", exfat_card()),
        Subject::image("ntfs", ntfs_card()),
        Subject::image("hfsplus", hfs_card()),
        Subject::image("apfs", apfs_card()),
    ]
}

// ---------------------------------------------------------------------------
// The tests

/// The synthetic images must be read correctly before corrupting them
/// means anything.
#[test]
fn synthetic_images_are_readable() {
    let expect = [
        (
            "fat16",
            vec![
                "DCIM/IMG_0001.JPG",
                "DCIM/Holiday photo.jpg",
                "Shopping notes.txt",
            ],
        ),
        (
            "exfat",
            vec![
                "DCIM/IMG_0001.JPG",
                "DCIM/IMG_0002.JPG",
                "DCIM/MVI_0003.MP4",
            ],
        ),
        (
            "ntfs",
            vec!["DCIM/IMG_0001.JPG", "DCIM/IMG_0002.JPG", "notes.txt"],
        ),
        ("hfsplus", vec!["IMG_0001.JPG", "IMG_0002.JPG"]),
        ("apfs", vec!["IMG_0001.JPG", "IMG_0002.JPG"]),
    ];
    for (s, (name, paths)) in synthetic_images().iter().zip(expect) {
        assert_eq!(s.name, name);
        let src: &dyn Source = s.data.as_ref();
        let result = session::scan(
            src,
            name,
            &ScanOptions::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        let found: Vec<String> = result.items.iter().map(Item::path).collect();
        for p in paths {
            let item = result
                .items
                .iter()
                .find(|i| i.path() == p)
                .unwrap_or_else(|| panic!("{name}: {p} not found in {found:?}"));
            // Text cannot be checked; media must check out.
            assert!(
                matches!(item.health, Health::Good | Health::Unknown),
                "{name}: {p} {:?}",
                item.health
            );
        }
    }
}

#[test]
fn synthetic_images_survive_corruption() {
    run_mutations(
        "synthetic images",
        &synthetic_images(),
        Target::Image { deep: true },
        cases_per_target(if cfg!(debug_assertions) { 40 } else { 400 }),
    );
}

#[test]
fn synthetic_files_survive_corruption() {
    run_mutations(
        "synthetic files",
        &synthetic_samples(),
        Target::Sample,
        cases_per_target(if cfg!(debug_assertions) { 25 } else { 300 }),
    );
}

#[test]
fn random_data_with_signatures_survives() {
    // Pure noise behind each signature the engine looks for.
    let mut report = Report::new();
    let n = cases_per_target(if cfg!(debug_assertions) { 30 } else { 300 });
    for i in 0..n as u64 {
        let seed = 0x5EED_0000 + i;
        let mut rng = Rng::new(seed);
        let mut data = rng.bytes(1 << 20);
        let what = match i % 8 {
            0 => {
                data[3..11].copy_from_slice(b"EXFAT   ");
                "exfat"
            }
            1 => {
                data[3..11].copy_from_slice(b"NTFS    ");
                "ntfs"
            }
            2 => {
                data[32..36].copy_from_slice(b"NXSB");
                put32(&mut data, 36, 4096);
                "apfs"
            }
            3 => {
                data[1024..1026].copy_from_slice(b"H+");
                put32be(&mut data, 1024 + 40, 4096);
                "hfsplus"
            }
            4 => {
                data[0] = 0xEB;
                put16(&mut data, 11, 512);
                data[13] = 1 << rng.below(8);
                data[16] = 2;
                data[510..512].copy_from_slice(&[0x55, 0xAA]);
                "fat"
            }
            5 => {
                data[510..512].copy_from_slice(&[0x55, 0xAA]);
                data[446 + 4] = 0xEE;
                data[512..520].copy_from_slice(b"EFI PART");
                "gpt"
            }
            6 => {
                data[510..512].copy_from_slice(&[0x55, 0xAA]);
                data[446 + 4] = 0x0F;
                "mbr extended"
            }
            7 if i % 16 == 7 => {
                // ext2/3/4 superblock magic, 1 KiB in.
                data[1024 + 56..1024 + 58].copy_from_slice(&[0x53, 0xEF]);
                "ext4"
            }
            _ => "noise",
        };
        let budget = Budget::for_len(data.len());
        let result = run_case(budget, move || exercise_image(&data, true));
        report.add(format!("{what}:{seed:#x}"), result);
    }
    report.finish("random data with signatures");
}

fn dir_files(var: &str, ext: Option<&str>) -> Option<Vec<PathBuf>> {
    let dir = std::env::var_os(var)?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.is_file())
        .filter(|p| ext.is_none_or(|x| p.extension().is_some_and(|e| e == x)))
        .collect();
    files.sort();
    Some(files)
}

fn file_name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

#[test]
fn cards_survive_corruption() {
    let Some(files) = dir_files("REPERIO_TEST_CARDS", Some("img")) else {
        return;
    };
    let subjects: Vec<Subject> = files
        .iter()
        .map(|p| Subject::image(&file_name(p), std::fs::read(p).unwrap()))
        .collect();
    run_mutations(
        "cards",
        &subjects,
        Target::Image { deep: true },
        cases_per_target(if cfg!(debug_assertions) { 10 } else { 150 }),
    );
}

#[test]
fn samples_survive_corruption() {
    let Some(files) = dir_files("REPERIO_TEST_SAMPLES", None) else {
        return;
    };
    let subjects: Vec<Subject> = files
        .iter()
        .map(|p| Subject::sample(&file_name(p), std::fs::read(p).unwrap()))
        .collect();
    run_mutations(
        "format samples",
        &subjects,
        Target::Sample,
        cases_per_target(if cfg!(debug_assertions) { 10 } else { 100 }),
    );
}

// ---------------------------------------------------------------------------
// Regressions: one case for each bug these tests found, built by patching
// the synthetic images and files so they stay readable and stable.

/// Runs regression cases, collecting every failure before reporting.
#[derive(Default)]
struct Cases(Vec<String>);

impl Cases {
    fn run(&mut self, what: &str, data: Vec<u8>, target: Target) {
        let budget = Budget::for_len(data.len());
        if let Err(e) = run_case(budget, move || exercise(&data, target)) {
            self.0.push(format!("{what}: {e}"));
        }
    }

    fn image(&mut self, what: &str, data: Vec<u8>) {
        self.run(what, data, Target::Image { deep: true });
    }

    fn file(&mut self, what: &str, data: Vec<u8>) {
        self.run(what, data, Target::Sample);
    }
}

impl Drop for Cases {
    fn drop(&mut self) {
        if !self.0.is_empty() && !std::thread::panicking() {
            panic!("{}", self.0.join("\n"));
        }
    }
}

/// The synthetic GPT disks' volumes start 1 MiB in.
const VOL: usize = 2048 * 512;

/// Re-seals the APFS object at `at` after a field was changed.
fn reseal(data: &mut [u8], at: usize) {
    let sum = fletcher64(&data[at + 8..at + 4096]);
    put64(data, at, sum);
}

#[test]
fn regression_ntfs_boot_sector_sizes() {
    let mut c = Cases::default();
    // Sectors per cluster 0xC0 means 2^64: the shift overflowed.
    let mut d = ntfs_card();
    d[13] = 0xC0;
    c.image("cluster shift", d);
    // Clusters per record -128 means 2^128 bytes: the shift overflowed.
    let mut d = ntfs_card();
    d[64] = 0x80;
    c.image("record shift", d);
    // Total sectors times sector size overflowed; then the free-space walk
    // counted to 2^64 clusters.
    let mut d = ntfs_card();
    put64(&mut d, 40, u64::MAX);
    c.image("total sectors", d);
    // The MFT's cluster number times the cluster size overflowed.
    let mut d = ntfs_card();
    put64(&mut d, 48, u64::MAX / 3);
    c.image("MFT cluster", d);
    // Zero sectors per cluster: the boot sector is invalid, but the volume
    // size was still divided by the cluster size (found by fuzzing).
    let mut d = ntfs_card();
    d[13] = 0;
    c.image("zero cluster size", d);
}

/// Replaces MFT record `n` of the synthetic NTFS volume.
fn ntfs_with_record(n: usize, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut d = ntfs_card();
    let at = 4 * 4096 + n * 1024;
    d[at..at + 1024].copy_from_slice(&ntfs_record(flags, attrs));
    d
}

#[test]
fn regression_ntfs_records() {
    let mut c = Cases::default();
    let name = ntfs_attr(0x30, Some(&ntfs_name("x.jpg", 5)), None);
    // Two runs whose offsets add up past i64::MAX.
    let mut runs = Vec::new();
    for _ in 0..2 {
        runs.push(0x81);
        runs.push(1);
        runs.extend(0x7FFF_FFFF_FFFF_FFF0u64.to_le_bytes());
    }
    runs.push(0);
    let d = ntfs_with_record(
        20,
        1,
        &[name.clone(), ntfs_attr(0x80, None, Some((&runs, 4096)))],
    );
    c.image("run offsets", d);
    // A deleted file with a run of 2^56 clusters: checking whether any was
    // in use counted through all of them.
    // Header 0x18: an 8-byte length, then a 1-byte offset (cluster 100).
    let mut runs = vec![0x18];
    runs.extend([0xFF; 7]);
    runs.extend([0x00, 100, 0]);
    let d = ntfs_with_record(
        20,
        0,
        &[name.clone(), ntfs_attr(0x80, None, Some((&runs, 4096)))],
    );
    c.image("huge deleted run", d);
    // A non-resident attribute only 24 bytes long: its run-list offset and
    // size fields were read past its end.
    let mut short = ntfs_attr(0x80, Some(&[0; 8]), None);
    short[8] = 1;
    let d = ntfs_with_record(20, 1, &[name, short]);
    c.image("short attribute", d);
    // A volume name of odd length.
    let d = ntfs_with_record(
        3,
        1,
        &[
            ntfs_attr(0x30, Some(&ntfs_name("$Volume", 5)), None),
            ntfs_attr(0x60, Some(b"abc"), None),
        ],
    );
    c.image("odd volume name", d);
}

#[test]
fn regression_ext4_inode_table() {
    let mut c = Cases::default();
    // A 64 KiB ext4 volume of 1 KiB blocks, one group, whose descriptor
    // puts the inode table at block 2^64 - 1: marking the table's blocks
    // in use overflowed (found by fuzzing).
    let mut d = vec![0u8; 64 << 10];
    let sb = &mut d[1024..2048];
    put32(sb, 0x00, 16); // inodes
    put32(sb, 0x04, 64); // blocks
    put32(sb, 0x14, 1); // first data block
    put32(sb, 0x20, 8192); // blocks per group
    put32(sb, 0x28, 16); // inodes per group
    put16(sb, 0x38, 0xEF53);
    put32(sb, 0x60, 0x80); // 64-bit
    put16(sb, 0xFE, 64); // descriptor size
    let desc = &mut d[2048..2112];
    put32(desc, 0x08, u32::MAX);
    put32(desc, 0x28, u32::MAX);
    c.image("inode table", d);
}

#[test]
fn regression_gpt_entries() {
    let mut c = Cases::default();
    // An entry ending at the last possible sector: (last + 1) * 512.
    let mut d = exfat_card();
    put64(&mut d, 1024 + 40, u64::MAX);
    c.image("last sector", d);
    // The entry array placed past the end of any disk.
    let mut d = exfat_card();
    put64(&mut d, 512 + 72, u64::MAX / 2);
    c.image("entry array", d);
}

#[test]
fn regression_hfsplus_sizes() {
    let mut c = Cases::default();
    // 2^32 blocks and a huge bitmap file: a 512 MB allocation.
    let mut d = hfs_card();
    put32be(&mut d, VOL + 1024 + 44, u32::MAX);
    put64be(&mut d, VOL + 1024 + 112, u64::MAX);
    c.image("block count", d);
    // A catalog claiming 2^32 nodes: each was read in turn.
    let mut d = hfs_card();
    put32be(&mut d, VOL + 2 * 4096 + 36, u32::MAX);
    c.image("node count", d);
}

#[test]
fn regression_apfs_addresses() {
    let mut c = Cases::default();
    // Object map at block 2^63: its address overflowed.
    let mut d = apfs_card();
    put64(&mut d, VOL + 160, 1 << 63);
    reseal(&mut d, VOL);
    c.image("object map address", d);
    // Checkpoint descriptors at the top of the address space, and 2^31 of
    // them: the range overflowed, then would take minutes.
    let mut d = apfs_card();
    put64(&mut d, VOL + 112, u64::MAX - 1);
    put32(&mut d, VOL + 104, 0x7FFF_FFFF);
    reseal(&mut d, VOL);
    c.image("checkpoint area", d);
}

#[test]
fn regression_exfat_cluster_count() {
    let mut c = Cases::default();
    // 2^32 - 1 clusters: a 16 GB FAT, and cluster numbers past u32.
    let mut d = exfat_card();
    put32(&mut d, VOL + 92, u32::MAX);
    c.image("cluster count", d);
}

#[test]
fn regression_fat_table_size() {
    let mut c = Cases::default();
    // A FAT32-layout boot sector claiming 2^32 sectors per FAT: the table
    // was allocated (1 TB) before finding the card too small.
    let mut d = fat16_card();
    let b = VOL;
    d[b + 16] = 1; // one FAT
    put16(&mut d, b + 17, 0);
    put16(&mut d, b + 22, 0);
    put32(&mut d, b + 36, 0x8000_0000);
    put32(&mut d, b + 32, u32::MAX);
    put16(&mut d, b + 19, 0);
    c.image("FAT size", d);
}

#[test]
fn regression_asf_object_size() {
    let mut c = Cases::default();
    // A WMV header object whose 64-bit size is near 2^64: the object walk
    // overflowed adding it to its position.
    let mut d = vec![
        0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE,
        0x6C,
    ];
    d.extend(200u64.to_le_bytes());
    d.extend(1u32.to_le_bytes());
    d.extend([1, 2]);
    d.extend([0x11; 16]);
    d.extend((u64::MAX - 8).to_le_bytes());
    d.resize(4096, 0);
    c.file("ASF object", d);
}

#[test]
fn regression_box_sizes() {
    let mut c = Cases::default();
    // A 64-bit box size near 2^64: position plus size overflowed in the
    // carver, in fragment repair and in video index rebuilding.
    let mut d = camera_mp4(10);
    let mdat = u32::from_be_bytes(d[..4].try_into().unwrap()) as usize;
    put32be(&mut d, mdat, 1);
    put64be(&mut d, mdat + 8, u64::MAX - 4);
    c.file("64-bit mdat", d);
    let mut d = bx(b"ftyp", b"isom\0\0\x02\0isomavc1");
    d.extend(1u32.to_be_bytes());
    d.extend(b"free");
    d.extend((u64::MAX - 8).to_be_bytes());
    d.resize(8192, 0);
    c.file("64-bit free", d);
}

#[test]
fn regression_zip64_offsets() {
    let mut c = Cases::default();
    // A ZIP64 locator pointing 2^64 - 1 bytes on.
    let mut d = zip_file(&["a.txt"]);
    let end = d.len() - 22;
    put32(&mut d, end + 16, u32::MAX);
    let mut loc = b"PK\x06\x07".to_vec();
    loc.extend([0; 4]);
    loc.extend(u64::MAX.to_le_bytes());
    loc.extend([1, 0, 0, 0]);
    d.splice(end..end, loc);
    c.file("zip64 locator", d);
}

#[test]
fn regression_tiff_length() {
    let mut c = Cases::default();
    // An IFD claiming 1000 entries runs past the end; carving counted them
    // into the file's length, beyond what it was allowed to read.
    let mut d = raw_tiff("NIKON CORPORATION", 3000);
    put16(&mut d, 8, 1000);
    c.file("IFD count", d);
}

#[test]
fn regression_preview_canvas() {
    let mut c = Cases::default();
    // A GIF whose header claims a 65535 x 65535 canvas: 17 GB.
    let mut d = encode(&noise_image(48, 32, 3), image::ImageFormat::Gif);
    put16(&mut d, 6, u16::MAX);
    put16(&mut d, 8, u16::MAX);
    c.file("GIF canvas", d);
    // A BMP claiming 60000 x 60000 pixels.
    let mut d = encode(&noise_image(48, 32, 3), image::ImageFormat::Bmp);
    put32(&mut d, 18, 60_000);
    put32(&mut d, 22, 60_000);
    c.file("BMP canvas", d);
    // A TIFF tag claiming 4 million values: the decoder reserved them.
    let mut d = encode(&noise_image(48, 32, 3), image::ImageFormat::Tiff);
    let ifd = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
    put32(&mut d, ifd + 2 + 4, 0x0040_0000);
    c.file("TIFF tag count", d);
}

// ---------------------------------------------------------------------------
// Lost partitions: volumes no partition table lists.

const MIB: usize = 1 << 20;
const TRACK: usize = 63 * 512;
const CYLINDER: usize = 255 * TRACK;

/// The synthetic volumes without their partition tables.
fn bare_volumes() -> Vec<(&'static str, Vec<u8>)> {
    let strip_gpt = |d: Vec<u8>| d[MIB..d.len() - 34 * 512].to_vec();
    vec![
        ("fat16", fat16_card()[MIB..].to_vec()),
        ("exfat", strip_gpt(exfat_card())),
        ("ntfs", {
            // With the copy of its boot sector in its last sector, as
            // formatters leave it.
            let mut v = ntfs_card();
            let n = v.len();
            let boot = v[..512].to_vec();
            v[n - 512..].copy_from_slice(&boot);
            v
        }),
        ("hfsplus", {
            // With the copy of its volume header 1024 bytes before its end.
            let mut v = strip_gpt(hfs_card());
            let n = v.len();
            let vh = v[1024..1536].to_vec();
            v[n - 1024..n - 512].copy_from_slice(&vh);
            v
        }),
        ("apfs", strip_gpt(apfs_card())),
    ]
}

/// A disk whose partition table is gone (all zeros), holding the volumes
/// at the given offsets.
fn lost_disk(len: usize, at: &[(usize, &[u8])]) -> Vec<u8> {
    let mut disk = vec![0u8; len];
    for &(o, v) in at {
        disk[o..o + v.len()].copy_from_slice(v);
    }
    disk
}

/// What the files of each synthetic volume are called.
fn lost_expect(name: &str) -> &'static [&'static str] {
    match name {
        "fat16" => &["DCIM/IMG_0001.JPG", "Shopping notes.txt"],
        "exfat" => &["DCIM/IMG_0001.JPG", "DCIM/MVI_0003.MP4"],
        "ntfs" => &["DCIM/IMG_0001.JPG", "notes.txt"],
        "hfsplus" | "apfs" => &["IMG_0001.JPG", "IMG_0002.JPG"],
        _ => &[],
    }
}

fn scan_lost(disk: &Vec<u8>, find_partitions: bool) -> session::ScanResult {
    let opts = ScanOptions {
        find_partitions,
        ..ScanOptions::default()
    };
    session::scan(disk, "lost", &opts, &AtomicBool::new(false), |_| {})
}

/// Checks that each volume was found at its offset, and its files read
/// with their names and intact.
fn assert_found(result: &session::ScanResult, want: &[(&str, usize)]) {
    let fs_name = |n: &str| match n {
        "fat16" => "FAT16",
        "exfat" => "exFAT",
        "ntfs" => "NTFS",
        "hfsplus" => "Mac OS Extended",
        _ => "APFS",
    };
    for &(name, offset) in want {
        let index = result
            .volumes
            .iter()
            .position(|v| v.offset == offset as u64 && v.readable)
            .unwrap_or_else(|| panic!("{name} at {offset} not found: {:?}", result.volumes));
        let v = &result.volumes[index];
        assert_eq!(v.fs, fs_name(name), "{v:?}");
        assert!(v.found.starts_with("Found partition"), "{v:?}");
        for p in lost_expect(name) {
            let item = result
                .items
                .iter()
                .find(|i| i.volume == Some(index as u32) && i.path() == *p)
                .unwrap_or_else(|| panic!("{name}: {p} not found"));
            assert!(
                matches!(item.health, Health::Good | Health::Unknown),
                "{name}: {p} {:?}",
                item.health
            );
        }
    }
}

/// Volumes at the places partitioning tools put them are found without
/// asking, after the partition table was zeroed.
#[test]
fn lost_volumes_at_usual_places_are_found() {
    let vols = bare_volumes();
    let place = [
        TRACK,
        5 * MIB,
        2 * CYLINDER + TRACK,
        24 * MIB,
        3 * CYLINDER + MIB,
    ];
    // 3 cylinders + 1 MiB is not a usual place; leave APFS at 40 MiB.
    let place = [place[0], place[1], place[2], place[3], 40 * MIB];
    let at: Vec<(usize, &[u8])> = place
        .iter()
        .zip(&vols)
        .map(|(&o, (_, v))| (o, v.as_slice()))
        .collect();
    let disk = lost_disk(48 * MIB, &at);
    let result = scan_lost(&disk, false);
    let want: Vec<(&str, usize)> = vols.iter().map(|v| v.0).zip(place).collect();
    assert_found(&result, &want);
    assert!(
        result.volumes.iter().all(|v| v.readable),
        "{:?}",
        result.volumes
    );
}

/// Volumes at odd sectors, or whose first sector is damaged, are found by
/// the search of every sector, through the copies their file systems keep.
#[test]
fn lost_volumes_at_odd_places_and_damaged_are_found() {
    let vols = bare_volumes();
    let mut ntfs = vols[2].1.clone();
    ntfs[..512].fill(0);
    let mut hfs = vols[3].1.clone();
    hfs[1024..1536].fill(0);
    let place = [
        12_345 * 512,
        MIB + 100 * 512,
        11 * MIB + 50 * 512,
        17 * MIB + 99 * 512,
        24 * MIB + 40 * 512,
    ];
    let disk = lost_disk(
        32 * MIB,
        &[
            (place[0], &vols[0].1),
            (place[1], &vols[1].1),
            (place[2], &ntfs),
            (place[3], &hfs),
            (place[4], &vols[4].1),
        ],
    );
    // Odd places, well away from where volumes usually start, are not where
    // the default search looks.
    let quick = scan_lost(&disk, false);
    assert!(
        quick.volumes.iter().all(|v| !v.readable),
        "{:?}",
        quick.volumes
    );
    let result = scan_lost(&disk, true);
    let want: Vec<(&str, usize)> = vols.iter().map(|v| v.0).zip(place).collect();
    assert_found(&result, &want);
    for (i, name) in [(2, "ntfs"), (3, "hfsplus")] {
        let v = result
            .volumes
            .iter()
            .find(|v| v.offset == place[i] as u64)
            .unwrap();
        assert!(v.found.contains("backup"), "{name}: {v:?}");
    }
}

/// A partition deleted from a table that still lists another is found in
/// the gap, and the listed one is read as before.
#[test]
fn deleted_partition_entry_is_found_in_the_gap() {
    let card = fat16_card();
    let vols = bare_volumes();
    let mut disk = card.clone();
    disk.resize(32 * MIB, 0);
    // The second partition, at 16 MiB, has lost its entry.
    disk[16 * MIB..16 * MIB + vols[1].1.len()].copy_from_slice(&vols[1].1);
    let result = scan_lost(&disk, false);
    assert_found(&result, &[("exfat", 16 * MIB)]);
    let listed = result
        .volumes
        .iter()
        .find(|v| v.offset == MIB as u64)
        .unwrap();
    assert!(listed.readable && listed.found.is_empty(), "{listed:?}");
}

#[test]
fn lost_volumes_survive_corruption() {
    let vols = bare_volumes();
    let at: Vec<(usize, &[u8])> = [TRACK, 5 * MIB, 2 * CYLINDER + TRACK, 24 * MIB, 40 * MIB]
        .iter()
        .zip(&vols)
        .map(|(&o, (_, v))| (o, v.as_slice()))
        .collect();
    let disk = lost_disk(48 * MIB, &at);
    // The search reads around every likely start; corruption matters most
    // in the structures it then opens.
    let hot = Hot::record(&disk, |src| {
        let _ = fs::lost::find(
            src,
            &[(0, src.len())],
            false,
            &[],
            &AtomicBool::new(false),
            &mut |_, _| {},
        );
    });
    let subject = Subject {
        name: "lost".into(),
        data: Arc::new(disk),
        hot,
        sealed: Vec::new(),
    };
    run_mutations(
        "lost partitions",
        &[subject],
        Target::Image { deep: false },
        cases_per_target(if cfg!(debug_assertions) { 12 } else { 150 }),
    );
}

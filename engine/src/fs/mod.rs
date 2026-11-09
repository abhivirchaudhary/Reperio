//! Filesystem-aware recovery.
//!
//! Reading the filesystem gives back what carving cannot: original file
//! names, folders and timestamps, plus every file type rather than just
//! photos and videos. It also tells us which clusters are still in use, so
//! deleted files can be rebuilt by skipping over clusters that belong to
//! files still on the card.

pub mod apfs;
pub(crate) mod custom;
mod exfat;
mod ext4;
mod fat;
mod hfsplus;
pub mod lost;
mod ntfs;
pub mod partition;
pub(crate) mod vss;

use crate::extent::Extent;
use crate::formats::DateTime;
use crate::reader::Reader;
use crate::source::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsType {
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    Ntfs,
    HfsPlus,
    Apfs,
    Ext2,
    Ext3,
    Ext4,
    /// A file system only a plug-in or custom reader knows.
    Other,
}

impl FsType {
    pub fn name(self) -> &'static str {
        match self {
            FsType::Fat12 => "FAT12",
            FsType::Fat16 => "FAT16",
            FsType::Fat32 => "FAT32",
            FsType::ExFat => "exFAT",
            FsType::Ntfs => "NTFS",
            FsType::HfsPlus => "Mac OS Extended",
            FsType::Apfs => "APFS",
            FsType::Ext2 => "ext2",
            FsType::Ext3 => "ext3",
            FsType::Ext4 => "ext4",
            FsType::Other => "Other",
        }
    }

    /// Whether Reperio can read this filesystem's directories. Other
    /// filesystems are still deep-scanned for photos and videos.
    pub fn readable(self) -> bool {
        matches!(
            self,
            FsType::Fat12
                | FsType::Fat16
                | FsType::Fat32
                | FsType::ExFat
                | FsType::Ntfs
                | FsType::HfsPlus
                | FsType::Apfs
                | FsType::Ext2
                | FsType::Ext3
                | FsType::Ext4
        )
    }
}

/// A file found in a filesystem's directories.
#[derive(Debug, Clone)]
pub struct FsFile {
    /// Folder path inside the volume, `/` separated, empty for the root.
    pub dir: String,
    pub name: String,
    pub size: u64,
    pub modified: Option<DateTime>,
    pub created: Option<DateTime>,
    pub deleted: bool,
    /// Where the file's data is, in source offsets.
    pub extents: Vec<Extent>,
    /// Part of the file's space now belongs to another file, so at least
    /// some of its content has been overwritten.
    pub overwritten: bool,
    /// Other guesses at a deleted file's layout, most likely first, for
    /// files whose cluster chain was erased: the clusters it would have if
    /// it had also gone around other deleted files, or if its recorded
    /// start was damaged on delete. Validation picks whichever decodes.
    pub alternatives: Vec<Vec<Extent>>,
}

/// A filesystem volume that has been read.
#[derive(Debug, Clone)]
pub struct Volume {
    pub fs: FsType,
    pub label: Option<String>,
    pub offset: u64,
    pub len: u64,
    pub cluster_size: u64,
    pub files: Vec<FsFile>,
    /// Unallocated space in the data area, where deleted files live.
    pub free: Vec<Extent>,
    /// For APFS, what is needed to recognise deleted files among the old
    /// directory nodes the deep scan finds in free space.
    pub apfs: Option<std::sync::Arc<apfs::Container>>,
    /// Deleted files known by name only (on NTFS, from the change journal
    /// and old directory entries), used to name what the deep scan finds.
    pub names: Vec<KnownName>,
    /// How many deleted files each of the file system's other records
    /// gave back, for the scan's details.
    pub name_sources: NameSources,
    /// Files only a snapshot of the volume still has (APFS snapshots,
    /// Windows shadow copies), newest snapshot first. Their extents are
    /// source offsets like any other file's.
    pub snapshots: Vec<SnapshotFile>,
}

/// A deleted file whose name, folder and some facts survive, but not where
/// all of its data was. A file the deep scan finds that fits exactly one of
/// these takes its name.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownName {
    pub dir: String,
    pub name: String,
    pub size: Option<u64>,
    /// Source offset of the file's first byte.
    pub start: Option<u64>,
    pub modified: Option<DateTime>,
}

/// Counts of deleted files found other than through the file table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NameSources {
    /// Files given back whole, data included, from the NTFS journal
    /// ($LogFile).
    #[serde(default)]
    pub logfile: usize,
    /// Deleted file names from the NTFS change journal ($UsnJrnl).
    #[serde(default)]
    pub journal: usize,
    /// Deleted file names from old entries left in folder indexes ($I30).
    #[serde(default)]
    pub index: usize,
    /// Files found by the deep scan that took one of those names.
    #[serde(default)]
    pub named_finds: usize,
}

impl NameSources {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A file found in a snapshot of a volume whose data the live volume no
/// longer has: deleted since the snapshot was taken, or changed.
#[derive(Debug, Clone)]
pub struct SnapshotFile {
    /// Which snapshot: its name, or when it was taken.
    pub snapshot: String,
    /// When the snapshot was taken.
    pub taken: Option<DateTime>,
    /// The file as the snapshot has it, `dir` as it was in the volume.
    pub file: FsFile,
}

/// Identifies the filesystem whose first sector is at `offset`.
pub fn detect(src: &dyn Source, offset: u64) -> Option<FsType> {
    let mut r = Reader::new(src, offset);
    let boot = r.bytes(512)?;
    match &boot[3..11] {
        b"EXFAT   " => return Some(FsType::ExFat),
        b"NTFS    " => return Some(FsType::Ntfs),
        _ => {}
    }
    if let Some(kind) = fat::detect(&boot) {
        return Some(kind);
    }
    r.seek(offset + 32);
    if r.array::<4>().as_ref() == Some(b"NXSB") {
        return Some(FsType::Apfs);
    }
    r.seek(offset + 1024);
    if matches!(r.array::<2>().as_ref(), Some(b"H+" | b"HX")) {
        return Some(FsType::HfsPlus);
    }
    r.seek(offset + 1024);
    ext4::detect(&r.bytes(1024)?)
}

/// Identifies the file system of the `len`-byte volume at `offset`, like
/// [`detect`], but when the volume's first structures are damaged also
/// recognises it by the copies file systems keep: NTFS's boot sector at
/// the volume's end, FAT32's 6 sectors in, exFAT's 12 sectors in, HFS+'s
/// volume header 1024 bytes before the end, and ext2/3/4's superblock
/// backups.
pub fn detect_volume(src: &dyn Source, offset: u64, len: u64) -> Option<FsType> {
    detect(src, offset).or_else(|| detect_by_backup(src, offset, len))
}

fn detect_by_backup(src: &dyn Source, offset: u64, len: u64) -> Option<FsType> {
    let end = offset.checked_add(len)?;
    let sector = |at: u64| {
        let mut b = [0u8; 512];
        (src.read_at(at, &mut b).ok()? == 512).then_some(b)
    };
    if sector(end.checked_sub(512)?).is_some_and(|b| &b[3..11] == b"NTFS    ") {
        return Some(FsType::Ntfs);
    }
    if fat::backup_boot(src, offset).is_some() {
        return Some(FsType::Fat32);
    }
    if exfat::backup_boot(src, offset).is_some() {
        return Some(FsType::ExFat);
    }
    if sector(end.checked_sub(1024)?).is_some_and(|b| matches!(&b[..2], b"H+" | b"HX")) {
        return Some(FsType::HfsPlus);
    }
    ext4::detect_backup(src, offset, len)
}

/// A reader of your own for a file system, added by dropping a file in
/// `src/fs/custom/` (see `docs/custom-scanners.md`). It never replaces the
/// built-in reader: what it finds is added to what that finds, and it
/// stands in alone when the built-in one gets nothing.
pub trait VolumeReader: Send + Sync {
    /// Name shown in logs.
    fn name(&self) -> &'static str;

    /// The file system this reader adds to. It runs on volumes of this type
    /// (and, when nothing recognises a volume, on any volume).
    fn fs(&self) -> FsType;

    /// Whether to run on a volume Reperio recognised as `detected` (`None`
    /// when it recognised nothing).
    fn handles(&self, detected: Option<FsType>) -> bool {
        detected == Some(self.fs())
    }

    /// Reads the volume at `offset`, `len` bytes long. Read-only. Return
    /// `None` when it is not yours or cannot be read.
    fn read(&self, src: &dyn Source, offset: u64, len: u64) -> Option<Volume>;

    /// [`read`](Self::read), told what Reperio took the volume for.
    fn read_as(
        &self,
        src: &dyn Source,
        offset: u64,
        len: u64,
        _detected: Option<FsType>,
    ) -> Option<Volume> {
        self.read(src, offset, len)
    }
}

impl Volume {
    /// An empty volume to fill in from a custom reader.
    pub fn empty(fs: FsType, offset: u64, len: u64, cluster_size: u64) -> Self {
        Self {
            fs,
            label: None,
            offset,
            len,
            cluster_size,
            files: Vec::new(),
            free: Vec::new(),
            apfs: None,
            names: Vec::new(),
            name_sources: NameSources::default(),
            snapshots: Vec::new(),
        }
    }
}

/// Runs the custom readers that apply and adds what they find to `base`.
/// A reader that panics is skipped, never taking the scan down with it.
fn with_custom(
    src: &dyn Source,
    offset: u64,
    len: u64,
    detected: Option<FsType>,
    base: Option<Volume>,
) -> Option<Volume> {
    let mut readers = custom::all();
    readers.extend(crate::plugin::readers());
    add_readers(&readers, src, offset, len, detected, base)
}

/// Volumes Reperio cannot read itself, for custom readers and plug-ins.
pub fn open_foreign(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    let cached = crate::source::Cached::new(src);
    with_custom(&cached, offset, len, detect_volume(src, offset, len), None)
}

fn add_readers(
    readers: &[Box<dyn VolumeReader>],
    src: &dyn Source,
    offset: u64,
    len: u64,
    detected: Option<FsType>,
    base: Option<Volume>,
) -> Option<Volume> {
    let mut volume = base;
    for reader in readers {
        if !reader.handles(detected) && volume.is_some() {
            continue;
        }
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reader.read_as(src, offset, len, detected)
        }));
        let Some(mut extra) = run.ok().flatten() else {
            continue;
        };
        match volume.as_mut() {
            None => {
                extra.fs = detected.unwrap_or(extra.fs);
                volume = Some(extra);
            }
            Some(v) => {
                let key = |f: &FsFile| (f.dir.clone(), f.name.clone(), f.size, f.deleted);
                let known: std::collections::HashSet<_> = v.files.iter().map(key).collect();
                v.files
                    .extend(extra.files.into_iter().filter(|f| !known.contains(&key(f))));
                for n in extra.names {
                    if !v.names.contains(&n) {
                        v.names.push(n);
                    }
                }
                v.snapshots.extend(extra.snapshots);
            }
        }
    }
    volume
}

/// Reads the directories and allocation map of the volume at `offset`.
pub fn open(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    // File systems' tables are read a node or block at a time.
    let cached = crate::source::Cached::new(src);
    let src: &dyn Source = &cached;
    let detected = detect_volume(src, offset, len);
    let built_in = detected.and_then(|t| open_built_in(src, offset, len, t));
    with_custom(src, offset, len, detected, built_in)
}

fn open_built_in(src: &dyn Source, offset: u64, len: u64, fs: FsType) -> Option<Volume> {
    match fs {
        FsType::Other => None,
        FsType::ExFat => exfat::read(src, offset, len),
        FsType::Ntfs => ntfs::read(src, offset, len).map(|mut v| {
            v.snapshots = vss::snapshot_files(src, &v);
            v
        }),
        FsType::HfsPlus => hfsplus::read(src, offset, len),
        FsType::Apfs => apfs::read(src, offset, len),
        FsType::Ext2 | FsType::Ext3 | FsType::Ext4 => ext4::read(src, offset, len),
        kind @ (FsType::Fat12 | FsType::Fat16 | FsType::Fat32) => fat::read(src, offset, len, kind),
    }
}

/// Decodes an MS-DOS date and time (local time, two-second resolution).
pub(crate) fn dos_datetime(date: u16, time: u16) -> Option<DateTime> {
    DateTime::new(
        1980 + (date >> 9),
        ((date >> 5) & 0x0F) as u8,
        (date & 0x1F) as u8,
        (time >> 11) as u8,
        ((time >> 5) & 0x3F) as u8,
        ((time & 0x1F) * 2) as u8,
    )
}

/// Builds the extents of a file from its clusters, in order, given each
/// cluster's source offset.
pub(crate) fn cluster_extents(
    clusters: impl IntoIterator<Item = u32>,
    size: u64,
    cluster_size: u64,
    offset_of: impl Fn(u32) -> u64,
) -> Vec<Extent> {
    let mut out = Vec::new();
    let mut left = size;
    for c in clusters {
        if left == 0 {
            break;
        }
        let n = left.min(cluster_size);
        crate::extent::push_merged(&mut out, Extent::new(offset_of(c), n));
        left -= n;
    }
    out
}

/// Picks the clusters of a deleted file whose cluster chain has been erased.
///
/// Starting at the file's first cluster, take the clusters it needs in
/// order, skipping clusters that are in use. A cluster in use now was
/// either in use when the file was written (so the file went around it) or
/// was reused after the delete (so that part is gone either way): skipping
/// is right in both cases. Returns the clusters and whether the first
/// cluster has been reused.
pub(crate) fn deleted_clusters(
    first: u32,
    needed: u64,
    max_cluster: u32,
    in_use: impl Fn(u32) -> bool,
) -> (Vec<u32>, bool) {
    let mut out = Vec::new();
    if first < 2 || first > max_cluster || needed == 0 {
        return (out, false);
    }
    let overwritten = in_use(first);
    out.push(first);
    let mut c = first;
    while (out.len() as u64) < needed && c < max_cluster {
        c += 1;
        if !in_use(c) {
            out.push(c);
        }
    }
    (out, overwritten)
}

/// How a deleted file's clusters were worked out, kept until every file in
/// the volume is known.
pub(crate) struct DeletedAlloc {
    pub file: usize,
    pub first: u32,
    pub needed: u64,
    pub clusters: Vec<u32>,
    /// The layout is known for certain (an exFAT contiguous file, or a
    /// complete surviving chain), so there is nothing to second-guess.
    pub certain: bool,
}

/// Adds an alternative layout for deleted files whose layout was guessed.
///
/// The first guess skips only clusters in use now. But a deleted file was
/// written around whatever existed at the time, which may since have been
/// deleted too, so the second guess also skips clusters claimed by other
/// deleted files.
pub(crate) fn add_alternatives(
    files: &mut [FsFile],
    allocs: &[DeletedAlloc],
    max_cluster: u32,
    in_use: impl Fn(u32) -> bool,
    cluster_size: u64,
    offset_of: impl Fn(u32) -> u64,
) {
    let mut claims = vec![0u16; max_cluster as usize + 1];
    for a in allocs {
        for &c in &a.clusters {
            if let Some(n) = claims.get_mut(c as usize) {
                *n = n.saturating_add(1);
            }
        }
    }
    for a in allocs {
        if a.certain || a.needed <= 1 {
            continue;
        }
        let mut own = a.clusters.clone();
        own.sort_unstable();
        let claimed = |c: u32| {
            let others = claims.get(c as usize).copied().unwrap_or(0)
                - u16::from(own.binary_search(&c).is_ok());
            others > 0
        };
        let (alt, _) =
            deleted_clusters(a.first, a.needed, max_cluster, |c| in_use(c) || claimed(c));
        if alt != a.clusters {
            let size = files[a.file].size;
            files[a.file]
                .alternatives
                .insert(0, cluster_extents(alt, size, cluster_size, &offset_of));
        }
    }
}

/// Converts a free/used flag per cluster into source extents of free space.
pub(crate) fn free_extents(
    clusters: u32,
    in_use: impl Fn(u32) -> bool,
    cluster_size: u64,
    offset_of: impl Fn(u32) -> u64,
) -> Vec<Extent> {
    let mut out = Vec::new();
    for c in 2..clusters + 2 {
        if !in_use(c) {
            crate::extent::push_merged(&mut out, Extent::new(offset_of(c), cluster_size));
        }
    }
    out
}

/// Trash folders macOS, Windows and Linux keep on removable disks.
pub fn in_trash(dir: &str) -> bool {
    dir.split('/').next().is_some_and(|top| {
        top == ".Trashes"
            || top == "$RECYCLE.BIN"
            || top == "RECYCLER"
            || top.starts_with(".Trash-")
    })
}

/// Joins a directory path and a name.
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod custom_tests {
    use super::*;

    struct Fake(&'static str, bool);

    impl VolumeReader for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn fs(&self) -> FsType {
            FsType::Ntfs
        }
        fn read(&self, _: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
            assert!(!self.1, "a bug in a custom reader");
            let mut v = Volume::empty(FsType::Ntfs, offset, len, 4096);
            v.files.push(FsFile {
                dir: String::new(),
                name: self.0.into(),
                size: 10,
                modified: None,
                created: None,
                deleted: true,
                extents: vec![Extent::new(offset, 10)],
                overwritten: false,
                alternatives: Vec::new(),
            });
            Some(v)
        }
    }

    fn run(readers: Vec<Box<dyn VolumeReader>>, base: Option<Volume>) -> Option<Volume> {
        let data = vec![0u8; 4096];
        add_readers(&readers, &data, 0, 4096, Some(FsType::Ntfs), base)
    }

    #[test]
    fn custom_files_are_added_once_and_a_panic_is_skipped() {
        let mut base = Volume::empty(FsType::Ntfs, 0, 4096, 4096);
        let same = run(vec![Box::new(Fake("a.txt", false))], None)
            .unwrap()
            .files;
        base.files = same;
        let v = run(
            vec![
                Box::new(Fake("a.txt", false)),
                Box::new(Fake("b.txt", false)),
                Box::new(Fake("c.txt", true)),
            ],
            Some(base),
        )
        .unwrap();
        let names: Vec<_> = v.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
    }

    #[test]
    fn a_custom_reader_stands_in_when_nothing_else_reads_the_volume() {
        let v = run(vec![Box::new(Fake("x.txt", false))], None).unwrap();
        assert_eq!(v.files.len(), 1);
        assert!(run(vec![Box::new(Fake("x.txt", true))], None).is_none());
    }
}

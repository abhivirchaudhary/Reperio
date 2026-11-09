//! Deleted NTFS files whose own records have been reused.
//!
//! Windows reuses a deleted file's record within minutes on a busy drive,
//! and with it go the file's name, folder and cluster list. Three other
//! places keep some of that for longer, used here best first:
//!
//! 1. The journal ($LogFile) holds whole record images: a file comes back
//!    with its data, as long as every cluster it lists is still free.
//! 2. The change journal ($UsnJrnl) names every file deleted while it
//!    was kept, with its folder and when.
//! 3. Old entries in folder indexes name deleted files with their folder,
//!    size and dates.
//!
//! Names without data from 2 and 3 are handed to the scan, which gives
//! them to files the deep scan finds when exactly one fits (see
//! `session::name_from_volumes`). Folders whose records were reused are
//! named from all three, so paths come back too.

use std::collections::{HashMap, HashSet};

use super::{
    index, logfile, mapped_extents, name_of, parse_record, stream_map, usn, FolderNames, Paths,
    Record, REF_MASK,
};
use crate::extent::{Extent, MappedSource};
use crate::formats::DateTime;
use crate::fs::{FsFile, KnownName, NameSources};
use crate::source::{read_tolerant, Source};

/// What the volume reader knows that this needs.
pub(super) struct Context<'a> {
    pub src: &'a dyn Source,
    pub cluster: u64,
    pub record_size: u64,
    pub total_clusters: u64,
    pub lcn_offset: &'a dyn Fn(u64) -> u64,
    pub in_use: &'a dyn Fn(u64) -> bool,
}

/// Everything read from the three places.
#[derive(Default)]
pub(super) struct Found {
    images: Vec<logfile::Image>,
    /// Deletions from the change journal.
    deletions: Vec<usn::Change>,
    /// The latest change journal record of each folder.
    folders: HashMap<u64, usn::Change>,
    slack: Vec<index::Entry>,
}

/// What came back.
pub(super) struct Recovered {
    pub files: Vec<FsFile>,
    pub names: Vec<KnownName>,
    pub sources: NameSources,
}

/// Most of the change journal read: its newest part.
const MAX_JOURNAL: u64 = 256 << 20;
/// Bytes of the change journal read at a time.
const JOURNAL_CHUNK: usize = 1 << 20;
/// Most deletions kept from the change journal.
const MAX_DELETIONS: usize = 1_000_000;
/// Most bytes of folder indexes read for their slack.
const MAX_INDEX_BYTES: u64 = 256 << 20;
/// Most old index entries kept.
const MAX_SLACK: usize = 1_000_000;
/// Record numbers of the journal and of `$Extend`, the change journal's
/// folder.
const LOGFILE: u64 = 2;
/// Largest file, in clusters, whose clusters are checked for being free
/// (64 GB at 4 KB clusters).
const MAX_CHECKED_CLUSTERS: u64 = 1 << 24;
const EXTEND: u64 = 11;

pub(super) fn gather(ctx: &Context, records: &HashMap<u64, Record>) -> Found {
    let mut found = Found::default();
    if let Some(rec) = records.get(&LOGFILE) {
        found.images = logfile::images(
            ctx.src,
            stream_extents(ctx, &rec.data, rec.size),
            ctx.cluster,
            ctx.record_size,
        );
    }
    let journal = records.values().find(|r| {
        !r.journal.is_empty()
            && r.names
                .iter()
                .any(|(n, p, _)| n == "$UsnJrnl" && p & REF_MASK == EXTEND)
    });
    if let Some(rec) = journal {
        read_journal(ctx, rec, &mut found);
    }
    read_slack(ctx, records, &mut found.slack);
    found
}

/// Where a stream's bytes are on the source, as one run from its start:
/// a hole (a piece in a lost record) ends it, since what follows would
/// be read at the wrong place.
fn stream_extents(ctx: &Context, data: &[(u64, super::Runs)], size: Option<u64>) -> Vec<Extent> {
    mapped_extents(data, size.unwrap_or(0), ctx.cluster, ctx.lcn_offset)
}

fn read_journal(ctx: &Context, rec: &Record, found: &mut Found) {
    let size = rec.journal_size.unwrap_or(0);
    // Only the tail holds records; the rest is sparse and so unmapped.
    let parts: Vec<(u64, u64, u64)> = stream_map(&rec.journal, ctx.cluster, ctx.lcn_offset)
        .into_iter()
        .filter(|&(vpos, _, _)| vpos < size)
        .map(|(vpos, disk, len)| (vpos, disk, len.min(size - vpos)))
        .collect();
    let mut skip = parts
        .iter()
        .map(|p| p.2)
        .sum::<u64>()
        .saturating_sub(MAX_JOURNAL);
    let mut carry: Vec<u8> = Vec::new();
    let mut last_end = None;
    let mut changes = Vec::new();
    for (vpos, disk, len) in parts {
        // The oldest part, beyond what is read, is skipped.
        let s = skip.min(len);
        skip -= s;
        let (vpos, disk, len) = (vpos + s, disk + s, len - s);
        if len == 0 {
            continue;
        }
        if last_end != Some(vpos) {
            carry.clear();
        }
        let mut off = 0;
        while off < len {
            let n = (len - off).min(JOURNAL_CHUNK as u64) as usize;
            let mut buf = std::mem::take(&mut carry);
            let have = buf.len();
            buf.resize(have + n, 0);
            let (got, _) = read_tolerant(ctx.src, disk + off, &mut buf[have..]);
            buf.truncate(have + got);
            let used = usn::parse(&buf, &mut changes);
            // Only a record cut off by the chunk's end is carried over.
            if buf.len() - used <= 4096 {
                carry = buf[used..].to_vec();
            }
            for c in changes.drain(..) {
                if c.directory {
                    found.folders.insert(c.file, c);
                } else if c.reason & usn::FILE_DELETE != 0 && found.deletions.len() < MAX_DELETIONS
                {
                    found.deletions.push(c);
                }
            }
            if got < n {
                break;
            }
            off += n as u64;
        }
        last_end = Some(vpos + len);
    }
}

/// Old entries in every folder's index.
fn read_slack(ctx: &Context, records: &HashMap<u64, Record>, out: &mut Vec<index::Entry>) {
    let mut numbers: Vec<u64> = records
        .iter()
        .filter(|(_, r)| r.directory)
        .map(|(&n, _)| n)
        .collect();
    numbers.sort_unstable();
    let mut budget = MAX_INDEX_BYTES;
    for n in numbers {
        if out.len() >= MAX_SLACK {
            break;
        }
        let rec = &records[&n];
        let Some(root) = &rec.index_root else {
            continue;
        };
        index::root_slack(root, n, out);
        let block = root
            .get(8..12)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as u64)
            .filter(|b| b.is_power_of_two() && (512..=65536).contains(b))
            .unwrap_or(4096);
        for (vpos, disk, len) in stream_map(&rec.index_blocks, ctx.cluster, ctx.lcn_offset) {
            // Blocks start at multiples of the block size in the stream.
            let first = vpos.next_multiple_of(block) - vpos;
            let mut at = first;
            while at + block <= len && budget >= block {
                let mut b = vec![0u8; block as usize];
                let (got, _) = read_tolerant(ctx.src, disk + at, &mut b);
                budget -= block;
                if got == b.len() {
                    index::block_slack(b, n, out);
                }
                at += block;
            }
        }
    }
}

/// Names of deleted folders, from all three places.
pub(super) fn folder_names(records: &HashMap<u64, Record>, found: &Found) -> FolderNames {
    let mut out = FolderNames::new();
    let mut add = |file: u64, name: &str, parent: u64| {
        let (num, seq) = (file & REF_MASK, (file >> 48) as u16);
        let v = out.entry(num).or_default();
        if !v.iter().any(|(s, _, _)| *s == seq) {
            v.push((seq, name.to_string(), parent));
        }
    };
    for c in found.folders.values() {
        add(c.file, &c.name, c.parent);
    }
    for img in &found.images {
        if let Some(rec) = parse_record(&img.bytes, img.number, 0).filter(|r| r.directory) {
            if let Some((name, parent)) = name_of(&rec) {
                add(img.number | (u64::from(rec.seq) << 48), &name, parent);
            }
        }
    }
    for e in found
        .slack
        .iter()
        .filter(|e| e.directory && e.namespace != 2)
    {
        if let Some(file) = e.file {
            add(file, &e.name, e.parent);
        }
    }
    // Folders still in the file table need nothing more.
    out.retain(|n, _| records.get(n).is_none_or(|r| !r.directory));
    out
}

/// A deleted file known by name, being put together from several places.
struct Name {
    parent: u64,
    name: String,
    size: Option<u64>,
    start: Option<u64>,
    modified: Option<DateTime>,
    journal: bool,
    index: bool,
}

pub(super) fn recover(
    ctx: &Context,
    records: &HashMap<u64, Record>,
    found: &Found,
    paths: &mut Paths,
    files: &[FsFile],
) -> Recovered {
    // A record still in use with the same sequence count is the same file,
    // still there.
    let alive = |file: u64| {
        let (num, seq) = (file & REF_MASK, (file >> 48) as u16);
        records.get(&num).is_some_and(|r| r.in_use && r.seq == seq)
    };
    // A record that still holds the name is read the usual way.
    let listed = |num: u64, name: &str| {
        records
            .get(&num)
            .is_some_and(|r| r.names.iter().any(|(n, _, _)| n == name))
    };
    // Every name in the file table, by folder.
    let taken: HashSet<(u64, String)> = records
        .values()
        .flat_map(|r| {
            r.names
                .iter()
                .map(|(n, p, _)| (p & REF_MASK, n.to_lowercase()))
        })
        .collect();
    let mut starts: HashSet<u64> = files
        .iter()
        .filter_map(|f| f.extents.first().map(|e| e.offset))
        .collect();

    let mut out = Recovered {
        files: Vec::new(),
        names: Vec::new(),
        sources: NameSources::default(),
    };
    // Deleted files known by name, by folder number and name.
    let mut names: HashMap<(u64, String), Name> = HashMap::new();
    let mut whole: HashSet<(u64, String)> = HashSet::new();

    // 1. Record images from the journal, newest first.
    let mut images: Vec<&logfile::Image> = found.images.iter().collect();
    images.sort_by_key(|i| std::cmp::Reverse(i.lsn));
    let mut seen: HashSet<u64> = HashSet::new();
    for img in images {
        let Some(rec) = parse_record(&img.bytes, img.number, 0) else {
            continue;
        };
        let Some((name, parent)) = name_of(&rec) else {
            continue;
        };
        let file = img.number | (u64::from(rec.seq) << 48);
        if rec.directory
            || alive(file)
            || listed(img.number, &name)
            || rec.unrecoverable
            || !seen.insert(file)
        {
            continue;
        }
        let Some(size) = rec.size.filter(|&s| s > 0) else {
            continue;
        };
        let key = (parent & REF_MASK, name.to_lowercase());
        let extents = match (rec.resident_at, &img.map) {
            (Some((at, len)), Some(map)) => {
                let e = MappedSource::new(ctx.src, map.clone()).physical(at, len);
                (crate::extent::total_len(&e) == len).then_some(e)
            }
            (Some(_), None) => None,
            (None, _) => {
                let e = mapped_extents(&rec.data, size, ctx.cluster, ctx.lcn_offset);
                let runs = super::collect_runs(&rec.data);
                // Checking each cluster is cheap, but a corrupt run list
                // can claim billions; files that big are not looked at.
                let clusters = runs.iter().fold(0u64, |t, r| t.saturating_add(r.1));
                let free = clusters <= MAX_CHECKED_CLUSTERS
                    && runs.iter().all(|&(lcn, n)| {
                        lcn.is_some_and(|l| {
                            l.checked_add(n)
                                .is_some_and(|end| end <= ctx.total_clusters)
                                && !(l..l + n).any(ctx.in_use)
                        })
                    });
                (free && crate::extent::total_len(&e) == size).then_some(e)
            }
        };
        match extents {
            Some(extents) if !starts.contains(&extents[0].offset) => {
                starts.insert(extents[0].offset);
                whole.insert(key);
                out.files.push(FsFile {
                    dir: paths.dir(parent),
                    name,
                    size,
                    modified: rec.modified,
                    created: rec.created,
                    deleted: true,
                    extents,
                    overwritten: false,
                    alternatives: Vec::new(),
                });
                out.sources.logfile += 1;
            }
            Some(_) => {}
            None => {
                // Not all there, but where it began and how long it was
                // still help name what the deep scan finds.
                let first = rec.data.iter().min_by_key(|(vcn, _)| *vcn);
                let start = first
                    .filter(|(vcn, _)| *vcn == 0)
                    .and_then(|(_, runs)| runs.first().copied())
                    .and_then(|(lcn, _)| lcn)
                    .filter(|&l| l < ctx.total_clusters && !(ctx.in_use)(l))
                    .map(ctx.lcn_offset);
                names.entry(key).or_insert(Name {
                    parent,
                    name,
                    size: Some(size),
                    start,
                    modified: rec.modified,
                    journal: false,
                    index: false,
                });
            }
        }
    }

    // 2. Deletions in the change journal, newest first.
    let mut deletions: Vec<&usn::Change> = found.deletions.iter().collect();
    deletions.sort_by_key(|c| std::cmp::Reverse(c.time));
    for c in deletions {
        let key = (c.parent & REF_MASK, c.name.to_lowercase());
        if alive(c.file) || listed(c.file & REF_MASK, &c.name) || whole.contains(&key) {
            continue;
        }
        let n = names.entry(key).or_insert_with(|| Name {
            parent: c.parent,
            name: c.name.clone(),
            size: None,
            start: None,
            modified: None,
            journal: false,
            index: false,
        });
        n.journal = true;
    }

    // 3. Old index entries.
    for e in found
        .slack
        .iter()
        .filter(|e| !e.directory && e.namespace != 2)
    {
        let key = (e.parent & REF_MASK, e.name.to_lowercase());
        let gone = match e.file {
            Some(f) => !alive(f) && !listed(f & REF_MASK, &e.name),
            // Without the file's reference, only a name the file table
            // does not hold at all can be one deleted.
            None => !taken.contains(&key),
        };
        if !gone || whole.contains(&key) {
            continue;
        }
        let n = names.entry(key).or_insert_with(|| Name {
            parent: e.parent,
            name: e.name.clone(),
            size: None,
            start: None,
            modified: None,
            journal: false,
            index: false,
        });
        n.index = true;
        if n.size.is_none_or(|s| s == 0) && e.size > 0 {
            n.size = Some(e.size);
        }
        n.modified = n.modified.or(e.modified);
    }

    let mut names: Vec<Name> = names.into_values().collect();
    names.sort_by(|a, b| (a.parent & REF_MASK, &a.name).cmp(&(b.parent & REF_MASK, &b.name)));
    for n in names {
        out.sources.journal += usize::from(n.journal);
        out.sources.index += usize::from(n.index);
        out.names.push(KnownName {
            dir: paths.dir(n.parent),
            name: n.name,
            size: n.size,
            start: n.start,
            modified: n.modified,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::index::tests::{entry_bytes, indx_block, key_bytes, TICKS};
    use super::super::logfile::tests::{
        file_name, journal, non_resident, record, resident, TestOp,
    };
    use super::super::usn::tests::v2;
    use crate::extent::MappedSource;
    use crate::source::Source;

    const CLUSTER: usize = 4096;
    const CLUSTERS: usize = 1024;
    const MFT: usize = 4;

    /// An attribute with a name, resident (`value`) or not (`runs`).
    fn named(typ: u32, name: &str, value: Option<&[u8]>, runs: Option<(&[u8], u64)>) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let head = if value.is_some() { 24 } else { 0x40 };
        let body_at = (head + units.len()).next_multiple_of(8);
        let body: &[u8] = value.or(runs.map(|r| r.0)).unwrap();
        let len = (body_at + body.len()).next_multiple_of(8);
        let mut a = vec![0u8; len];
        a[..4].copy_from_slice(&typ.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = (units.len() / 2) as u8;
        a[10..12].copy_from_slice(&(head as u16).to_le_bytes());
        a[head..head + units.len()].copy_from_slice(&units);
        a[body_at..body_at + body.len()].copy_from_slice(body);
        match runs {
            None => {
                a[16..20].copy_from_slice(&(body.len() as u32).to_le_bytes());
                a[20..22].copy_from_slice(&(body_at as u16).to_le_bytes());
            }
            Some((_, size)) => {
                a[8] = 1;
                a[0x20..0x22].copy_from_slice(&(body_at as u16).to_le_bytes());
                for at in [0x28, 0x30, 0x38] {
                    a[at..at + 8].copy_from_slice(&size.to_le_bytes());
                }
            }
        }
        a
    }

    /// A record image as written to disk: padded, fixups applied.
    fn on_disk(mut r: Vec<u8>) -> Vec<u8> {
        r.resize(1024, 0);
        r[0x30..0x32].copy_from_slice(&[0x44, 0x00]);
        for i in 1..=2 {
            let end = i * 512;
            r[0x30 + i * 2] = r[end - 2];
            r[0x31 + i * 2] = r[end - 1];
            r[end - 2..end].copy_from_slice(&[0x44, 0x00]);
        }
        r
    }

    fn name(parent: u64, n: &str) -> Vec<u8> {
        resident(0x30, &file_name(parent, n))
    }

    /// A volume where four deleted files' records have since been
    /// reused: the journal holds images of two, the change journal names
    /// their deleted folder and a third file, and a folder's index slack
    /// names a fourth.
    fn volume() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut vol = vec![0u8; CLUSTERS * CLUSTER];
        let b = &mut vol[..512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 8;
        b[40..48].copy_from_slice(&((CLUSTERS * 8 - 1) as u64).to_le_bytes());
        b[48..56].copy_from_slice(&(MFT as u64).to_le_bytes());
        b[56..64].copy_from_slice(&2u64.to_le_bytes());
        b[64] = 0xF6;
        b[510] = 0x55;
        b[511] = 0xAA;

        let victim: Vec<u8> = (0..40_000u32).map(|i| (i * 31 % 251) as u8).collect();
        vol[600 * CLUSTER..600 * CLUSTER + victim.len()].copy_from_slice(&victim);
        let small: Vec<u8> = (0..700u32).map(|i| b'A' + (i % 23) as u8).collect();
        let holiday = 21 | (1 << 48);
        let root = 5 | (5 << 48);

        // The journal: both files' records as they were when freed. Record
        // n of 1 KB in 4 KB clusters is cluster n / 4, block (n % 4) * 2.
        let victim_img = record(
            20,
            1,
            1,
            &[
                name(holiday, "victim.bin"),
                non_resident(&[0x21, 10, 0x58, 0x02, 0], victim.len() as u64, 9),
            ],
        );
        let small_img = record(
            22,
            1,
            1,
            &[name(holiday, "small.txt"), resident(0x80, &small)],
        );
        let freed = |undo, n: u64| TestOp {
            redo_op: 3,
            undo_op: 2,
            redo: &[],
            undo,
            record_off: 0,
            attr_off: 0,
            vcn: n / 4,
            block: ((n % 4) * 2) as u16,
        };
        let log = journal(&[freed(&victim_img, 20), freed(&small_img, 22)], 16, true);
        vol[300 * CLUSTER..300 * CLUSTER + log.len()].copy_from_slice(&log);

        // The change journal's tail, after 16 sparse clusters.
        let mut j = v2(holiday, root, TICKS, 0x100, true, "Holiday");
        j.extend(v2(24 | (1 << 48), root, TICKS, 0x200, false, "gone.jpg"));
        j.extend(v2(
            20 | (1 << 48),
            holiday,
            TICKS,
            0x200,
            false,
            "victim.bin",
        ));
        vol[500 * CLUSTER..500 * CLUSTER + j.len()].copy_from_slice(&j);

        // Docs' index block: a live entry, then a removed one in slack.
        let docs = 16 | (1 << 48);
        let live = entry_bytes(
            25 | (1 << 48),
            &key_bytes(docs, "keep.txt", 3, TICKS, false),
        );
        let gone = entry_bytes(
            23 | (1 << 48),
            &key_bytes(docs, "report.zip", 5000, TICKS, false),
        );
        let block = indx_block(&live, &gone, 0);
        vol[400 * CLUSTER..401 * CLUSTER].copy_from_slice(&block);
        // Its root: just the end marker, pointing at that block.
        let mut root_value = vec![0u8; 0x30];
        root_value[0] = 0x30;
        root_value[4] = 1;
        root_value[8..12].copy_from_slice(&4096u32.to_le_bytes());
        root_value[12] = 1;
        root_value[0x10] = 0x10;
        root_value[0x14] = 0x20;
        root_value[0x18] = 0x20;
        root_value[0x1C] = 1;
        root_value[0x28] = 0x10;
        root_value[0x2C] = 2;

        let mut bitmap = vec![0u8; CLUSTERS / 8];
        for c in (0..13).chain(300..316).chain(400..401).chain(500..501) {
            bitmap[c / 8] |= 1 << (c % 8);
        }
        vol[12 * CLUSTER..12 * CLUSTER + bitmap.len()].copy_from_slice(&bitmap);

        let mut records = vec![Vec::new(); 32];
        let file = |n: u32, seq, flags, attrs: &[Vec<u8>]| on_disk(record(n, seq, flags, attrs));
        records[0] = file(
            0,
            1,
            1,
            &[
                name(root, "$MFT"),
                non_resident(&[0x11, 8, MFT as u8, 0], 32 * 1024, 7),
            ],
        );
        records[2] = file(
            2,
            1,
            1,
            &[
                name(root, "$LogFile"),
                non_resident(&[0x21, 16, 0x2C, 0x01, 0], log.len() as u64, 15),
            ],
        );
        records[5] = file(5, 5, 3, &[name(root, ".")]);
        records[6] = file(
            6,
            1,
            1,
            &[
                name(root, "$Bitmap"),
                non_resident(&[0x11, 1, 12, 0], bitmap.len() as u64, 0),
            ],
        );
        records[11] = file(11, 1, 3, &[name(root, "$Extend")]);
        records[16] = file(
            16,
            1,
            3,
            &[
                name(root, "Docs"),
                named(0x90, "$I30", Some(&root_value), None),
                named(0xA0, "$I30", None, Some((&[0x21, 1, 0x90, 0x01, 0], 4096))),
            ],
        );
        records[17] = file(
            17,
            1,
            1,
            &[
                name(11 | (1 << 48), "$UsnJrnl"),
                named(
                    0x80,
                    "$J",
                    None,
                    Some((&[0x01, 16, 0x21, 1, 0xF4, 0x01, 0], 17 * 4096)),
                ),
            ],
        );
        // The deleted files' records, reused by new small files.
        for (n, new) in [
            (20, "new.txt"),
            (21, "other.txt"),
            (22, "x.txt"),
            (23, "y.txt"),
        ] {
            records[n as usize] = file(n, 2, 1, &[name(root, new), resident(0x80, b"new")]);
        }
        records[25] = file(25, 1, 1, &[name(docs, "keep.txt"), resident(0x80, b"abc")]);
        for (i, r) in records.iter().enumerate().filter(|(_, r)| !r.is_empty()) {
            let at = MFT * CLUSTER + i * 1024;
            vol[at..at + 1024].copy_from_slice(r);
        }
        (vol, victim, small)
    }

    fn contents(src: &dyn Source, f: &crate::fs::FsFile) -> Vec<u8> {
        let view = MappedSource::new(src, f.extents.clone());
        let mut out = vec![0u8; view.len() as usize];
        assert_eq!(view.read_at(0, &mut out).unwrap(), out.len());
        out
    }

    #[test]
    fn deleted_files_come_back_from_journals_and_index_slack() {
        let (vol, victim, small) = volume();
        let v = crate::fs::open(&vol, 0, vol.len() as u64).unwrap();
        let find = |n: &str| v.files.iter().find(|f| f.name == n);
        let got = find("victim.bin").expect("victim.bin from the journal");
        assert_eq!((got.dir.as_str(), got.deleted), ("Holiday", true));
        assert_eq!(contents(&vol, got), victim);
        let got = find("small.txt").expect("small.txt from the journal");
        assert_eq!(got.dir, "Holiday");
        assert_eq!(contents(&vol, got), small);
        let names: Vec<(&str, &str, Option<u64>)> = v
            .names
            .iter()
            .map(|n| (n.dir.as_str(), n.name.as_str(), n.size))
            .collect();
        assert_eq!(
            names,
            vec![("", "gone.jpg", None), ("Docs", "report.zip", Some(5000))]
        );
        assert_eq!(
            v.name_sources,
            crate::fs::NameSources {
                logfile: 2,
                journal: 1,
                index: 1,
                named_finds: 0
            }
        );
        // Live files are untouched; nothing is listed twice.
        assert!(find("keep.txt").is_some_and(|f| !f.deleted));
        assert_eq!(v.files.iter().filter(|f| f.name == "victim.bin").count(), 1);
    }

    /// Damage anywhere in those structures never panics or hangs the
    /// reader.
    #[test]
    fn survives_damaged_volumes() {
        let (good, _, _) = volume();
        let hot: Vec<usize> = [MFT * CLUSTER, 300 * CLUSTER, 400 * CLUSTER, 500 * CLUSTER]
            .into_iter()
            .flat_map(|s| s..s + 32 * 1024)
            .collect();
        let mut rng = 0x0123_4567_89ab_cdefu64;
        for _ in 0..300 {
            let mut b = good.clone();
            for _ in 0..1 + rng % 40 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let at = hot[(rng % hot.len() as u64) as usize];
                b[at] = (rng >> 40) as u8;
            }
            if let Some(v) = crate::fs::open(&b, 0, b.len() as u64) {
                for f in &v.files {
                    let _ = MappedSource::new(&b, f.extents.clone()).read_at(0, &mut [0u8; 64]);
                }
            }
        }
    }
}

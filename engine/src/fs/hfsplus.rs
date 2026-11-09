//! Mac OS Extended (HFS+), the format of Mac-formatted cards and drives
//! before APFS, still offered by Disk Utility as "Mac OS Extended
//! (Journaled)".
//!
//! Every file and folder is a record in the catalog, a B-tree. Deleting a
//! file removes its record, and the records after it in the node close up
//! over it, but the bytes they leave behind at the end of the node are not
//! cleared. Nodes the tree no longer needs keep their old contents too. And
//! on a journaled volume, the journal holds copies of catalog nodes from
//! recent changes, so a deleted file's record usually survives there,
//! complete with name, folder, dates and where its data is.
//!
//! Reperio reads the live catalog, then looks for file records everywhere
//! else they can survive: unused nodes, the leftover bytes at the end of
//! nodes, and every node copy in the journal.

use std::collections::{HashMap, HashSet};

use super::{free_extents, FsFile, FsType, Volume};
use crate::extent::{push_merged, Extent, MappedSource};
use crate::formats::DateTime;
use crate::source::Source;

const ROOT: u32 = 2;
/// File and folder IDs below this belong to the volume's own structures.
const FIRST_USER_ID: u32 = 16;
const CATALOG_FILE: u32 = 4;
const JOURNALED: u32 = 0x2000;
const LEAF: u8 = 0xFF;
const HFS_EPOCH: i64 = 2_082_844_800;
const MAX_DEPTH: usize = 64;

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn date(secs: u32) -> Option<DateTime> {
    DateTime::from_unix(i64::from(secs) - HFS_EPOCH)
}

/// A fork's size and its first eight extents, in allocation blocks.
#[derive(Debug, Clone)]
struct Fork {
    size: u64,
    blocks: u32,
    extents: Vec<(u32, u32)>,
}

impl Fork {
    fn parse(b: &[u8]) -> Self {
        let extents = (0..8)
            .map(|i| (be32(b, 16 + i * 8), be32(b, 20 + i * 8)))
            .take_while(|&(_, n)| n > 0)
            .collect();
        Self {
            size: be64(b, 0),
            blocks: be32(b, 12),
            extents,
        }
    }
}

#[derive(Debug, Clone)]
enum Record {
    Folder {
        parent: u32,
        name: String,
        id: u32,
    },
    File {
        parent: u32,
        name: String,
        id: u32,
        created: u32,
        modified: u32,
        data: Fork,
    },
    Thread {
        id: u32,
        parent: u32,
        name: String,
    },
}

/// Decodes a name. HFS+ stores names decomposed ("e" then a combining
/// accent); everything else, including the names people type to search,
/// uses the composed form.
fn utf16be(b: &[u8]) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    let units: Vec<u16> = b.chunks_exact(2).map(|c| be16(c, 0)).collect();
    Some(String::from_utf16(&units).ok()?.nfc().collect())
}

/// Overflow extents of one fork: (first block within the fork, extents).
type Overflow = Vec<(u32, Vec<(u32, u32)>)>;

struct Hfs<'a> {
    src: &'a dyn Source,
    offset: u64,
    block: u64,
    blocks: u32,
    bitmap: Vec<u8>,
    /// Extents beyond the first eight, by file ID and fork type, from the
    /// extents overflow file.
    overflow: HashMap<(u32, u8), Overflow>,
}

impl<'a> Hfs<'a> {
    fn in_use(&self, b: u32) -> bool {
        self.bitmap
            .get(b as usize / 8)
            .is_none_or(|x| x & (0x80 >> (b % 8)) != 0)
    }

    /// Whether a record's extents lie inside the volume.
    fn plausible(&self, fork: &Fork) -> bool {
        let listed: u64 = fork.extents.iter().map(|&(_, n)| u64::from(n)).sum();
        fork.extents
            .iter()
            .all(|&(s, n)| u64::from(s) + u64::from(n) <= u64::from(self.blocks))
            && listed <= u64::from(fork.blocks)
            && fork.size <= u64::from(fork.blocks) * self.block
            && (fork.size == 0 || !fork.extents.is_empty())
    }

    /// All of a live fork's extents, including any in the overflow file.
    fn all_extents(&self, id: u32, fork_type: u8, fork: &Fork) -> Vec<(u32, u32)> {
        let mut out = fork.extents.clone();
        if let Some(more) = self.overflow.get(&(id, fork_type)) {
            // Counted in 64 bits: corrupt extent lengths can sum past u32.
            let mut have: u64 = out.iter().map(|&(_, n)| u64::from(n)).sum();
            for (start, list) in more {
                if u64::from(*start) != have {
                    break;
                }
                out.extend(list);
                have += list.iter().map(|&(_, n)| u64::from(n)).sum::<u64>();
            }
        }
        out
    }

    fn to_extents(&self, blocks: &[(u32, u32)], size: u64) -> Vec<Extent> {
        let mut out = Vec::new();
        let mut left = size;
        for &(s, n) in blocks {
            if left == 0 {
                break;
            }
            let len = (u64::from(n) * self.block).min(left);
            push_merged(
                &mut out,
                Extent::new(self.offset + u64::from(s) * self.block, len),
            );
            left -= len;
        }
        out
    }

    fn fork_source(&self, blocks: &[(u32, u32)], size: u64) -> MappedSource<'a> {
        MappedSource::new(self.src, self.to_extents(blocks, size))
    }

    /// The blocks of a deleted file. Its record lists its extents, so this
    /// is exact unless it had more than eight: then the rest is taken from
    /// the free blocks that follow, as for a FAT file with no chain.
    fn deleted_blocks(&self, fork: &Fork) -> (Vec<(u32, u32)>, bool) {
        let mut list = fork.extents.clone();
        let needed = fork.size.div_ceil(self.block);
        let mut have: u64 = list.iter().map(|&(_, n)| u64::from(n)).sum();
        if have > needed {
            // Space reserved beyond the end of the file.
            let mut keep = Vec::new();
            let mut left = needed;
            for (s, n) in list {
                let take = u64::from(n).min(left) as u32;
                if take > 0 {
                    keep.push((s, take));
                }
                left -= u64::from(take);
            }
            list = keep;
            have = needed;
        }
        if have < needed {
            let mut b = list.last().map_or(0, |&(s, n)| s + n);
            while have < needed && b < self.blocks {
                if !self.in_use(b) {
                    match list.last_mut() {
                        Some((s, n)) if *s + *n == b => *n += 1,
                        _ => list.push((b, 1)),
                    }
                    have += 1;
                }
                b += 1;
            }
        }
        let overwritten = list
            .iter()
            .any(|&(s, n)| (s..s + n).any(|b| self.in_use(b)));
        (list, overwritten)
    }
}

/// B-tree header fields Reperio needs.
struct Tree {
    node_size: usize,
    first_leaf: u32,
    total: u32,
}

fn tree_header(src: &dyn Source) -> Option<Tree> {
    let mut h = [0u8; 46];
    if src.read_at(0, &mut h).ok()? < h.len() || h[8] != 1 {
        return None;
    }
    let node_size = be16(&h, 32) as usize;
    if !(512..=32768).contains(&node_size) || !node_size.is_power_of_two() {
        return None;
    }
    // Every node is read in turn, so the count must not exceed what the
    // file holds: a corrupt header could otherwise mean 4 billion reads.
    let fits = (src.len() / node_size as u64).min(u64::from(u32::MAX)) as u32;
    Some(Tree {
        node_size,
        first_leaf: be32(&h, 24),
        total: be32(&h, 36).min(fits),
    })
}

fn read_node(src: &dyn Source, size: usize, n: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; size];
    let got = src.read_at(u64::from(n) * size as u64, &mut buf).ok()?;
    (got == size).then_some(buf)
}

/// The records of a node, by its offset table. Returns nothing unless the
/// node is a well-formed leaf.
fn leaf_records(node: &[u8]) -> Vec<usize> {
    let size = node.len();
    if node.len() < 14 || node[8] != LEAF || node[9] != 1 {
        return Vec::new();
    }
    let n = be16(node, 10) as usize;
    if n == 0 || 14 + n * 2 > size {
        return Vec::new();
    }
    let offsets: Vec<usize> = (0..=n)
        .map(|i| be16(node, size - 2 * (i + 1)) as usize)
        .collect();
    let ok = offsets[0] == 14
        && offsets.windows(2).all(|w| w[0] < w[1])
        && offsets[n] <= size - 2 * (n + 1);
    if ok {
        offsets[..n].to_vec()
    } else {
        Vec::new()
    }
}

/// Parses a catalog record at `at`. Strict, because it is also run on
/// leftover bytes at every even offset.
fn parse_record(node: &[u8], at: usize) -> Option<Record> {
    let key_len = be16(node, at) as usize;
    if !(6..=516).contains(&key_len) || at + 2 + key_len + 2 > node.len() {
        return None;
    }
    let parent = be32(node, at + 2);
    let name_len = be16(node, at + 6) as usize;
    if key_len != 6 + 2 * name_len || parent == 0 {
        return None;
    }
    let name = utf16be(&node[at + 8..at + 8 + 2 * name_len])?;
    let d = at + 2 + key_len;
    let rest = &node[d..];
    match be16(rest, 0) {
        1 if rest.len() >= 88 && name_len > 0 => {
            let id = be32(rest, 8);
            (id >= ROOT).then_some(Record::Folder { parent, name, id })
        }
        2 if rest.len() >= 248 && name_len > 0 => {
            let id = be32(rest, 8);
            if id < FIRST_USER_ID || be32(rest, 4) != 0 {
                return None;
            }
            Some(Record::File {
                parent,
                name,
                id,
                created: be32(rest, 12),
                modified: be32(rest, 16),
                data: Fork::parse(&rest[88..168]),
            })
        }
        3 | 4 if name_len == 0 && rest.len() >= 10 && be16(rest, 2) == 0 => {
            let n = be16(rest, 8) as usize;
            if n == 0 || n > 255 || rest.len() < 10 + 2 * n {
                return None;
            }
            Some(Record::Thread {
                id: parent,
                parent: be32(rest, 4),
                name: utf16be(&rest[10..10 + 2 * n])?,
            })
        }
        _ => None,
    }
}

/// Every record that can be found anywhere in a leaf-shaped node,
/// including in the bytes left over at its end.
fn scan_node(node: &[u8], out: &mut Vec<Record>) {
    if node.len() < 14 || node[8] != LEAF || node[9] != 1 {
        return;
    }
    let mut at = 14;
    while at + 8 < node.len() {
        if let Some(r) = parse_record(node, at) {
            out.push(r);
        }
        at += 2;
    }
}

pub fn read(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    // The volume header, or its copy 1024 bytes before the volume's end
    // when the first is damaged.
    let header = |at: u64| {
        let mut vh = [0u8; 512];
        (src.read_at(at, &mut vh).ok()? == 512 && matches!(&vh[..2], b"H+" | b"HX")).then_some(vh)
    };
    let vh = header(offset.checked_add(1024)?)
        .or_else(|| header(offset.checked_add(len)?.checked_sub(1024)?))?;
    let attributes = be32(&vh, 4);
    let journal_info = be32(&vh, 12);
    let block = u64::from(be32(&vh, 40));
    if !(512..=1 << 20).contains(&block) || !block.is_power_of_two() {
        return None;
    }
    // The header's block count sizes the allocation bitmap and the free
    // space walk; no more blocks exist than the volume (or a truncated
    // image of it) holds. Two are kept clear of u32::MAX for
    // free_extents's cluster numbering.
    let real = len.min(src.len().saturating_sub(offset)) / block;
    let blocks = be32(&vh, 44).min(real.min(u64::from(u32::MAX - 2)) as u32);
    if blocks == 0 {
        return None;
    }
    let alloc = Fork::parse(&vh[112..192]);
    let ext = Fork::parse(&vh[192..272]);
    let cat = Fork::parse(&vh[272..352]);

    let mut hfs = Hfs {
        src,
        offset,
        block,
        blocks,
        bitmap: Vec::new(),
        overflow: HashMap::new(),
    };
    let bitmap_src = hfs.fork_source(&alloc.extents, alloc.size);
    let mut bitmap = vec![0u8; (u64::from(blocks).div_ceil(8)).min(alloc.size) as usize];
    bitmap_src.read_at(0, &mut bitmap).ok()?;
    hfs.bitmap = bitmap;

    // The extents overflow file: extents of fragmented files past the
    // eighth, including the catalog's own if it has grown in pieces.
    let ext_src = hfs.fork_source(&ext.extents, ext.size);
    if let Some(t) = tree_header(&ext_src) {
        let mut n = t.first_leaf;
        let mut seen = HashSet::new();
        while n != 0 && n < t.total && seen.insert(n) {
            let Some(node) = read_node(&ext_src, t.node_size, n) else {
                break;
            };
            for at in leaf_records(&node) {
                if at + 12 + 64 > node.len() || be16(&node, at) != 10 {
                    continue;
                }
                let fork_type = node[at + 2];
                let id = be32(&node, at + 4);
                let start = be32(&node, at + 8);
                let list = (0..8)
                    .map(|i| (be32(&node, at + 12 + i * 8), be32(&node, at + 16 + i * 8)))
                    .take_while(|&(_, c)| c > 0)
                    .collect();
                hfs.overflow
                    .entry((id, fork_type))
                    .or_default()
                    .push((start, list));
            }
            n = be32(&node, 0);
        }
        for list in hfs.overflow.values_mut() {
            list.sort_by_key(|(s, _)| *s);
        }
    }

    let cat_blocks = hfs.all_extents(CATALOG_FILE, 0, &cat);
    let cat_src = hfs.fork_source(&cat_blocks, cat.size);
    let tree = tree_header(&cat_src)?;

    // The live catalog: the chain of leaf nodes.
    let mut live = Vec::new();
    let mut live_nodes = HashSet::new();
    let mut n = tree.first_leaf;
    while n != 0 && n < tree.total && live_nodes.insert(n) {
        let Some(node) = read_node(&cat_src, tree.node_size, n) else {
            break;
        };
        let offsets = leaf_records(&node);
        if offsets.is_empty() {
            break;
        }
        live.extend(offsets.iter().filter_map(|&at| parse_record(&node, at)));
        n = be32(&node, 0);
    }

    // Everywhere else records survive: every node of the catalog (unused
    // nodes and the leftover end of nodes in use), then the journal.
    let mut old = Vec::new();
    for n in 0..tree.total {
        if let Some(node) = read_node(&cat_src, tree.node_size, n) {
            scan_node(&node, &mut old);
        }
    }
    if attributes & JOURNALED != 0 && journal_info != 0 {
        let mut jib = [0u8; 52];
        if src
            .read_at(offset + u64::from(journal_info) * block, &mut jib)
            .is_ok()
        {
            let (j_off, j_len) = (be64(&jib, 36), be64(&jib, 44));
            let in_volume = j_off.checked_add(j_len).is_some_and(|e| e <= len);
            if be32(&jib, 0) & 1 != 0 && in_volume {
                scan_journal(src, offset + j_off, j_len, tree.node_size, &mut old);
            }
        }
    }

    let mut folders: HashMap<u32, (u32, String)> = HashMap::new();
    let mut label = None;
    let mut live_ids = HashSet::new();
    for r in &live {
        match r {
            Record::Folder { parent, name, id } => {
                folders.insert(*id, (*parent, name.clone()));
                if *id == ROOT {
                    label = Some(name.clone());
                }
            }
            Record::Thread { id, parent, name } => {
                folders.entry(*id).or_insert((*parent, name.clone()));
                live_ids.insert(*id);
            }
            Record::File { id, .. } => {
                live_ids.insert(*id);
            }
        }
    }
    // A deleted folder can have several old records: in its place, then
    // in the Trash. Prefer where it was.
    let mut trash: HashSet<u32> = HashSet::new();
    for r in &live {
        if let Record::Folder { parent, name, id } = r {
            if *parent == ROOT && name == ".Trashes" {
                trash.insert(*id);
            }
        }
    }
    for r in &live {
        if let Record::Folder { parent, id, .. } = r {
            if trash.contains(parent) {
                trash.insert(*id);
            }
        }
    }
    let known: HashSet<u32> = folders.keys().copied().collect();
    for r in &old {
        if let Record::Folder { parent, name, id } = r {
            if known.contains(id) {
                continue;
            }
            match folders.get(id) {
                Some((was, _)) if !trash.contains(was) || trash.contains(parent) => {}
                _ => {
                    folders.insert(*id, (*parent, name.clone()));
                }
            }
        }
    }
    let path_of = |mut id: u32| {
        let mut parts = Vec::new();
        while id != ROOT && parts.len() < MAX_DEPTH {
            match folders.get(&id) {
                Some((parent, name)) => {
                    parts.push(name.clone());
                    id = *parent;
                }
                None => {
                    parts.push(format!("Folder {id}"));
                    break;
                }
            }
        }
        parts.reverse();
        parts.join("/")
    };

    let mut files = Vec::new();
    for r in &live {
        if let Record::File {
            parent,
            name,
            id,
            created,
            modified,
            data,
        } = r
        {
            let blocks = hfs.all_extents(*id, 0, data);
            files.push(FsFile {
                dir: path_of(*parent),
                name: name.clone(),
                size: data.size,
                modified: date(*modified),
                created: date(*created),
                deleted: false,
                extents: hfs.to_extents(&blocks, data.size),
                overwritten: false,
                alternatives: Vec::new(),
            });
        }
    }

    // One entry per deleted file, preferring the copy of its record from
    // before it went into the Trash.
    let mut best: HashMap<u32, (bool, &Record)> = HashMap::new();
    for r in &old {
        let Record::File {
            parent, id, data, ..
        } = r
        else {
            continue;
        };
        if live_ids.contains(id) || data.size == 0 || !hfs.plausible(data) {
            continue;
        }
        let trash = super::in_trash(&path_of(*parent));
        match best.get(id) {
            Some((was_trash, _)) if *was_trash <= trash => {}
            _ => {
                best.insert(*id, (trash, r));
            }
        }
    }
    let mut deleted: Vec<&Record> = best.into_values().map(|(_, r)| r).collect();
    deleted.sort_by_key(|r| match r {
        Record::File { id, .. } => *id,
        _ => 0,
    });
    for r in deleted {
        let Record::File {
            parent,
            name,
            created,
            modified,
            data,
            ..
        } = r
        else {
            continue;
        };
        let (blocks, overwritten) = hfs.deleted_blocks(data);
        files.push(FsFile {
            dir: path_of(*parent),
            name: name.clone(),
            size: data.size,
            modified: date(*modified),
            created: date(*created),
            deleted: true,
            extents: hfs.to_extents(&blocks, data.size),
            overwritten,
            alternatives: Vec::new(),
        });
    }

    let free = free_extents(
        blocks,
        |c| hfs.in_use(c - 2),
        block,
        |c| offset + u64::from(c - 2) * block,
    );
    Some(Volume {
        fs: FsType::HfsPlus,
        label,
        offset,
        len,
        cluster_size: block,
        files,
        free,
        apfs: None,
        names: Vec::new(),
        name_sources: Default::default(),
        snapshots: Vec::new(),
    })
}

/// Finds copies of catalog leaf nodes in the journal. They sit at
/// sector-aligned positions inside its circular buffer.
fn scan_journal(src: &dyn Source, start: u64, len: u64, node_size: usize, out: &mut Vec<Record>) {
    const CHUNK: usize = 8 << 20;
    let mut buf = vec![0u8; CHUNK + node_size];
    let mut pos = 0;
    while pos < len {
        let want = ((len - pos) as usize).min(buf.len());
        let Ok(n) = src.read_at(start + pos, &mut buf[..want]) else {
            break;
        };
        if n < node_size {
            break;
        }
        let mut at = 0;
        while at + node_size <= n && at < CHUNK {
            let node = &buf[at..at + node_size];
            if node[8] == LEAF && node[9] == 1 && be16(node, 10) > 0 {
                scan_node(node, out);
            }
            at += 512;
        }
        pos += CHUNK as u64;
    }
}

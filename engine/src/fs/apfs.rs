//! APFS, the format of Macs since 2017 and of cards and drives formatted
//! on one with Disk Utility's default settings.
//!
//! APFS never overwrites its own records in place. Every change writes new
//! copies of the B-tree nodes it touches to free space and lets go of the
//! old ones. So when a file is deleted, the nodes that still list it (its
//! name, folder, dates, size and where its data is) stay where they were,
//! in what is now free space, until something else is written there.
//!
//! Reperio reads the live volumes, which also tells it every block in use,
//! then looks through free space for old copies of directory nodes. That
//! search runs as part of the deep scan, which reads free space anyway, so
//! it costs no extra pass over the card.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{free_extents, join_path, FsFile, FsType, SnapshotFile, Volume};
use crate::extent::{push_merged, Extent};
use crate::formats::DateTime;
use crate::source::Source;

const NX_MAGIC: &[u8; 4] = b"NXSB";
const APFS_MAGIC: &[u8; 4] = b"APSB";
const TYPE_NX: u32 = 0x01;
const TYPE_BTREE: u32 = 0x02;
const TYPE_NODE: u32 = 0x03;
const TYPE_FSTREE: u32 = 0x0E;
const PHYSICAL: u32 = 0x4000_0000;
const NODE_ROOT: u16 = 1;
const NODE_LEAF: u16 = 2;
const NODE_FIXED: u16 = 4;
const OMAP_DELETED: u32 = 1;
const FS_UNENCRYPTED: u64 = 1;
const J_SNAP_METADATA: u8 = 1;
const J_INODE: u8 = 3;
const J_EXTENT: u8 = 8;
const J_DREC: u8 = 9;
const XF_NAME: u8 = 4;
const XF_DSTREAM: u8 = 8;
const S_IFMT: u16 = 0o170_000;
const S_IFREG: u16 = 0o100_000;
const DT_DIR: u16 = 4;
const DT_REG: u16 = 8;
const ROOT_DIR: u64 = 2;
const MAX_DEPTH: usize = 64;
const MAX_TREE_DEPTH: usize = 16;
/// Most snapshots read per volume. macOS keeps a few dozen at most; a
/// corrupt tree could claim any number.
const MAX_SNAPSHOTS: usize = 100;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The Fletcher-64 checksum every APFS object starts with.
fn checksum_ok(obj: &[u8]) -> bool {
    const M: u64 = 0xFFFF_FFFF;
    let (mut s1, mut s2) = (0u64, 0u64);
    for w in obj[8..].chunks_exact(4) {
        s1 = (s1 + u64::from(le32(w, 0))) % M;
        s2 = (s2 + s1) % M;
    }
    let c1 = M - (s1 + s2) % M;
    let c2 = M - (s1 + c1) % M;
    le64(obj, 0) == (c2 << 32) | c1
}

fn obj_type(obj: &[u8]) -> u32 {
    le32(obj, 24) & 0xFFFF
}

fn time(ns: u64) -> Option<DateTime> {
    DateTime::from_unix((ns / 1_000_000_000) as i64)
}

/// The key/value pairs of a B-tree node. `fixed` gives the key and value
/// sizes of trees with fixed-size entries.
fn entries(node: &[u8], fixed: Option<(usize, usize)>) -> Option<Vec<(&[u8], &[u8])>> {
    let size = node.len();
    let flags = le16(node, 32);
    let count = le32(node, 36) as usize;
    let toc = 56 + le16(node, 40) as usize;
    let keys = toc + le16(node, 42) as usize;
    let vals = if flags & NODE_ROOT != 0 {
        size - 40
    } else {
        size
    };
    if keys > vals || count > size / 4 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let (k, kl, v, vl) = if flags & NODE_FIXED != 0 {
            let (kl, vl) = fixed?;
            let at = toc + i * 4;
            if at + 4 > keys {
                return None;
            }
            let vl = if flags & NODE_LEAF != 0 { vl } else { 8 };
            (le16(node, at) as usize, kl, le16(node, at + 2) as usize, vl)
        } else {
            let at = toc + i * 8;
            if at + 8 > keys {
                return None;
            }
            (
                le16(node, at) as usize,
                le16(node, at + 2) as usize,
                le16(node, at + 4) as usize,
                le16(node, at + 6) as usize,
            )
        };
        if v == 0xFFFF {
            continue;
        }
        let key = node.get(keys + k..keys + k + kl)?;
        let start = vals.checked_sub(v)?;
        let val = node.get(start..start + vl)?;
        out.push((key, val));
    }
    Some(out)
}

#[derive(Debug, Clone, Default)]
struct Inode {
    parent: u64,
    private: u64,
    created: u64,
    modified: u64,
    mode: u16,
    name: Option<String>,
    size: Option<u64>,
}

#[derive(Debug, Clone)]
enum Rec {
    Inode(u64, Inode),
    Drec {
        parent: u64,
        name: String,
        id: u64,
        kind: u16,
    },
    Extent {
        owner: u64,
        logical: u64,
        len: u64,
        block: u64,
    },
}

fn parse_inode(v: &[u8]) -> Option<Inode> {
    if v.len() < 92 {
        return None;
    }
    let mut inode = Inode {
        parent: le64(v, 0),
        private: le64(v, 8),
        created: le64(v, 16),
        modified: le64(v, 24),
        mode: le16(v, 80),
        ..Default::default()
    };
    // Extended fields: a count, a table of (type, flags, size), then the
    // data, each item padded to 8 bytes.
    if v.len() >= 96 {
        let n = le16(v, 92) as usize;
        let mut data = 96 + n * 4;
        for i in 0..n {
            let at = 96 + i * 4;
            if at + 4 > v.len() {
                break;
            }
            let (kind, size) = (v[at], le16(v, at + 2) as usize);
            let Some(item) = v.get(data..data + size) else {
                break;
            };
            match kind {
                XF_NAME => {
                    let end = item.iter().position(|&b| b == 0).unwrap_or(item.len());
                    inode.name = std::str::from_utf8(&item[..end]).ok().map(str::to_string);
                }
                XF_DSTREAM if size >= 8 => inode.size = Some(le64(item, 0)),
                _ => {}
            }
            data += size.div_ceil(8) * 8;
        }
    }
    Some(inode)
}

fn parse_record(k: &[u8], v: &[u8]) -> Option<Rec> {
    if k.len() < 8 {
        return None;
    }
    let hdr = le64(k, 0);
    let id = hdr & 0x0FFF_FFFF_FFFF_FFFF;
    match (hdr >> 60) as u8 {
        J_INODE => Some(Rec::Inode(id, parse_inode(v)?)),
        J_DREC if v.len() >= 18 => {
            // Hashed keys (every Mac since 2017) and the older plain form.
            let name = if k.len() >= 12 && k.len() == 12 + (le32(k, 8) & 0x3FF) as usize {
                &k[12..]
            } else if k.len() >= 10 && k.len() == 10 + le16(k, 8) as usize {
                &k[10..]
            } else {
                return None;
            };
            let name = name.strip_suffix(&[0]).unwrap_or(name);
            Some(Rec::Drec {
                parent: id,
                name: std::str::from_utf8(name).ok()?.to_string(),
                id: le64(v, 0),
                kind: le16(v, 16) & 0xF,
            })
        }
        J_EXTENT if k.len() >= 16 && v.len() >= 16 => Some(Rec::Extent {
            owner: id,
            logical: le64(k, 8),
            len: le64(v, 0) & 0x00FF_FFFF_FFFF_FFFF,
            block: le64(v, 8),
        }),
        _ => None,
    }
}

/// Records gathered from a volume's directory tree.
#[derive(Debug, Default)]
struct Records {
    inodes: HashMap<u64, Inode>,
    drecs: Vec<(u64, String, u64, u16)>,
    extents: HashMap<u64, Vec<(u64, u64, u64)>>,
}

impl Records {
    fn add(&mut self, r: Rec) {
        match r {
            Rec::Inode(id, i) => {
                self.inodes.insert(id, i);
            }
            Rec::Drec {
                parent,
                name,
                id,
                kind,
            } => self.drecs.push((parent, name, id, kind)),
            Rec::Extent {
                owner,
                logical,
                len,
                block,
            } => self
                .extents
                .entry(owner)
                .or_default()
                .push((logical, len, block)),
        }
    }
}

/// Old records found in free space, newest version of each kept.
#[derive(Debug, Default)]
pub struct OldNodes {
    inodes: HashMap<u64, (u64, Inode)>,
    drecs: HashMap<(u64, String), (u64, u64, u16)>,
    extents: HashMap<(u64, u64), (u64, u64, u64)>,
    pub nodes: usize,
}

#[derive(Debug)]
struct LiveVolume {
    name: String,
    ids: HashSet<u64>,
    dirs: HashMap<u64, (u64, String)>,
}

/// What a container's live state says, kept to recognise deleted files
/// among old nodes the deep scan finds.
#[derive(Debug)]
pub struct Container {
    offset: u64,
    len: u64,
    pub block: u64,
    blocks: u64,
    volumes: Vec<LiveVolume>,
    used: Vec<u64>,
    /// Where live files start, with their sizes: a deleted clone of one of
    /// them shares its data and is not really gone.
    live_starts: HashSet<(u64, u64)>,
}

impl Container {
    /// Whether a source offset is inside this container.
    pub fn contains(&self, at: u64) -> bool {
        (self.offset..self.offset.saturating_add(self.len)).contains(&at)
    }

    fn in_use(&self, b: u64) -> bool {
        self.used
            .get((b / 64) as usize)
            .is_none_or(|w| w & (1 << (b % 64)) != 0)
    }

    /// Whether `node` (one block, from a container this deep scan covers) is
    /// an old directory-tree leaf; if so its records are kept.
    pub fn collect(node: &[u8], into: &mut OldNodes) -> bool {
        if node.len() < 4096
            || !matches!(obj_type(node), TYPE_BTREE | TYPE_NODE)
            || le32(node, 28) != TYPE_FSTREE
            || le16(node, 32) & NODE_LEAF == 0
            || !checksum_ok(node)
        {
            return false;
        }
        let xid = le64(node, 16);
        let Some(list) = entries(node, None) else {
            return false;
        };
        into.nodes += 1;
        for (k, v) in list {
            match parse_record(k, v) {
                Some(Rec::Inode(id, inode)) => {
                    if into.inodes.get(&id).is_none_or(|(x, _)| *x < xid) {
                        into.inodes.insert(id, (xid, inode));
                    }
                }
                Some(Rec::Drec {
                    parent,
                    name,
                    id,
                    kind,
                }) => {
                    let e = into.drecs.entry((parent, name)).or_insert((0, id, kind));
                    if e.0 < xid {
                        *e = (xid, id, kind);
                    }
                }
                Some(Rec::Extent {
                    owner,
                    logical,
                    len,
                    block,
                }) => {
                    let e = into
                        .extents
                        .entry((owner, logical))
                        .or_insert((0, len, block));
                    if e.0 < xid {
                        *e = (xid, len, block);
                    }
                }
                None => {}
            }
        }
        true
    }

    /// The deleted files among old records.
    pub fn deleted(&self, old: &OldNodes) -> Vec<FsFile> {
        let live: HashSet<u64> = self
            .volumes
            .iter()
            .flat_map(|v| v.ids.iter().copied())
            .collect();
        let mut dirs: HashMap<u64, (u64, String)> = HashMap::new();
        for v in self.volumes.iter().rev() {
            dirs.extend(v.dirs.iter().map(|(k, d)| (*k, d.clone())));
        }
        // The Trash and the per-user folders in it, live or not (a card
        // that has been erased since has no live folders at all).
        let trash: HashSet<u64> = {
            let all_dirs = || {
                dirs.iter().map(|(id, (p, n))| (*id, *p, n.as_str())).chain(
                    old.drecs
                        .iter()
                        .filter(|(_, v)| v.2 == DT_DIR)
                        .map(|((p, n), v)| (v.1, *p, n.as_str())),
                )
            };
            let top: HashSet<u64> = all_dirs()
                .filter(|&(_, p, n)| p == ROOT_DIR && n == ".Trashes")
                .map(|(id, _, _)| id)
                .collect();
            all_dirs()
                .filter(|(id, p, _)| top.contains(id) || top.contains(p))
                .map(|(id, _, _)| id)
                .collect()
        };
        // Folders deleted along with the files, preferring where they were
        // over the Trash.
        let mut old_dirs: HashMap<u64, (u64, String)> = HashMap::new();
        for ((parent, name), &(_, id, kind)) in &old.drecs {
            if kind != DT_DIR || dirs.contains_key(&id) {
                continue;
            }
            match old_dirs.get(&id) {
                Some((was, _)) if !trash.contains(was) || trash.contains(parent) => {}
                _ => {
                    old_dirs.insert(id, (*parent, name.clone()));
                }
            }
        }
        dirs.extend(old_dirs);
        let path_of = |mut id: u64| {
            let mut parts = Vec::new();
            while id != ROOT_DIR && parts.len() < MAX_DEPTH {
                match dirs.get(&id) {
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

        let mut names: HashMap<u64, (u64, &str)> = HashMap::new();
        for ((parent, name), &(_, id, kind)) in &old.drecs {
            if kind != DT_REG || live.contains(&id) {
                continue;
            }
            match names.get(&id) {
                Some((was, _)) if !trash.contains(was) || trash.contains(parent) => {}
                _ => {
                    names.insert(id, (*parent, name));
                }
            }
        }
        let mut by_owner: HashMap<u64, Vec<(u64, u64, u64)>> = HashMap::new();
        for (&(owner, logical), &(_, len, block)) in &old.extents {
            by_owner
                .entry(owner)
                .or_default()
                .push((logical, len, block));
        }

        let mut ids: Vec<&u64> = old.inodes.keys().collect();
        ids.sort_unstable();
        let mut out = Vec::new();
        for id in ids {
            let (_, inode) = &old.inodes[id];
            if live.contains(id) || inode.mode & S_IFMT != S_IFREG {
                continue;
            }
            let Some(size) = inode.size.filter(|&s| s > 0) else {
                continue;
            };
            let Some(mut list) = by_owner.get(&inode.private).cloned() else {
                continue;
            };
            list.sort_unstable();
            let extents = self.extents(&list, size);
            if extents.is_empty() || self.live_starts.contains(&(extents[0].offset, size)) {
                continue;
            }
            let (parent, name) = match names.get(id) {
                Some(&(p, n)) => (p, n.to_string()),
                None => match &inode.name {
                    Some(n) => (inode.parent, n.clone()),
                    None => continue,
                },
            };
            let overwritten = list.iter().any(|&(_, len, block)| {
                // Old records are not checked against the container size;
                // past its end every block counts as in use anyway.
                let end = block.saturating_add(len.div_ceil(self.block));
                end > self.blocks || (block..end).any(|b| self.in_use(b))
            });
            out.push(FsFile {
                dir: self.prefix(path_of(parent)),
                name,
                size,
                modified: time(inode.modified),
                created: time(inode.created),
                deleted: true,
                extents,
                overwritten,
                alternatives: Vec::new(),
            });
        }
        out
    }

    /// Source extents of a file's data, from its (logical, len, block)
    /// extent records sorted by position in the file.
    fn extents(&self, list: &[(u64, u64, u64)], size: u64) -> Vec<Extent> {
        let mut out = Vec::new();
        let mut pos = 0;
        for &(logical, len, block) in list {
            if logical != pos || pos >= size {
                break;
            }
            if block.saturating_add(len.div_ceil(self.block)) > self.blocks {
                break;
            }
            let take = len.min(size - pos);
            push_merged(
                &mut out,
                Extent::new(self.offset + block * self.block, take),
            );
            pos += len;
        }
        out
    }

    fn prefix(&self, dir: String) -> String {
        // Containers with several volumes (a Mac's internal drive) list
        // each volume as a folder.
        if self.volumes.len() > 1 {
            join_path(&self.volumes[0].name, &dir)
        } else {
            dir
        }
    }
}

struct Reader<'a> {
    src: &'a dyn Source,
    offset: u64,
    block: u64,
    blocks: u64,
    used: Vec<u64>,
}

impl Reader<'_> {
    fn block(&mut self, n: u64) -> Option<Vec<u8>> {
        // Block numbers come from the data; one outside the container is
        // not there, rather than an overflow.
        if n >= self.blocks {
            return None;
        }
        let mut buf = vec![0u8; self.block as usize];
        let got = self
            .src
            .read_at(self.offset + n * self.block, &mut buf)
            .ok()?;
        (got == buf.len() && checksum_ok(&buf)).then_some(buf)
    }

    fn mark(&mut self, start: u64, count: u64) {
        for b in start..start.saturating_add(count) {
            if let Some(w) = self.used.get_mut((b / 64) as usize) {
                *w |= 1 << (b % 64);
            } else {
                break;
            }
        }
    }

    /// Loads an object map: virtual object ID to (transaction, block).
    fn omap(&mut self, at: u64) -> HashMap<u64, Vec<(u64, u64)>> {
        let mut map: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();
        let Some(om) = self.block(at) else {
            return map;
        };
        self.mark(at, 1);
        let mut stack = vec![(le64(&om, 48), 0)];
        while let Some((n, depth)) = stack.pop() {
            if depth > MAX_TREE_DEPTH {
                continue;
            }
            let Some(node) = self.block(n) else { continue };
            self.mark(n, 1);
            let leaf = le16(&node, 32) & NODE_LEAF != 0;
            let Some(list) = entries(&node, Some((16, 16))) else {
                continue;
            };
            for (k, v) in list {
                // A node not flagged fixed-size gives its own entry sizes,
                // which may be too short for an object map entry.
                if k.len() < 16 || v.len() < if leaf { 16 } else { 8 } {
                    continue;
                }
                if leaf {
                    if le32(v, 0) & OMAP_DELETED == 0 {
                        map.entry(le64(k, 0))
                            .or_default()
                            .push((le64(k, 8), le64(v, 8)));
                    }
                } else {
                    stack.push((le64(v, 0), depth + 1));
                }
            }
        }
        map
    }

    /// Walks a directory tree, marking its nodes in use.
    fn fs_tree(
        &mut self,
        root: u64,
        physical: bool,
        omap: &HashMap<u64, Vec<(u64, u64)>>,
        xid: u64,
        out: &mut Records,
    ) {
        self.walk(root, physical, omap, xid, |k, v| {
            if let Some(r) = parse_record(k, v) {
                out.add(r);
            }
        });
    }

    /// Walks a B-tree, marking its nodes in use and handing each leaf
    /// entry to `leaf`. A virtual tree's nodes are found through `omap`
    /// as they were at transaction `xid`.
    fn walk(
        &mut self,
        root: u64,
        physical: bool,
        omap: &HashMap<u64, Vec<(u64, u64)>>,
        xid: u64,
        mut leaf: impl FnMut(&[u8], &[u8]),
    ) {
        let resolve = |oid: u64| {
            if physical {
                return Some(oid);
            }
            omap.get(&oid)?
                .iter()
                .filter(|(x, _)| *x <= xid)
                .max_by_key(|(x, _)| *x)
                .map(|(_, b)| *b)
        };
        let mut stack = vec![(root, 0)];
        let mut seen = HashSet::new();
        while let Some((oid, depth)) = stack.pop() {
            let Some(n) = resolve(oid) else { continue };
            if depth > MAX_TREE_DEPTH || !seen.insert(n) {
                continue;
            }
            let Some(node) = self.block(n) else { continue };
            self.mark(n, 1);
            let is_leaf = le16(&node, 32) & NODE_LEAF != 0;
            let Some(list) = entries(&node, None) else {
                continue;
            };
            for (k, v) in list {
                if is_leaf {
                    leaf(k, v);
                } else if v.len() >= 8 {
                    stack.push((le64(v, 0), depth + 1));
                }
            }
        }
    }
}

/// A snapshot of a volume, from its snapshot metadata tree.
#[derive(Debug)]
struct Snap {
    xid: u64,
    /// The volume superblock as it was when the snapshot was taken.
    sblock: u64,
    /// Its extent reference tree, which says which blocks it holds.
    extentref: u64,
    created: u64,
    name: String,
}

impl Snap {
    /// How the snapshot is shown: its name (Time Machine's include the
    /// date), or the date it was taken.
    fn label(&self) -> String {
        let name = self.name.replace('/', "_");
        if !name.trim().is_empty() {
            return name;
        }
        match time(self.created) {
            Some(t) => format!("Snapshot {t}"),
            None => format!("Snapshot {}", self.xid),
        }
    }
}

/// Parses a snapshot metadata record (`j_snap_metadata_val_t`).
fn parse_snap(k: &[u8], v: &[u8]) -> Option<Snap> {
    if k.len() < 8 || (le64(k, 0) >> 60) as u8 != J_SNAP_METADATA || v.len() < 50 {
        return None;
    }
    let len = usize::from(le16(v, 48));
    let raw = v.get(50..50 + len).unwrap_or(&v[50..]);
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(Snap {
        xid: le64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF,
        extentref: le64(v, 0),
        sblock: le64(v, 8),
        created: le64(v, 16),
        name: String::from_utf8_lossy(&raw[..end]).into_owned(),
    })
}

/// Reads a volume's snapshots, newest first, each with its directory
/// tree, marking everything they hold in use. `sb` is the live volume
/// superblock and `vol_omap` its object map, which keeps the versions of
/// tree nodes that snapshots still need.
fn snapshots(
    r: &mut Reader,
    sb: &[u8],
    vol_omap: &HashMap<u64, Vec<(u64, u64)>>,
    xid: u64,
) -> Vec<(Snap, Records)> {
    let root = le64(sb, 152);
    if root == 0 {
        return Vec::new();
    }
    let mut snaps = Vec::new();
    let physical = le32(sb, 124) & PHYSICAL != 0;
    r.walk(root, physical, vol_omap, xid, |k, v| {
        if snaps.len() < MAX_SNAPSHOTS {
            snaps.extend(parse_snap(k, v));
        }
    });
    snaps.sort_by_key(|s| std::cmp::Reverse(s.xid));
    let mut out = Vec::new();
    for snap in snaps {
        // A snapshot from after the live state is a corrupt record.
        if snap.xid > xid {
            continue;
        }
        let Some(ssb) = r.block(snap.sblock) else {
            continue;
        };
        if &ssb[32..36] != APFS_MAGIC {
            continue;
        }
        r.mark(snap.sblock, 1);
        // The extent reference tree is the snapshot's own; its nodes must
        // not count as free.
        if le32(&ssb, 120) & PHYSICAL != 0 && snap.extentref != 0 {
            r.walk(snap.extentref, true, vol_omap, snap.xid, |_, _| {});
        }
        let tree = le64(&ssb, 136);
        let physical = le32(&ssb, 116) & PHYSICAL != 0;
        let mut recs = Records::default();
        r.fs_tree(tree, physical, vol_omap, snap.xid, &mut recs);
        if recs.drecs.is_empty() && !physical {
            // The live object map should keep the snapshot's nodes; if it
            // does not, the one the snapshot recorded may.
            let own = r.omap(le64(&ssb, 128));
            r.fs_tree(tree, false, &own, snap.xid, &mut recs);
        }
        if !recs.drecs.is_empty() {
            out.push((snap, recs));
        }
    }
    out
}

/// A volume's (or a snapshot's) regular files, as (folder, name, size,
/// inode, extent records), marking their blocks in use; and its folders
/// by ID. `prefix` is the volume's name in a container with several.
#[allow(clippy::type_complexity)]
fn listing(
    recs: &Records,
    prefix: Option<&str>,
    block: u64,
    r: &mut Reader,
) -> (
    HashMap<u64, (u64, String)>,
    Vec<(String, String, u64, Inode, Vec<(u64, u64, u64)>)>,
) {
    let dirs: HashMap<u64, (u64, String)> = recs
        .drecs
        .iter()
        .filter(|d| d.3 == DT_DIR)
        .map(|(p, n, id, _)| (*id, (*p, n.clone())))
        .collect();
    let path_of = |mut id: u64| {
        let mut parts = Vec::new();
        while id != ROOT_DIR && parts.len() < MAX_DEPTH {
            let Some((parent, name)) = dirs.get(&id) else {
                break;
            };
            parts.push(name.clone());
            id = *parent;
        }
        parts.extend(prefix.map(str::to_string));
        parts.reverse();
        parts.join("/")
    };
    let mut files = Vec::new();
    for (parent, fname, id, kind) in &recs.drecs {
        if *kind != DT_REG {
            continue;
        }
        let Some(inode) = recs.inodes.get(id) else {
            continue;
        };
        let size = inode.size.unwrap_or(0);
        let mut list = recs
            .extents
            .get(&inode.private)
            .cloned()
            .unwrap_or_default();
        list.sort_unstable();
        for &(_, l, b) in &list {
            r.mark(b, l.div_ceil(block));
        }
        files.push((path_of(*parent), fname.clone(), size, inode.clone(), list));
    }
    (dirs, files)
}

pub fn read(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    let mut first = vec![0u8; 4096];
    src.read_at(offset, &mut first).ok()?;
    if &first[32..36] != NX_MAGIC {
        return None;
    }
    let block = u64::from(le32(&first, 36));
    if !(4096..=65536).contains(&block) || !block.is_power_of_two() {
        return None;
    }
    let blocks = le64(&first, 40).min(len / block);
    let mut r = Reader {
        src,
        offset,
        block,
        blocks,
        used: vec![0; blocks.div_ceil(64) as usize],
    };

    // The newest valid superblock in the checkpoint area is the current
    // state; the copy in block 0 can be older.
    let mut nx = r.block(0)?;
    let desc_blocks = le32(&nx, 104);
    let desc_base = le64(&nx, 112);
    let (data_blocks, data_base) = (le32(&nx, 108), le64(&nx, 120));
    if desc_blocks & 0x8000_0000 == 0 {
        // The descriptor area must lie inside the container; a corrupt
        // base or count would otherwise overflow or try 2^31 blocks.
        let end = desc_base.saturating_add(u64::from(desc_blocks)).min(blocks);
        for b in desc_base..end {
            if let Some(o) = r.block(b) {
                if obj_type(&o) == TYPE_NX && &o[32..36] == NX_MAGIC && le64(&o, 16) > le64(&nx, 16)
                {
                    nx = o;
                }
            }
        }
        r.mark(desc_base, u64::from(desc_blocks));
    }
    if data_blocks & 0x8000_0000 == 0 {
        r.mark(data_base, u64::from(data_blocks));
    }
    r.mark(0, 1);
    let xid = le64(&nx, 16);
    let omap = r.omap(le64(&nx, 160));
    let lookup = |map: &HashMap<u64, Vec<(u64, u64)>>, oid: u64| {
        map.get(&oid)?
            .iter()
            .filter(|(x, _)| *x <= xid)
            .max_by_key(|(x, _)| *x)
            .map(|(_, b)| *b)
    };

    let max_fs = (le32(&nx, 180) as usize).min(100);
    let mut volumes = Vec::new();
    let mut all = Vec::new();
    for i in 0..max_fs {
        let oid = le64(&nx, 184 + i * 8);
        if oid == 0 {
            continue;
        }
        let Some(at) = lookup(&omap, oid) else {
            continue;
        };
        let Some(sb) = r.block(at) else { continue };
        if &sb[32..36] != APFS_MAGIC {
            continue;
        }
        r.mark(at, 1);
        let name_end = sb[704..960].iter().position(|&b| b == 0).unwrap_or(256);
        let name = String::from_utf8_lossy(&sb[704..704 + name_end]).into_owned();
        let mut recs = Records::default();
        let mut snaps = Vec::new();
        // An encrypted volume's directories cannot be read without its
        // password; its files are only found by deep scan, and only if
        // they were never encrypted.
        if le64(&sb, 264) & FS_UNENCRYPTED != 0 {
            let vol_omap = r.omap(le64(&sb, 128));
            let physical = le32(&sb, 116) & PHYSICAL != 0;
            r.fs_tree(le64(&sb, 136), physical, &vol_omap, xid, &mut recs);
            snaps = snapshots(&mut r, &sb, &vol_omap, xid);
        }
        volumes.push(name);
        all.push((recs, snaps));
    }
    if volumes.is_empty() {
        return None;
    }

    let mut files = Vec::new();
    let mut live = Vec::new();
    let mut live_starts = HashSet::new();
    let several = volumes.len() > 1;
    // Files as each snapshot has them: (snapshot, its date, the file).
    let mut snap_files = Vec::new();
    for (name, (recs, snaps)) in volumes.iter().zip(&all) {
        let prefix = several.then_some(name.as_str());
        let (dirs, list) = listing(recs, prefix, block, &mut r);
        files.extend(list);
        // Blocks a snapshot still holds are in use, however long ago the
        // live volume let go of them.
        for (snap, srecs) in snaps {
            let (_, list) = listing(srecs, prefix, block, &mut r);
            let label = snap.label();
            snap_files.extend(
                list.into_iter()
                    .map(|f| (label.clone(), time(snap.created), f)),
            );
        }
        let mut ids: HashSet<u64> = recs.inodes.keys().copied().collect();
        ids.extend(recs.drecs.iter().map(|d| d.2));
        live.push(LiveVolume {
            name: name.clone(),
            ids,
            dirs,
        });
    }

    let container = Container {
        offset,
        len,
        block,
        blocks,
        volumes: live,
        used: r.used,
        live_starts: HashSet::new(),
    };
    let mut out = Vec::new();
    for (dir, name, size, inode, list) in files {
        let extents = container.extents(&list, size);
        if let Some(e) = extents.first() {
            live_starts.insert((e.offset, size));
        }
        out.push(FsFile {
            dir,
            name,
            size,
            modified: time(inode.modified),
            created: time(inode.created),
            deleted: false,
            extents,
            overwritten: false,
            alternatives: Vec::new(),
        });
    }
    // Snapshot files whose data the live volume no longer has: deleted
    // since, or changed (APFS writes changes to new blocks, so the same
    // blocks mean the same content).
    let live_data: HashSet<(u64, &[Extent])> =
        out.iter().map(|f| (f.size, f.extents.as_slice())).collect();
    let mut snapshots = Vec::new();
    for (snapshot, taken, (dir, name, size, inode, list)) in snap_files {
        let extents = container.extents(&list, size);
        if extents.is_empty() || live_data.contains(&(size, extents.as_slice())) {
            continue;
        }
        snapshots.push(SnapshotFile {
            snapshot,
            taken,
            file: FsFile {
                dir,
                name,
                size,
                modified: time(inode.modified),
                created: time(inode.created),
                deleted: false,
                extents,
                overwritten: false,
                alternatives: Vec::new(),
            },
        });
    }
    let container = Container {
        live_starts,
        ..container
    };
    let free = free_extents(
        blocks.min(u64::from(u32::MAX - 2)) as u32,
        |c| container.in_use(u64::from(c - 2)),
        block,
        |c| offset + u64::from(c - 2) * block,
    );
    Some(Volume {
        fs: FsType::Apfs,
        label: volumes.first().cloned(),
        offset,
        len: container.len,
        cluster_size: block,
        files: out,
        free,
        apfs: Some(Arc::new(container)),
        names: Vec::new(),
        name_sources: Default::default(),
        snapshots,
    })
}

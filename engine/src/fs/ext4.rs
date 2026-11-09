//! ext2, ext3 and ext4, the Linux filesystems: used on Linux drives, on
//! Raspberry Pi and other single-board computers' cards, and by Android.
//!
//! Every file has an inode in a table: its size, dates and where its data
//! is (a tree of extents on ext4, block pointers on ext2 and ext3). Folders
//! are lists of entries, each a name and an inode number.
//!
//! Deleting a file on ext3 or ext4 erases the inode's size and block map,
//! and its folder entry is absorbed into the entry before it. But both
//! filesystems are journaled: every change is first written to the journal
//! as a copy of the whole block it is in, and those copies stay there until
//! the journal comes round again. So the journal usually holds a copy of a
//! deleted file's inode from before the delete, size, dates and block map
//! intact. Its name usually survives on the disk itself, because the
//! absorbed entry's bytes are left in place, and in old copies of its
//! folder's blocks in the journal. ext2 has no journal but does not erase
//! the inode on delete, only marks it free.

use std::collections::{HashMap, HashSet};

use super::{FsFile, FsType, Volume};
use crate::extent::{push_merged, Extent};
use crate::formats::DateTime;
use crate::source::Source;

const MAGIC: u16 = 0xEF53;
const ROOT: u32 = 2;
const JOURNAL_MAGIC: u32 = 0xC03B_3998;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const RO_SPARSE_SUPER: u32 = 0x1;
const RO_HUGE_FILE: u32 = 0x8;
const RO_GDT_CSUM: u32 = 0x10;
const RO_BIGALLOC: u32 = 0x200;
const RO_METADATA_CSUM: u32 = 0x400;

const INODE_UNINIT: u16 = 0x1;
const BLOCK_UNINIT: u16 = 0x2;

const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const ENCRYPT_FL: u32 = 0x800;
const EXTENTS_FL: u32 = 0x8_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const EXTENT_MAGIC: u16 = 0xF30A;
const XATTR_MAGIC: u32 = 0xEA02_0000;

const JBD2_DESCRIPTOR: u32 = 1;
const JBD2_CSUM_V2: u32 = 0x8;
const JBD2_CSUM_V3: u32 = 0x10;
const JBD2_64BIT: u32 = 0x2;
const TAG_ESCAPED: u32 = 0x1;
const TAG_SAME_UUID: u32 = 0x2;
const TAG_LAST: u32 = 0x8;

const MAX_GROUPS: u64 = 1 << 21;
/// Most inodes read; bounds the work on a corrupt or huge volume.
const MAX_INODES: u64 = 20_000_000;
const MAX_EXTENTS: usize = 1 << 16;
const MAX_TREE_DEPTH: u16 = 5;
const MAX_DIR_BLOCKS: u64 = 1 << 15;
const MAX_JOURNAL_BLOCKS: u64 = 1 << 22;
const MAX_PATH_DEPTH: usize = 64;
/// Folder for deleted files whose name was not found anywhere.
const LOST: &str = "Lost files";

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

fn be16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |x| u16::from_be_bytes([x[0], x[1]]))
}

fn be32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
}

fn read_exact(src: &dyn Source, at: u64, buf: &mut [u8]) -> bool {
    matches!(src.read_at(at, buf), Ok(n) if n == buf.len())
}

/// A timestamp: signed seconds, extended past 2038 by two bits of the
/// matching `_extra` field when the inode has one.
fn timestamp(lo: u32, extra: Option<u32>) -> i64 {
    i64::from(lo as i32) + extra.map_or(0, |e| i64::from(e & 3) << 32)
}

/// The superblock fields Reperio needs.
#[derive(Debug, Clone)]
struct Super {
    block: u64,
    blocks: u64,
    first_data_block: u64,
    blocks_per_group: u64,
    /// log2 of blocks per allocation cluster (bigalloc), usually 0.
    cluster_bits: u32,
    inodes_count: u32,
    inodes_per_group: u32,
    inode_size: usize,
    first_ino: u32,
    desc_size: usize,
    groups: u64,
    compat: u32,
    incompat: u32,
    ro_compat: u32,
    first_meta_bg: u64,
    reserved_gdt: u64,
    journal_inum: u32,
    backup_bgs: [u32; 2],
    label: Option<String>,
    /// The group whose copy of the superblock this is: 0 for the volume's
    /// own, another for a backup read because the first was damaged. A
    /// backup group also holds a copy of the group descriptors.
    copy_group: u64,
}

impl Super {
    /// Parses the 1024-byte superblock, rejecting anything inconsistent.
    fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 1024 || le16(b, 0x38) != MAGIC {
            return None;
        }
        let log = le32(b, 0x18);
        if log > 6 {
            return None;
        }
        let block = 1024u64 << log;
        let compat = le32(b, 0x5C);
        let incompat = le32(b, 0x60);
        let ro_compat = le32(b, 0x64);
        let wide = incompat & INCOMPAT_64BIT != 0;
        let blocks = u64::from(le32(b, 0x04))
            | if wide {
                u64::from(le32(b, 0x150)) << 32
            } else {
                0
            };
        let first_data_block = u64::from(le32(b, 0x14));
        let blocks_per_group = u64::from(le32(b, 0x20));
        let cluster_bits = if ro_compat & RO_BIGALLOC != 0 {
            le32(b, 0x1C).checked_sub(log)?
        } else {
            0
        };
        if cluster_bits > 16 {
            return None;
        }
        let clusters_per_group = blocks_per_group >> cluster_bits;
        let inodes_count = le32(b, 0x00);
        let inodes_per_group = le32(b, 0x28);
        let (inode_size, first_ino) = if le32(b, 0x4C) == 0 {
            (128, 11)
        } else {
            (le16(b, 0x58) as usize, le32(b, 0x54))
        };
        let desc_size = if wide { le16(b, 0xFE) as usize } else { 32 };
        let valid = clusters_per_group > 0
            && clusters_per_group.is_multiple_of(8)
            && clusters_per_group <= 8 * block
            && inodes_per_group > 0
            && inodes_per_group.is_multiple_of(8)
            && u64::from(inodes_per_group) <= 8 * block
            && (128..=block as usize).contains(&inode_size)
            && inode_size.is_power_of_two()
            && (32..=1024).contains(&desc_size)
            && desc_size.is_power_of_two()
            && (!wide || desc_size >= 64)
            && first_data_block <= 1
            && blocks > first_data_block
            && first_ino > ROOT
            && first_ino <= inodes_count;
        if !valid {
            return None;
        }
        let groups = (blocks - first_data_block).div_ceil(blocks_per_group);
        if groups > MAX_GROUPS || u64::from(inodes_count) > groups * u64::from(inodes_per_group) {
            return None;
        }
        let name = &b[0x78..0x88];
        let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())];
        let label =
            Some(String::from_utf8_lossy(name).trim().to_string()).filter(|s| !s.is_empty());
        Some(Self {
            block,
            blocks,
            first_data_block,
            blocks_per_group,
            cluster_bits,
            inodes_count,
            inodes_per_group,
            inode_size,
            first_ino,
            desc_size,
            groups,
            compat,
            incompat,
            ro_compat,
            first_meta_bg: u64::from(le32(b, 0x104)),
            reserved_gdt: u64::from(le16(b, 0xCE)),
            journal_inum: le32(b, 0xE0),
            backup_bgs: [le32(b, 0x24C), le32(b, 0x250)],
            label,
            copy_group: u64::from(le16(b, 0x5A)),
        })
    }

    /// Which of the three the volume is, by the features it uses.
    fn kind(&self) -> FsType {
        let ext4_incompat =
            INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_INLINE_DATA;
        let ext4_ro = RO_HUGE_FILE | RO_GDT_CSUM | RO_BIGALLOC | RO_METADATA_CSUM;
        if self.incompat & ext4_incompat != 0 || self.ro_compat & ext4_ro != 0 {
            FsType::Ext4
        } else if self.compat & COMPAT_HAS_JOURNAL != 0 {
            FsType::Ext3
        } else {
            FsType::Ext2
        }
    }

    /// Whether the group descriptors carry checksums, which is when their
    /// "not initialised" flags and unused-inode counts can be trusted.
    fn csum(&self) -> bool {
        self.ro_compat & (RO_GDT_CSUM | RO_METADATA_CSUM) != 0
    }

    fn filetype(&self) -> bool {
        self.incompat & INCOMPAT_FILETYPE != 0
    }

    fn group_start(&self, g: u64) -> u64 {
        self.first_data_block + g * self.blocks_per_group
    }

    /// Whether group `g` holds a backup of the superblock.
    fn has_super(&self, g: u64) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return self.backup_bgs.iter().any(|&b| u64::from(b) == g);
        }
        if self.ro_compat & RO_SPARSE_SUPER == 0 || g == 1 {
            return true;
        }
        [3u64, 5, 7].iter().any(|&base| {
            let mut n = base;
            while n < g {
                n *= base;
            }
            n == g
        })
    }

    fn desc_blocks(&self) -> u64 {
        (self.groups * self.desc_size as u64).div_ceil(self.block)
    }

    /// Where the `n`th block of group descriptors is. With meta_bg, each
    /// run of groups described by one block keeps that block at its start.
    fn desc_block(&self, n: u64) -> u64 {
        if self.incompat & INCOMPAT_META_BG == 0 || n < self.first_meta_bg {
            if self.copy_group == 0 {
                1024 / self.block + 1 + n
            } else {
                self.group_start(self.copy_group) + 1 + n
            }
        } else {
            let g = n * (self.block / self.desc_size as u64);
            self.group_start(g) + u64::from(self.has_super(g))
        }
    }

    fn inodes_per_block(&self) -> u64 {
        self.block / self.inode_size as u64
    }

    fn table_blocks(&self) -> u64 {
        (u64::from(self.inodes_per_group) * self.inode_size as u64).div_ceil(self.block)
    }
}

/// Identifies an ext2/3/4 volume from its superblock (1024 bytes, found
/// 1024 bytes into the volume).
pub(crate) fn detect(sb: &[u8]) -> Option<FsType> {
    Super::parse(sb)
        .filter(|s| s.incompat & INCOMPAT_JOURNAL_DEV == 0 && s.copy_group == 0)
        .map(|s| s.kind())
}

/// Where a superblock, the volume's own or a backup, says its volume is:
/// the file system, the volume's length, how far into the volume this
/// superblock sits, and which group's copy it is (0 for the volume's own).
pub(crate) fn locate(sb: &[u8]) -> Option<(FsType, u64, u64, u64)> {
    let s = Super::parse(sb).filter(|s| s.incompat & INCOMPAT_JOURNAL_DEV == 0)?;
    let g = s.copy_group;
    if g >= s.groups || (g > 0 && !s.has_super(g)) {
        return None;
    }
    let at = if g == 0 {
        1024
    } else {
        s.group_start(g).checked_mul(s.block)?
    };
    Some((s.kind(), s.blocks.checked_mul(s.block)?, at, g))
}

/// Groups whose superblock backups are tried when the first superblock
/// is damaged: the first few a sparse volume keeps.
const BACKUP_GROUPS: [u64; 3] = [1, 3, 5];

/// A backup superblock of the volume at `offset`, for when its own is
/// damaged. Backups sit at the start of groups 1, 3, 5..., whose size
/// depends on the block size; formatters use 8 blocks' worth of bits per
/// group (8192 blocks of 1 KiB, 32768 of 4 KiB), so those are tried.
fn backup_super(src: &dyn Source, offset: u64, len: u64) -> Option<Super> {
    for g in BACKUP_GROUPS {
        for (block, first) in [(4096u64, 0u64), (1024, 1), (2048, 0)] {
            let at = (first + g * 8 * block) * block;
            if at.saturating_add(1024) > len {
                continue;
            }
            let mut raw = [0u8; 1024];
            if !read_exact(src, offset.saturating_add(at), &mut raw) {
                continue;
            }
            let Some(sb) = Super::parse(&raw) else {
                continue;
            };
            let fits = sb.copy_group == g
                && sb.block == block
                && sb.first_data_block == first
                && sb.blocks_per_group == 8 * block
                && sb.incompat & INCOMPAT_JOURNAL_DEV == 0;
            if fits {
                return Some(sb);
            }
        }
    }
    None
}

/// Identifies an ext2/3/4 volume at `offset` whose own superblock is
/// damaged, by a backup.
pub(crate) fn detect_backup(src: &dyn Source, offset: u64, len: u64) -> Option<FsType> {
    backup_super(src, offset, len).map(|s| s.kind())
}

#[derive(Debug, Clone, Copy, Default)]
struct Group {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    flags: u16,
    itable_unused: u32,
}

fn parse_group(d: &[u8], wide: bool) -> Group {
    let hi = |at| {
        if wide {
            u64::from(le32(d, at)) << 32
        } else {
            0
        }
    };
    Group {
        block_bitmap: u64::from(le32(d, 0x0)) | hi(0x20),
        inode_bitmap: u64::from(le32(d, 0x4)) | hi(0x24),
        inode_table: u64::from(le32(d, 0x8)) | hi(0x28),
        flags: le16(d, 0x12),
        itable_unused: u32::from(le16(d, 0x1C))
            | if wide {
                u32::from(le16(d, 0x32)) << 16
            } else {
                0
            },
    }
}

/// One inode's useful contents.
#[derive(Debug, Clone)]
struct Inode {
    mode: u16,
    size: u64,
    links: u16,
    dtime: u32,
    flags: u32,
    generation: u32,
    /// `i_block`: the extent tree root, block pointers or inline data.
    map: [u8; 60],
    mtime: i64,
    crtime: Option<i64>,
    /// For a file stored inside its inode: where its bytes are, in source
    /// offsets (wherever this copy of the inode was read from).
    inline: Vec<Extent>,
}

impl Inode {
    fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    fn has_inline(&self) -> bool {
        self.flags & INLINE_DATA_FL != 0
    }
}

/// Parses an inode. `at` is where `raw` was read from, to locate inline
/// data.
fn parse_inode(raw: &[u8], at: u64) -> Option<Inode> {
    if raw.len() < 128 {
        return None;
    }
    let mode = le16(raw, 0x0);
    if mode == 0 {
        return None;
    }
    let mut extra = if raw.len() > 128 {
        le16(raw, 0x80) as usize
    } else {
        0
    };
    if 128 + extra > raw.len() || !extra.is_multiple_of(4) {
        extra = 0;
    }
    let field = |off: usize| (off + 4 <= 128 + extra).then(|| le32(raw, off));
    let flags = le32(raw, 0x20);
    // On ext2 and ext3 a folder's i_size_high held something else.
    let high = if mode & S_IFMT == S_IFREG {
        u64::from(le32(raw, 0x6C)) << 32
    } else {
        0
    };
    let size = u64::from(le32(raw, 0x4)) | high;
    let mut map = [0u8; 60];
    map.copy_from_slice(&raw[0x28..0x64]);
    let crtime = field(0x90)
        .filter(|&t| t != 0)
        .map(|t| timestamp(t, field(0x94)));
    let mut inode = Inode {
        mode,
        size,
        links: le16(raw, 0x1A),
        dtime: le32(raw, 0x14),
        flags,
        generation: le32(raw, 0x64),
        map,
        mtime: timestamp(le32(raw, 0x10), field(0x88)),
        crtime,
        inline: Vec::new(),
    };
    if flags & INLINE_DATA_FL != 0 {
        inode.inline = inline_extents(raw, at, 128 + extra, size);
    }
    Some(inode)
}

/// Where an inline file's bytes are: the first 60 in `i_block`, the rest
/// in the "system.data" extended attribute stored in the inode after its
/// fixed fields.
fn inline_extents(raw: &[u8], at: u64, ibody: usize, size: u64) -> Vec<Extent> {
    let mut out = Vec::new();
    push_merged(&mut out, Extent::new(at + 0x28, size.min(60)));
    if size <= 60 || le32(raw, ibody) != XATTR_MAGIC {
        return out;
    }
    let first = ibody + 4;
    let mut p = first;
    while p + 16 <= raw.len() && le32(raw, p) != 0 {
        let name_len = raw[p] as usize;
        let (index, value_at) = (raw[p + 1], le16(raw, p + 2) as usize);
        let (inum, value_len) = (le32(raw, p + 4), le32(raw, p + 8) as u64);
        let name = raw.get(p + 16..p + 16 + name_len);
        if index == 7 && name == Some(b"data") && inum == 0 {
            let start = first + value_at;
            if start as u64 + value_len <= raw.len() as u64 {
                let len = value_len.min(size - 60);
                out.push(Extent::new(at + start as u64, len));
            }
            break;
        }
        p += (16 + name_len + 3) & !3;
    }
    out
}

/// Runs of a file's blocks: (first block in the file, first block on the
/// volume, count).
type Runs = Vec<(u64, u64, u64)>;

fn push_run(runs: &mut Runs, logical: u64, physical: u64, count: u64) {
    if let Some(last) = runs.last_mut() {
        if last.0 + last.2 == logical && last.1 + last.2 == physical {
            last.2 += count;
            return;
        }
    }
    runs.push((logical, physical, count));
}

/// An extent tree node: its depth, then for a leaf (depth 0) the runs it
/// maps, or for an index node the blocks of its children.
#[derive(Debug, PartialEq)]
enum Node {
    Leaf(Runs),
    Index(u16, Vec<u64>),
}

/// Parses an extent tree node: the 60 bytes of `i_block`, or a tree block.
fn extent_node(b: &[u8]) -> Option<Node> {
    if le16(b, 0) != EXTENT_MAGIC {
        return None;
    }
    let entries = le16(b, 2) as usize;
    let max = le16(b, 4) as usize;
    let depth = le16(b, 6);
    if entries > max || 12 + 12 * entries > b.len() || depth > MAX_TREE_DEPTH {
        return None;
    }
    let entry = |i: usize| &b[12 + 12 * i..24 + 12 * i];
    if depth == 0 {
        let mut runs = Vec::new();
        for e in (0..entries).map(entry) {
            let len = u64::from(le16(e, 4));
            // Over 32768 marks space reserved but never written.
            let len = if len > 32768 { len - 32768 } else { len };
            let start = u64::from(le16(e, 6)) << 32 | u64::from(le32(e, 8));
            runs.push((u64::from(le32(e, 0)), start, len));
        }
        Some(Node::Leaf(runs))
    } else {
        let children = (0..entries)
            .map(entry)
            .map(|e| u64::from(le16(e, 8)) << 32 | u64::from(le32(e, 4)))
            .collect();
        Some(Node::Index(depth, children))
    }
}

/// The blocks named in a jbd2 descriptor block, in the order the logged
/// copies follow it, with whether each copy's first four bytes were
/// escaped (zeroed because they looked like the journal's magic).
fn descriptor_tags(b: &[u8], incompat: u32) -> Vec<(u64, bool)> {
    let v3 = incompat & JBD2_CSUM_V3 != 0;
    let wide = incompat & JBD2_64BIT != 0;
    let tag_bytes = if v3 {
        16
    } else {
        let sz = 12 + if incompat & JBD2_CSUM_V2 != 0 { 2 } else { 0 };
        if wide {
            sz
        } else {
            sz - 4
        }
    };
    let end = b.len()
        - if incompat & (JBD2_CSUM_V2 | JBD2_CSUM_V3) != 0 {
            4
        } else {
            0
        };
    let mut out = Vec::new();
    let mut at = 12;
    while at + tag_bytes <= end {
        let flags = if v3 {
            be32(b, at + 4)
        } else {
            u32::from(be16(b, at + 6))
        };
        let high = if wide {
            u64::from(be32(b, at + 8)) << 32
        } else {
            0
        };
        out.push((u64::from(be32(b, at)) | high, flags & TAG_ESCAPED != 0));
        at += tag_bytes;
        if flags & TAG_SAME_UUID == 0 {
            at += 16;
        }
        if flags & TAG_LAST != 0 {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
struct Dirent {
    ino: u32,
    /// 1 for a file, 2 for a folder, 0 if the volume does not record it.
    kind: u8,
    name: String,
    /// Still in the folder, rather than left over in the gap a deleted
    /// entry leaves behind.
    live: bool,
}

/// An entry's name length and file type.
fn name_fields(b: &[u8], at: usize, filetype: bool) -> (usize, u8) {
    if filetype {
        (b[at + 6] as usize, b[at + 7])
    } else {
        (le16(b, at + 6) as usize, 0)
    }
}

fn entry_len(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && !name.iter().any(|&c| c == 0 || c == b'/')
}

/// Parses a block of folder entries, including deleted entries left in
/// the gaps. `None` unless the entries chain exactly over the whole block,
/// which is what tells a folder block apart from anything else.
fn parse_dir(b: &[u8], filetype: bool, max_ino: u32) -> Option<Vec<Dirent>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        if at + 8 > b.len() {
            return None;
        }
        let ino = le32(b, at);
        let rec_len = match le16(b, at + 4) {
            0 | 0xFFFF if b.len() >= 65536 => 65536,
            n => n as usize,
        };
        let (name_len, kind) = name_fields(b, at, filetype);
        if rec_len < 8
            || !rec_len.is_multiple_of(4)
            || at + rec_len > b.len()
            || 8 + name_len > rec_len
        {
            return None;
        }
        if ino != 0 {
            let name = &b[at + 8..at + 8 + name_len];
            if ino > max_ino || !valid_name(name) {
                return None;
            }
            out.push(Dirent {
                ino,
                kind,
                name: String::from_utf8_lossy(name).into_owned(),
                live: true,
            });
        }
        scan_gap(
            b,
            at + entry_len(name_len),
            at + rec_len,
            filetype,
            max_ino,
            &mut out,
        );
        at += rec_len;
    }
    Some(out)
}

/// Finds deleted entries between `start` and `end`. Strict, because it is
/// run at every 4-byte step through the gap.
fn scan_gap(
    b: &[u8],
    start: usize,
    end: usize,
    filetype: bool,
    max_ino: u32,
    out: &mut Vec<Dirent>,
) {
    let mut p = start;
    while p + 12 <= end {
        let ino = le32(b, p);
        let rec_len = le16(b, p + 4) as usize;
        let (name_len, kind) = name_fields(b, p, filetype);
        let fits = name_len > 0
            && name_len <= 255
            && p + 8 + name_len <= end
            && rec_len.is_multiple_of(4)
            && rec_len >= entry_len(name_len)
            && p + rec_len <= end;
        if ino == 0 || ino > max_ino || !fits || (filetype && !(1..=7).contains(&kind)) {
            p += 4;
            continue;
        }
        let name = &b[p + 8..p + 8 + name_len];
        if !valid_name(name) || name == b"." || name == b".." {
            p += 4;
            continue;
        }
        out.push(Dirent {
            ino,
            kind,
            name: String::from_utf8_lossy(name).into_owned(),
            live: false,
        });
        p += entry_len(name_len);
    }
}

/// A copy of a block in the journal.
#[derive(Debug, Clone, Copy)]
struct Logged {
    seq: u32,
    at: u64,
    escaped: bool,
}

/// How good a sighting of a folder entry is, best last: the more recent
/// the copy it was seen in the better (the journal transaction, or the
/// disk itself), then an entry in use beats one left in a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    seq: u64,
    live: bool,
}

/// What is on the disk now ranks as newer than any journal copy.
const ON_DISK: u64 = u64::MAX;

struct Ext<'a> {
    src: &'a dyn Source,
    offset: u64,
    sb: Super,
    groups: Vec<Group>,
    /// One bit per allocation cluster, group after group.
    block_map: Vec<u8>,
    inode_map: Vec<u8>,
    /// Inode table locations: (first block, group), sorted.
    tables: Vec<(u64, u64)>,
    /// Copies of blocks in the journal, by volume block, oldest first.
    journal: HashMap<u64, Vec<Logged>>,
}

impl<'a> Ext<'a> {
    fn block_offset(&self, b: u64) -> u64 {
        self.offset + b * self.sb.block
    }

    fn read_block(&self, b: u64) -> Option<Vec<u8>> {
        if b >= self.sb.blocks {
            return None;
        }
        let mut buf = vec![0u8; self.sb.block as usize];
        read_exact(self.src, self.block_offset(b), &mut buf).then_some(buf)
    }

    fn read_logged(&self, l: &Logged) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.sb.block as usize];
        if !read_exact(self.src, l.at, &mut buf) {
            return None;
        }
        if l.escaped {
            buf[..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
        }
        Some(buf)
    }

    /// Bit index of block `b` in the block map.
    fn block_bit(&self, b: u64) -> Option<usize> {
        let rel = b.checked_sub(self.sb.first_data_block)?;
        if b >= self.sb.blocks {
            return None;
        }
        let per_group = self.sb.blocks_per_group >> self.sb.cluster_bits;
        let g = rel / self.sb.blocks_per_group;
        let i = (rel % self.sb.blocks_per_group) >> self.sb.cluster_bits;
        Some((g * per_group + i) as usize)
    }

    fn block_used(&self, b: u64) -> bool {
        let Some(i) = self.block_bit(b) else {
            return true;
        };
        self.block_map
            .get(i / 8)
            .is_none_or(|x| x & (1 << (i % 8)) != 0)
    }

    fn mark_used(&mut self, b: u64) {
        if let Some(i) = self.block_bit(b) {
            if let Some(x) = self.block_map.get_mut(i / 8) {
                *x |= 1 << (i % 8);
            }
        }
    }

    fn inode_used(&self, ino: u32) -> bool {
        let i = ino.wrapping_sub(1) as usize;
        self.inode_map
            .get(i / 8)
            .is_some_and(|x| x & (1 << (i % 8)) != 0)
    }

    /// The first inode stored in block `b`, if it is in an inode table.
    fn inode_block(&self, b: u64) -> Option<u32> {
        let n = self.tables.partition_point(|&(start, _)| start <= b);
        let (start, g) = *self.tables.get(n.checked_sub(1)?)?;
        if b >= start + self.sb.table_blocks() {
            return None;
        }
        let first =
            g * u64::from(self.sb.inodes_per_group) + (b - start) * self.sb.inodes_per_block() + 1;
        u32::try_from(first).ok()
    }

    fn read_groups(&mut self) -> Option<()> {
        let wide = self.sb.desc_size >= 64;
        let per_block = (self.sb.block / self.sb.desc_size as u64) as usize;
        for n in 0..self.sb.desc_blocks() {
            let block = self.read_block(self.sb.desc_block(n))?;
            for d in block.chunks_exact(self.sb.desc_size).take(per_block) {
                if self.groups.len() as u64 == self.sb.groups {
                    break;
                }
                self.groups.push(parse_group(d, wide));
            }
        }
        let mut tables: Vec<(u64, u64)> = (0..self.groups.len())
            .map(|g| (self.groups[g].inode_table, g as u64))
            .filter(|&(t, _)| t > 0 && t < self.sb.blocks)
            .collect();
        tables.sort_unstable();
        self.tables = tables;
        Some(())
    }

    /// Reads the block and inode bitmaps. Groups never used have no bitmap
    /// written, and count as empty.
    fn read_bitmaps(&mut self) {
        let sb = self.sb.clone();
        let block_bytes = ((sb.blocks_per_group >> sb.cluster_bits) / 8) as usize;
        let inode_bytes = (sb.inodes_per_group / 8) as usize;
        self.block_map = vec![0; block_bytes * self.groups.len()];
        self.inode_map = vec![0; inode_bytes * self.groups.len()];
        for g in 0..self.groups.len() {
            let grp = self.groups[g];
            if !(sb.csum() && grp.flags & BLOCK_UNINIT != 0) {
                if let Some(b) = self.read_block(grp.block_bitmap) {
                    self.block_map[g * block_bytes..(g + 1) * block_bytes]
                        .copy_from_slice(&b[..block_bytes]);
                }
            }
            if !(sb.csum() && grp.flags & INODE_UNINIT != 0) {
                if let Some(b) = self.read_block(grp.inode_bitmap) {
                    self.inode_map[g * inode_bytes..(g + 1) * inode_bytes]
                        .copy_from_slice(&b[..inode_bytes]);
                }
            }
        }
        // The volume's own structures, which an unwritten bitmap leaves
        // out: superblock backups, descriptors, bitmaps and inode tables.
        let gdt = if sb.incompat & INCOMPAT_META_BG != 0 {
            0
        } else {
            sb.desc_blocks() + sb.reserved_gdt
        };
        for g in 0..self.groups.len() as u64 {
            if sb.has_super(g) {
                let start = sb.group_start(g);
                for b in start..start.saturating_add(1 + gdt) {
                    self.mark_used(b);
                }
            }
            let grp = self.groups[g as usize];
            self.mark_used(grp.block_bitmap);
            self.mark_used(grp.inode_bitmap);
            // The table's start comes from a group descriptor: near 2^64 it
            // overflowed. Blocks past the end are ignored by mark_used.
            let table = grp.inode_table;
            for b in table..table.saturating_add(sb.table_blocks()) {
                self.mark_used(b);
            }
        }
        if sb.incompat & INCOMPAT_META_BG != 0 {
            for n in 0..sb.desc_blocks() {
                self.mark_used(sb.desc_block(n));
            }
        }
    }

    /// Every inode in the tables, skipping the parts the descriptors say
    /// were never used.
    fn read_inodes(&self) -> HashMap<u32, Inode> {
        let sb = &self.sb;
        let mut out = HashMap::new();
        let mut read = 0u64;
        let chunk = ((1 << 20) / sb.inode_size).max(1);
        for (g, grp) in self.groups.iter().enumerate() {
            let mut count = sb.inodes_per_group;
            if sb.csum() {
                if grp.flags & INODE_UNINIT != 0 {
                    continue;
                }
                count = count.saturating_sub(grp.itable_unused);
            }
            if grp.inode_table == 0 || grp.inode_table >= sb.blocks {
                continue;
            }
            let table = self.block_offset(grp.inode_table);
            let mut i = 0u32;
            while i < count && read < MAX_INODES {
                let n = (count - i).min(chunk as u32) as usize;
                let at = table + u64::from(i) * sb.inode_size as u64;
                let mut buf = vec![0u8; n * sb.inode_size];
                if !read_exact(self.src, at, &mut buf) {
                    break;
                }
                for (k, raw) in buf.chunks_exact(sb.inode_size).enumerate() {
                    let ino =
                        g as u64 * u64::from(sb.inodes_per_group) + u64::from(i) + k as u64 + 1;
                    let Ok(ino) = u32::try_from(ino) else { break };
                    if ino > sb.inodes_count {
                        break;
                    }
                    if let Some(inode) = parse_inode(raw, at + (k * sb.inode_size) as u64) {
                        out.insert(ino, inode);
                    }
                }
                i += n as u32;
                read += n as u64;
            }
        }
        out
    }

    /// A tree or indirect block. For a deleted file whose inode copy is
    /// from transaction `old` (0 for one still in the table), a copy from
    /// the journal that passes `good`, since deleting rewrote the block on
    /// the disk: the newest from no later than the inode copy, as the block
    /// may since have gone to another file, else the newest. Otherwise the
    /// block on the disk.
    fn meta_block(
        &self,
        b: u64,
        old: Option<u64>,
        good: impl Fn(&[u8]) -> bool,
    ) -> Option<Vec<u8>> {
        if let (Some(seq), Some(list)) = (old, self.journal.get(&b)) {
            let before = list.iter().rev().filter(|l| u64::from(l.seq) < seq);
            for l in before.chain(list.iter().rev()) {
                if let Some(buf) = self.read_logged(l).filter(|x| good(x)) {
                    return Some(buf);
                }
            }
        }
        self.read_block(b).filter(|x| good(x))
    }

    /// Where an inode's data is. `old` is for a copy of a deleted inode.
    /// `None` if the map is damaged or points outside the volume.
    fn runs(&self, inode: &Inode, old: Option<u64>) -> Option<Runs> {
        let needed = inode.size.div_ceil(self.sb.block);
        if needed > self.sb.blocks {
            return None;
        }
        let mut runs = Vec::new();
        if inode.flags & EXTENTS_FL != 0 {
            let mut seen = HashSet::new();
            self.walk_extents(&inode.map, None, old, &mut runs, &mut seen)?;
        } else {
            self.walk_indirect(&inode.map, needed, old, &mut runs)?;
        }
        runs.sort_unstable();
        let inside = runs
            .iter()
            .all(|&(_, p, n)| p > 0 && p.checked_add(n).is_some_and(|e| e <= self.sb.blocks));
        inside.then_some(runs)
    }

    fn walk_extents(
        &self,
        node: &[u8],
        depth: Option<u16>,
        old: Option<u64>,
        runs: &mut Runs,
        seen: &mut HashSet<u64>,
    ) -> Option<()> {
        match extent_node(node)? {
            Node::Leaf(list) => {
                if depth.is_some_and(|d| d != 0) {
                    return None;
                }
                for (l, p, n) in list {
                    if runs.len() >= MAX_EXTENTS {
                        return None;
                    }
                    push_run(runs, l, p, n);
                }
            }
            Node::Index(d, children) => {
                if depth.is_some_and(|want| want != d) {
                    return None;
                }
                for child in children {
                    if !seen.insert(child) || seen.len() > MAX_EXTENTS {
                        return None;
                    }
                    let good = |b: &[u8]| match extent_node(b) {
                        Some(Node::Leaf(r)) => d == 1 && !r.is_empty(),
                        Some(Node::Index(cd, c)) => cd + 1 == d && !c.is_empty(),
                        None => false,
                    };
                    let block = self.meta_block(child, old, good)?;
                    self.walk_extents(&block, Some(d - 1), old, runs, seen)?;
                }
            }
        }
        Some(())
    }

    /// Follows ext2/ext3 block pointers: twelve direct, then single,
    /// double and triple indirect blocks.
    fn walk_indirect(
        &self,
        map: &[u8; 60],
        needed: u64,
        old: Option<u64>,
        runs: &mut Runs,
    ) -> Option<()> {
        let per = self.sb.block / 4;
        let mut logical = 0u64;
        for i in 0..12 {
            if logical >= needed {
                return Some(());
            }
            let p = u64::from(le32(map, i * 4));
            if p != 0 {
                push_run(runs, logical, p, 1);
            }
            logical += 1;
        }
        for (level, slot) in [(1u32, 12usize), (2, 13), (3, 14)] {
            if logical >= needed {
                break;
            }
            let p = u64::from(le32(map, slot * 4));
            if p == 0 {
                logical += per.pow(level);
                continue;
            }
            self.indirect(p, level, &mut logical, needed, old, runs)?;
        }
        Some(())
    }

    fn indirect(
        &self,
        b: u64,
        level: u32,
        logical: &mut u64,
        needed: u64,
        old: Option<u64>,
        runs: &mut Runs,
    ) -> Option<()> {
        let blocks = self.sb.blocks;
        let good = |x: &[u8]| {
            x.chunks_exact(4).any(|c| c != [0; 4])
                && x.chunks_exact(4).all(|c| u64::from(le32(c, 0)) < blocks)
        };
        let buf = self.meta_block(b, old, good)?;
        let per = self.sb.block / 4;
        for c in buf.chunks_exact(4) {
            if *logical >= needed {
                break;
            }
            let p = u64::from(le32(c, 0));
            if level == 1 {
                if p != 0 {
                    push_run(runs, *logical, p, 1);
                }
                *logical += 1;
            } else if p == 0 {
                *logical += per.pow(level - 1);
            } else {
                if runs.len() >= MAX_EXTENTS {
                    return None;
                }
                self.indirect(p, level - 1, logical, needed, old, runs)?;
            }
        }
        Some(())
    }

    /// Source extents of the first `size` bytes, stopping at a hole.
    fn to_extents(&self, runs: &Runs, size: u64) -> Vec<Extent> {
        let mut out = Vec::new();
        let mut next = 0;
        let mut left = size;
        for &(l, p, n) in runs {
            if left == 0 || l != next {
                break;
            }
            let len = (n * self.sb.block).min(left);
            push_merged(&mut out, Extent::new(self.block_offset(p), len));
            left -= len;
            next = l + n;
        }
        out
    }

    /// The blocks holding the first `size` bytes.
    fn data_blocks(&self, runs: &Runs, size: u64) -> impl Iterator<Item = u64> + '_ {
        let needed = size.div_ceil(self.sb.block);
        let runs = runs.clone();
        runs.into_iter()
            .flat_map(|(l, p, n)| (0..n).map(move |i| (l + i, p + i)))
            .take_while(move |&(l, _)| l < needed)
            .map(|(_, p)| p)
    }

    /// Finds the logged blocks in the journal, from every descriptor block
    /// still in it, whether or not its transaction has been checkpointed.
    fn read_journal(&mut self, inode: &Inode) -> Option<()> {
        let runs = self.runs(inode, None)?;
        let len = (inode.size / self.sb.block).min(MAX_JOURNAL_BLOCKS);
        let phys = |l: u64| -> Option<u64> {
            let n = runs.partition_point(|&(start, _, _)| start <= l);
            let &(start, p, count) = runs.get(n.checked_sub(1)?)?;
            (l < start + count).then(|| self.block_offset(p + l - start))
        };
        let jsb = {
            let mut b = vec![0u8; self.sb.block as usize];
            read_exact(self.src, phys(0)?, &mut b).then_some(b)?
        };
        if be32(&jsb, 0) != JOURNAL_MAGIC
            || !matches!(be32(&jsb, 4), 3 | 4)
            || u64::from(be32(&jsb, 12)) != self.sb.block
        {
            return None;
        }
        let maxlen = u64::from(be32(&jsb, 16)).min(len);
        let first = u64::from(be32(&jsb, 20));
        if first == 0 || first >= maxlen {
            return None;
        }
        let incompat = if be32(&jsb, 4) == 4 {
            be32(&jsb, 0x28)
        } else {
            0
        };
        let wrap = |l: u64| first + (l - first) % (maxlen - first);
        let mut journal: HashMap<u64, Vec<Logged>> = HashMap::new();
        let block = self.sb.block as usize;
        // Read in large pieces: most blocks are logged copies, only the
        // descriptor blocks among them matter here.
        let mut chunk = Vec::new();
        let mut l = first;
        while l < maxlen {
            let n = runs
                .iter()
                .find(|&&(start, _, count)| start <= l && l < start + count)
                .map_or(0, |&(start, _, count)| start + count - l)
                .min(maxlen - l)
                .min(256);
            let Some(at) = phys(l).filter(|_| n > 0) else {
                l += 1;
                continue;
            };
            chunk.resize(n as usize * block, 0);
            if !read_exact(self.src, at, &mut chunk) {
                l += n;
                continue;
            }
            for (i, buf) in chunk.chunks_exact(block).enumerate() {
                if be32(buf, 0) != JOURNAL_MAGIC || be32(buf, 4) != JBD2_DESCRIPTOR {
                    continue;
                }
                let seq = be32(buf, 8);
                let here = l + i as u64;
                for (k, (target, escaped)) in descriptor_tags(buf, incompat).into_iter().enumerate()
                {
                    if target >= self.sb.blocks {
                        continue;
                    }
                    if let Some(at) = phys(wrap(here + 1 + k as u64)) {
                        journal
                            .entry(target)
                            .or_default()
                            .push(Logged { seq, at, escaped });
                    }
                }
            }
            l += n;
        }
        for list in journal.values_mut() {
            list.sort_by_key(|l| l.seq);
        }
        self.journal = journal;
        Some(())
    }

    /// Every inode in the journal's copies of inode table blocks, with the
    /// transaction that logged it.
    fn logged_inodes(&self) -> Vec<(u32, u32, Inode)> {
        let isz = self.sb.inode_size;
        let mut out = Vec::new();
        for (&b, list) in &self.journal {
            let Some(first) = self.inode_block(b) else {
                continue;
            };
            for l in list {
                let Some(buf) = self.read_logged(l) else {
                    continue;
                };
                for (k, raw) in buf.chunks_exact(isz).enumerate() {
                    let ino = first.saturating_add(k as u32);
                    if ino > self.sb.inodes_count {
                        break;
                    }
                    if let Some(inode) = parse_inode(raw, l.at + (k * isz) as u64) {
                        out.push((ino, l.seq, inode));
                    }
                }
            }
        }
        out
    }

    /// The blocks of a folder, as (block or `None` for inline, contents).
    /// For a deleted folder, only blocks not since reused.
    fn dir_blocks(&self, inode: &Inode, old: Option<u64>) -> Vec<(Option<u64>, Vec<u8>)> {
        if inode.has_inline() {
            return vec![(None, inode.map.to_vec())];
        }
        let Some(runs) = self.runs(inode, old) else {
            return Vec::new();
        };
        let size = inode.size.min(MAX_DIR_BLOCKS * self.sb.block);
        self.data_blocks(&runs, size)
            .filter(|&b| old.is_none() || !self.block_used(b))
            .filter_map(|b| Some((Some(b), self.read_block(b)?)))
            .collect()
    }
}

/// A folder entry seen somewhere: how good the sighting is, the folder
/// it was in and the name.
type Sighting = (Rank, u32, String);

/// Folder entries found everywhere, and what they say about each folder.
#[derive(Default)]
struct Names {
    /// Entries in live folders: (inode, is folder) to (folder, name).
    current: HashMap<(u32, bool), (u32, String)>,
    /// Every other sighting: (inode, is folder) to (rank, folder, name).
    old: HashMap<(u32, bool), Vec<Sighting>>,
    /// A folder's parent, from its ".." entry.
    dotdot: HashMap<u32, u32>,
}

impl Names {
    fn add(&mut self, dir: u32, entries: &[Dirent], seq: u64, live_dir: bool) {
        for e in entries {
            if e.name == "." {
                continue;
            }
            if e.name == ".." {
                self.dotdot.entry(dir).or_insert(e.ino);
                continue;
            }
            let targets: &[bool] = match e.kind {
                0 => &[false, true],
                2 => &[true],
                _ => &[false],
            };
            for &is_dir in targets {
                let key = (e.ino, is_dir);
                if live_dir && e.live && seq == ON_DISK {
                    self.current.entry(key).or_insert((dir, e.name.clone()));
                    continue;
                }
                let rank = Rank { seq, live: e.live };
                let list = self.old.entry(key).or_default();
                if !list
                    .iter()
                    .any(|(r, d, n)| *r == rank && *d == dir && *n == e.name)
                {
                    list.push((rank, dir, e.name.clone()));
                }
            }
        }
    }

    fn best(&self, key: (u32, bool)) -> Option<&Sighting> {
        self.old.get(&key)?.iter().max_by_key(|(r, _, _)| *r)
    }

    /// The name of one of the files an inode has held. `lives` lists when
    /// each file the inode held was first seen, as (transaction, generation),
    /// in order: a file's name is one seen while it was the inode's file.
    fn file_name(&self, ino: u32, gen: u32, lives: &[(u64, u32)]) -> Option<&Sighting> {
        let Some(at) = lives
            .iter()
            .position(|&(_, g)| g == gen)
            .filter(|_| lives.len() > 1)
        else {
            return self.best((ino, false));
        };
        let from = lives[at].0;
        let until = lives.get(at + 1).map(|&(s, _)| s);
        let within = self
            .old
            .get(&(ino, false))?
            .iter()
            .filter(|(r, _, _)| r.seq >= from && until.is_none_or(|u| r.seq < u));
        match within.max_by_key(|(r, _, _)| *r) {
            Some(found) => Some(found),
            None if until.is_none() => self.best((ino, false)),
            None => None,
        }
    }
}

pub fn read(src: &dyn Source, offset: u64, len: u64) -> Option<Volume> {
    let mut raw = [0u8; 1024];
    let own = read_exact(src, offset.checked_add(1024)?, &mut raw)
        .then(|| Super::parse(&raw))
        .flatten()
        .filter(|s| s.copy_group == 0);
    let sb = match own {
        Some(sb) => sb,
        None => backup_super(src, offset, len)?,
    };
    if sb.incompat & INCOMPAT_JOURNAL_DEV != 0 {
        return None;
    }
    let from_backup = sb.copy_group != 0;
    let kind = sb.kind();
    let mut ext = Ext {
        src,
        offset,
        sb,
        groups: Vec::new(),
        block_map: Vec::new(),
        inode_map: Vec::new(),
        tables: Vec::new(),
        journal: HashMap::new(),
    };
    ext.read_groups()?;
    if from_backup {
        // Backup descriptors are written when the volume is made and not
        // kept up to date: their "never used" flags and counts are from
        // then, so trusting them would skip everything written since.
        for g in &mut ext.groups {
            g.flags = 0;
            g.itable_unused = 0;
        }
    }
    ext.read_bitmaps();
    let table = ext.read_inodes();
    table.get(&ROOT).filter(|i| i.is_dir())?;
    if ext.sb.compat & COMPAT_HAS_JOURNAL != 0 {
        if let Some(j) = table.get(&ext.sb.journal_inum) {
            let j = j.clone();
            ext.read_journal(&j);
        }
    }
    let sb = &ext.sb;
    let filetype = sb.filetype();
    let max_ino = sb.inodes_count;
    let first_ino = sb.first_ino;
    let logged = ext.logged_inodes();

    // An inode copy is of a file or folder since deleted if the inode is
    // free now, or now belongs to something else.
    let gone = |ino: u32, copy: &Inode| {
        !ext.inode_used(ino)
            || table.get(&ino).is_none_or(|live| {
                live.generation != copy.generation
                    || live.mode & S_IFMT != copy.mode & S_IFMT
                    || matches!((live.crtime, copy.crtime), (Some(a), Some(b)) if a != b)
            })
    };
    let user = |ino: u32| ino == ROOT || ino >= first_ino;
    let live_dir = |ino: u32| {
        ext.inode_used(ino)
            && table
                .get(&ino)
                .is_some_and(|i| i.is_dir() && i.links > 0 && i.dtime == 0)
    };

    // Folders: live ones, then deleted ones from the journal and (ext2)
    // from the table, whose blocks were not erased.
    let mut names = Names::default();
    let mut owner: HashMap<u64, u32> = HashMap::new();
    let mut live_dirs: Vec<u32> = table
        .keys()
        .copied()
        .filter(|&i| user(i) && live_dir(i))
        .collect();
    live_dirs.sort_unstable();
    for &d in &live_dirs {
        let inode = &table[&d];
        for (b, data) in ext.dir_blocks(inode, None) {
            let entries = match b {
                Some(b) => {
                    owner.entry(b).or_insert(d);
                    parse_dir(&data, filetype, max_ino)
                }
                None => {
                    names.dotdot.entry(d).or_insert(le32(&data, 0));
                    parse_dir(&data[4..], filetype, max_ino)
                }
            };
            if let Some(entries) = entries {
                names.add(d, &entries, ON_DISK, true);
            }
        }
    }
    let mut old_dirs: Vec<(u32, u64, &Inode)> = logged
        .iter()
        .filter(|(ino, _, i)| {
            user(*ino) && i.is_dir() && i.links > 0 && i.dtime == 0 && i.size > 0 && gone(*ino, i)
        })
        .map(|(ino, seq, i)| (*ino, u64::from(*seq) + 1, i))
        .collect();
    old_dirs.extend(
        table
            .iter()
            .filter(|(&ino, i)| user(ino) && i.is_dir() && i.size > 0 && !ext.inode_used(ino))
            .map(|(&ino, i)| (ino, 0, i)),
    );
    old_dirs.sort_by_key(|(ino, seq, _)| (*ino, *seq));
    for (d, seq, inode) in &old_dirs {
        for (b, data) in ext.dir_blocks(inode, Some(*seq)) {
            let entries = match b {
                Some(b) => {
                    owner.entry(b).or_insert(*d);
                    parse_dir(&data, filetype, max_ino)
                }
                None => parse_dir(&data[4..], filetype, max_ino),
            };
            if let Some(entries) = entries {
                names.add(*d, &entries, ON_DISK, false);
            }
        }
    }
    // Old copies of folder blocks in the journal.
    let mut logged_blocks: Vec<(&u64, &Vec<Logged>)> = ext.journal.iter().collect();
    logged_blocks.sort_by_key(|(b, _)| **b);
    for (&b, list) in logged_blocks {
        if ext.inode_block(b).is_some() {
            continue;
        }
        for l in list {
            let Some(data) = ext.read_logged(l) else {
                continue;
            };
            let Some(entries) = parse_dir(&data, filetype, max_ino) else {
                continue;
            };
            let dir = match entries.first() {
                Some(e) if e.live && e.name == "." && le32(&data, 0) == e.ino => Some(e.ino),
                _ => owner.get(&b).copied(),
            };
            if let Some(dir) = dir {
                names.add(dir, &entries, u64::from(l.seq) + 1, false);
            }
        }
    }

    let dir_name = |d: u32| -> Option<(u32, String)> {
        if live_dir(d) {
            if let Some(n) = names.current.get(&(d, true)) {
                return Some(n.clone());
            }
        }
        names.best((d, true)).map(|(_, p, n)| (*p, n.clone()))
    };
    let path_of = |mut d: u32| {
        let mut parts = Vec::new();
        let mut seen = HashSet::new();
        while d != ROOT && parts.len() < MAX_PATH_DEPTH && seen.insert(d) {
            match dir_name(d) {
                Some((parent, name)) => {
                    parts.push(name);
                    d = parent;
                }
                None => {
                    parts.push(format!("Folder {d}"));
                    match names.dotdot.get(&d) {
                        Some(&p) if p != d => d = p,
                        _ => break,
                    }
                }
            }
        }
        parts.reverse();
        parts.join("/")
    };
    let date = |t: i64| DateTime::from_unix(t);

    let mut files = Vec::new();
    let mut live_files: Vec<(&u32, &Inode)> = table
        .iter()
        .filter(|(&ino, i)| {
            ino >= first_ino
                && i.is_file()
                && i.links > 0
                && i.flags & ENCRYPT_FL == 0
                && ext.inode_used(ino)
        })
        .collect();
    live_files.sort_by_key(|(ino, _)| **ino);
    for (&ino, inode) in live_files {
        let Some((dir, name)) = names.current.get(&(ino, false)) else {
            continue;
        };
        let extents = if inode.has_inline() {
            inode.inline.clone()
        } else {
            match ext.runs(inode, None) {
                Some(runs) => ext.to_extents(&runs, inode.size),
                None => continue,
            }
        };
        files.push(FsFile {
            dir: path_of(*dir),
            name: name.clone(),
            size: inode.size,
            modified: date(inode.mtime),
            created: inode.crtime.and_then(date),
            deleted: false,
            extents,
            overwritten: false,
            alternatives: Vec::new(),
        });
    }

    // Deleted files: the newest intact copy of each, by inode and
    // generation (an inode can have held several files since).
    let mut best: HashMap<(u32, u32), (u64, &Inode)> = HashMap::new();
    let journal_copies = logged
        .iter()
        .filter(|(ino, _, i)| i.links > 0 && i.dtime == 0 && gone(*ino, i))
        .map(|(ino, seq, i)| (*ino, u64::from(*seq) + 1, i));
    let table_copies = table
        .iter()
        .filter(|(&ino, _)| !ext.inode_used(ino))
        .map(|(&ino, i)| (ino, 0, i));
    for (ino, seq, inode) in journal_copies.chain(table_copies) {
        let usable = ino >= first_ino
            && inode.is_file()
            && inode.size > 0
            && inode.size <= sb.blocks * sb.block
            && inode.flags & ENCRYPT_FL == 0;
        if !usable {
            continue;
        }
        match best.get(&(ino, inode.generation)) {
            Some((s, _)) if *s >= seq => {}
            _ => {
                best.insert((ino, inode.generation), (seq, inode));
            }
        }
    }
    let mut deleted: Vec<_> = best.into_iter().collect();
    deleted.sort_by_key(|(k, _)| *k);
    // When each file each inode has held was first seen.
    let mut first_seen: HashMap<u32, HashMap<u32, u64>> = HashMap::new();
    for (ino, seq, inode) in &logged {
        let seen = first_seen
            .entry(*ino)
            .or_default()
            .entry(inode.generation)
            .or_insert(u64::MAX);
        *seen = (*seen).min(u64::from(*seq) + 1);
    }
    for (&ino, inode) in &table {
        let start = if ext.inode_used(ino) { ON_DISK } else { 0 };
        first_seen
            .entry(ino)
            .or_default()
            .entry(inode.generation)
            .or_insert(start);
    }
    let lives: HashMap<u32, Vec<(u64, u32)>> = first_seen
        .into_iter()
        .map(|(ino, gens)| {
            let mut v: Vec<(u64, u32)> = gens.into_iter().map(|(g, s)| (s, g)).collect();
            v.sort_unstable();
            (ino, v)
        })
        .collect();
    for ((ino, gen), (seq, inode)) in deleted {
        let (extents, overwritten) = if inode.has_inline() {
            (inode.inline.clone(), false)
        } else {
            let Some(runs) = ext.runs(inode, Some(seq)) else {
                continue;
            };
            let over = ext
                .data_blocks(&runs, inode.size)
                .any(|b| ext.block_used(b));
            (ext.to_extents(&runs, inode.size), over)
        };
        if extents.is_empty() {
            continue;
        }
        let (dir, name) = match names.file_name(ino, gen, lives.get(&ino).map_or(&[][..], |v| v)) {
            Some((_, d, n)) => (path_of(*d), n.clone()),
            None => (LOST.to_string(), format!("File {ino}")),
        };
        files.push(FsFile {
            dir,
            name,
            size: inode.size,
            modified: date(inode.mtime),
            created: inode.crtime.and_then(date),
            deleted: true,
            extents,
            overwritten,
            alternatives: Vec::new(),
        });
    }

    // Free space, for the deep scan.
    let mut free = Vec::new();
    let end = sb.blocks.min(len / sb.block);
    for b in sb.first_data_block..end {
        if !ext.block_used(b) {
            push_merged(&mut free, Extent::new(ext.block_offset(b), sb.block));
        }
    }

    Some(Volume {
        fs: kind,
        label: sb.label.clone(),
        offset,
        len,
        cluster_size: sb.block,
        files,
        free,
        apfs: None,
        names: Vec::new(),
        name_sources: Default::default(),
        snapshots: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn superblock() -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        let mut put = |at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0x00, 16384); // inodes
        put(0x04, 65536); // blocks
        put(0x14, 0); // first data block
        put(0x18, 2); // 4096-byte blocks
        put(0x20, 32768); // blocks per group
        put(0x28, 8192); // inodes per group
        put(0x4C, 1); // dynamic revision
        put(0x54, 11); // first inode
        put(0x5C, COMPAT_HAS_JOURNAL);
        put(0x60, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
        put(0x64, RO_SPARSE_SUPER);
        put(0xE0, 8);
        b[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        b[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes());
        b[0x78..0x7E].copy_from_slice(b"rootfs");
        b
    }

    #[test]
    fn parses_superblock_and_kind() {
        let b = superblock();
        let sb = Super::parse(&b).unwrap();
        assert_eq!(sb.block, 4096);
        assert_eq!(sb.groups, 2);
        assert_eq!(sb.inode_size, 256);
        assert_eq!(sb.label.as_deref(), Some("rootfs"));
        assert_eq!(sb.kind(), FsType::Ext4);
        assert_eq!(sb.desc_block(0), 1);
        assert_eq!(detect(&b), Some(FsType::Ext4));

        let mut ext3 = b.clone();
        ext3[0x60..0x64].copy_from_slice(&INCOMPAT_FILETYPE.to_le_bytes());
        assert_eq!(detect(&ext3), Some(FsType::Ext3));
        ext3[0x5C..0x60].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(detect(&ext3), Some(FsType::Ext2));

        let mut bad = b.clone();
        bad[0x18] = 9; // absurd block size
        assert!(Super::parse(&bad).is_none());
        let mut bad = b;
        bad[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes());
        assert!(Super::parse(&bad).is_none());
    }

    #[test]
    fn sparse_super_backups() {
        let sb = Super::parse(&superblock()).unwrap();
        let with: Vec<u64> = (0..30).filter(|&g| sb.has_super(g)).collect();
        assert_eq!(with, [0, 1, 3, 5, 7, 9, 25, 27]);
    }

    #[test]
    fn extent_nodes() {
        let mut b = vec![0u8; 60];
        b[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        b[2..4].copy_from_slice(&2u16.to_le_bytes());
        b[4..6].copy_from_slice(&4u16.to_le_bytes());
        // Logical 0, 10 blocks at 1000; logical 10, 5 unwritten at 2^32+7.
        b[12..16].copy_from_slice(&0u32.to_le_bytes());
        b[16..18].copy_from_slice(&10u16.to_le_bytes());
        b[20..24].copy_from_slice(&1000u32.to_le_bytes());
        b[24..28].copy_from_slice(&10u32.to_le_bytes());
        b[28..30].copy_from_slice(&(32768u16 + 5).to_le_bytes());
        b[30..32].copy_from_slice(&1u16.to_le_bytes());
        b[32..36].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            extent_node(&b),
            Some(Node::Leaf(vec![(0, 1000, 10), (10, (1 << 32) + 7, 5)]))
        );
        b[6..8].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            extent_node(&b),
            Some(Node::Index(
                1,
                vec![1000 << 32 | 10, 7 << 32 | 1 << 16 | (32768 + 5)]
            ))
        );
        b[2..4].copy_from_slice(&5u16.to_le_bytes()); // more entries than room
        assert_eq!(extent_node(&b), None);
        assert_eq!(extent_node(&[0u8; 60]), None);
    }

    fn dirent(b: &mut Vec<u8>, ino: u32, rec_len: u16, kind: u8, name: &[u8]) {
        b.extend(ino.to_le_bytes());
        b.extend(rec_len.to_le_bytes());
        b.push(name.len() as u8);
        b.push(kind);
        b.extend(name);
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
    }

    #[test]
    fn folder_entries_and_leftovers() {
        let mut b = Vec::new();
        dirent(&mut b, 2, 12, 2, b".");
        dirent(&mut b, 2, 12, 2, b"..");
        // "IMG_0001.JPG" was deleted: absorbed into "a.txt" before it.
        dirent(&mut b, 12, 36, 1, b"a.txt");
        dirent(&mut b, 13, 20, 1, b"IMG_0001.JPG");
        let rest = 128 - b.len() as u16;
        dirent(&mut b, 14, rest, 2, b"DCIM");
        b.resize(128, 0);
        let got = parse_dir(&b, true, 100).unwrap();
        let names: Vec<(&str, u32, bool)> = got
            .iter()
            .map(|d| (d.name.as_str(), d.ino, d.live))
            .collect();
        assert_eq!(
            names,
            [
                (".", 2, true),
                ("..", 2, true),
                ("a.txt", 12, true),
                ("IMG_0001.JPG", 13, false),
                ("DCIM", 14, true)
            ]
        );
        // Inode numbers beyond the volume mean this is not a folder.
        assert!(parse_dir(&b, true, 10).is_none());
        // Nor is a block whose entries do not chain to its end.
        let mut short = b.clone();
        short[64..66].copy_from_slice(&40u16.to_le_bytes());
        assert!(parse_dir(&short, true, 100).is_none());
        assert!(parse_dir(&[0u8; 128], true, 100).is_none());
    }

    #[test]
    fn journal_descriptor_tags() {
        // csum v3: 16-byte tags, a UUID after the first.
        let mut b = vec![0u8; 1024];
        b[0..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&1u32.to_be_bytes());
        b[12..16].copy_from_slice(&500u32.to_be_bytes());
        b[16..20].copy_from_slice(&TAG_ESCAPED.to_be_bytes());
        b[44..48].copy_from_slice(&501u32.to_be_bytes());
        b[48..52].copy_from_slice(&(TAG_SAME_UUID | TAG_LAST).to_be_bytes());
        assert_eq!(
            descriptor_tags(&b, JBD2_CSUM_V3),
            [(500, true), (501, false)]
        );

        // Old format, 32-bit: 8-byte tags.
        let mut b = vec![0u8; 1024];
        b[12..16].copy_from_slice(&7u32.to_be_bytes());
        b[36..40].copy_from_slice(&8u32.to_be_bytes());
        b[42..44].copy_from_slice(&((TAG_SAME_UUID | TAG_LAST) as u16).to_be_bytes());
        assert_eq!(descriptor_tags(&b, 0), [(7, false), (8, false)]);
    }

    #[test]
    fn inode_fields() {
        let mut raw = vec![0u8; 256];
        raw[0..2].copy_from_slice(&(S_IFREG | 0o644).to_le_bytes());
        raw[4..8].copy_from_slice(&100u32.to_le_bytes());
        raw[0x6C..0x70].copy_from_slice(&1u32.to_le_bytes());
        raw[0x10..0x14].copy_from_slice(&1_262_304_000u32.to_le_bytes());
        raw[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        raw[0x90..0x94].copy_from_slice(&1_000_000_000u32.to_le_bytes());
        let i = parse_inode(&raw, 0).unwrap();
        assert!(i.is_file());
        assert_eq!(i.size, (1 << 32) + 100);
        assert_eq!(i.mtime, 1_262_304_000);
        assert_eq!(i.crtime, Some(1_000_000_000));
        // Past 2038: the epoch bits of the extra field.
        assert_eq!(timestamp(0x8000_0000, Some(1)), (1i64 << 32) - (1i64 << 31));
        assert!(parse_inode(&[0u8; 256], 0).is_none());
    }

    #[test]
    fn inline_data() {
        let mut raw = vec![0u8; 256];
        raw[0..2].copy_from_slice(&(S_IFREG | 0o644).to_le_bytes());
        raw[4..8].copy_from_slice(&70u32.to_le_bytes());
        raw[0x20..0x24].copy_from_slice(&INLINE_DATA_FL.to_le_bytes());
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        let ibody = 160;
        raw[ibody..ibody + 4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
        let e = ibody + 4;
        raw[e] = 4;
        raw[e + 1] = 7;
        raw[e + 2..e + 4].copy_from_slice(&40u16.to_le_bytes());
        raw[e + 8..e + 12].copy_from_slice(&10u32.to_le_bytes());
        raw[e + 16..e + 20].copy_from_slice(b"data");
        let i = parse_inode(&raw, 1000).unwrap();
        assert_eq!(
            i.inline,
            [Extent::new(1040, 60), Extent::new(1000 + 164 + 40, 10)]
        );
    }
}

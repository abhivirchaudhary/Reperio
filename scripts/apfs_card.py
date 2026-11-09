"""An APFS container with one volume, written the way APFS writes.

Linux cannot write APFS, so this module builds the container itself from
Apple's File System Reference: container superblock and checkpoints,
object maps, the volume superblock, and the volume's directory tree.

The part that matters for recovery is copy-on-write: every transaction
writes new copies of the directory-tree nodes it changes, the object map
and the superblocks to free blocks, and frees the old copies without
clearing them. Blocks are handed out from a moving cursor, so freed
blocks are not reused straight away. Deleting a file from a card moves
it to .Trashes/501 and then empties the Trash, as the Finder does.

Snapshots (as Time Machine takes them) keep the directory tree, volume
superblock and file data of the moment they were taken: the volume's
object map keeps the tree's mappings at the snapshot's transaction, and
none of those blocks are freed while the snapshot exists.

The container is cross-checked with libfsapfs (pyfsapfs) in the tests.
"""
import struct
import unicodedata

BLOCK = 4096
NODE_ROOT, NODE_LEAF, NODE_FIXED = 1, 2, 4
T_NX, T_BTREE, T_NODE, T_OMAP, T_CPM, T_FS, T_FSTREE = 0x1, 0x2, 0x3, 0xB, 0xC, 0xD, 0xE
T_SNAPMETATREE = 0x10
J_SNAP_METADATA = 1
PHYS, EPHEMERAL = 0x40000000, 0x80000000
J_INODE, J_DSTREAM_ID, J_EXTENT, J_DREC = 3, 6, 8, 9
VOL_OID, ROOT_TREE_OID = 1026, 1027
DESC_BASE, DESC_BLOCKS = 1, 8
DATA_BASE, DATA_BLOCKS = DESC_BASE + DESC_BLOCKS, 16
NS = 1_000_000_000


def fletcher(block):
    s1 = s2 = 0
    m = 0xFFFFFFFF
    for (w,) in struct.iter_unpack("<I", block[8:]):
        s1 = (s1 + w) % m
        s2 = (s2 + s1) % m
    c1 = m - (s1 + s2) % m
    c2 = m - (s1 + c1) % m
    return (c2 << 32) | c1


def obj(oid, xid, typ, subtype, body):
    b = bytearray(BLOCK)
    struct.pack_into("<QQII", b, 8, oid, xid, typ, subtype)
    b[32:32 + len(body)] = body
    struct.pack_into("<Q", b, 0, fletcher(b))
    return bytes(b)


def crc32c(data, crc=0xFFFFFFFF):
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc


def drec_key(parent, name):
    norm = unicodedata.normalize("NFD", name).casefold()
    h = crc32c(norm.encode("utf-32-le")) & 0x3FFFFF
    raw = name.encode() + b"\0"
    return struct.pack("<QI", parent | (J_DREC << 60), (h << 10) | len(raw)) + raw, (parent, J_DREC, h, raw)


def btree_node(oid, xid, typ, subtype, flags, level, entries, fixed=None, info=None):
    """entries: list of (key, value) bytes. Root nodes carry `info`."""
    body = bytearray(BLOCK - 32)
    n = len(entries)
    toc_len = n * (4 if fixed else 8)
    keys_at = 24 + toc_len  # relative to body start (btn_data at 56)
    vals_end = (BLOCK - 32) - (40 if flags & NODE_ROOT else 0)
    k_off, v_off = 0, 0
    toc = bytearray()
    for k, v in entries:
        body[keys_at + k_off:keys_at + k_off + len(k)] = k
        v_off += len(v)
        body[vals_end - v_off:vals_end - v_off + len(v)] = v
        if fixed:
            toc += struct.pack("<HH", k_off, v_off)
        else:
            toc += struct.pack("<HHHH", k_off, len(k), v_off, len(v))
        k_off += len(k)
    assert keys_at + k_off <= vals_end - v_off, "node overflow"
    body[24:24 + len(toc)] = toc
    free_off, free_len = k_off, (vals_end - v_off) - (keys_at + k_off)
    struct.pack_into("<HHIHHHHHHHH", body, 0, flags, level, n, 0, toc_len, free_off, free_len,
                     0xFFFF, 0, 0xFFFF, 0)
    if info:
        body[BLOCK - 32 - 40:] = info
    return obj(oid, xid, typ, subtype, bytes(body))


def inode_val(parent, private, t, mode, name, size=None, nchildren=0):
    v = struct.pack("<QQQQQQQiIIIIIHHQ", parent, private, t, t, t, t, 0, nchildren, 0, 0, 0, 501, 20, mode, 0, 0)
    fields = [(4, 0x02, name.encode() + b"\0")]
    if size is not None:
        alloc = -(-size // BLOCK) * BLOCK
        fields.append((8, 0x20, struct.pack("<QQQQQ", size, alloc, 0, size, 0)))
    table = b"".join(struct.pack("<BBH", typ, fl, len(d)) for typ, fl, d in fields)
    data = b"".join(d.ljust(-(-len(d) // 8) * 8, b"\0") for _, _, d in fields)
    return v + struct.pack("<HH", len(fields), len(data)) + table + data


class ApfsCard:
    """Same interface as the other test cards in make_test_cards.py."""

    def __init__(self, image, size, label="Untitled", date=1262304000):
        self.image = image
        self.size = size
        self.blocks = size // BLOCK
        self.label = label
        self.t = date * NS
        self.xid = 1
        self.used = bytearray(self.blocks)
        for b in range(DATA_BASE + DATA_BLOCKS):
            self.used[b] = 1
        self.cursor = DATA_BASE + DATA_BLOCKS
        self.disk = {}  # block -> bytes, the card's contents
        self.records = {}  # sort key -> (key, value)
        self.next_id = 16
        self.dirs = {"": 2}
        self.files = {}  # path -> (id, parent, extents, size)
        self.nchildren = {2: 0}
        self.meta = []  # blocks of the current metadata, freed on the next commit
        self.desc_index = 0
        self.snapshots = []  # (name, xid, time, volume superblock, tree mappings)
        self.held = set()  # blocks snapshots keep
        self.tree = []  # the current tree's (oid, block) mappings
        self.vsb = None
        self.put_inode(2, 1, 0o40755, "root")
        self.put_inode(3, 1, 0o40755, "private-dir")
        self.put_drec(1, "root", 2, 4)
        self.put_drec(1, "private-dir", 3, 4)
        self.commit()

    # -- allocation -----------------------------------------------------
    def alloc(self, count):
        out = []
        scanned = 0
        while count and scanned < self.blocks:
            b = self.cursor
            self.cursor = self.cursor + 1 if self.cursor + 1 < self.blocks else DATA_BASE + DATA_BLOCKS
            scanned += 1
            if self.used[b]:
                continue
            self.used[b] = 1
            if out and out[-1][0] + out[-1][1] == b:
                out[-1][1] += 1
            else:
                out.append([b, 1])
            count -= 1
        assert count == 0, "card full"
        return [tuple(e) for e in out]

    def write_meta(self, data):
        (b, _), = self.alloc(1)
        self.disk[b] = data
        self.meta.append(b)
        return b

    # -- records --------------------------------------------------------
    def put_inode(self, fid, parent, mode, name, size=None):
        key = struct.pack("<Q", fid | (J_INODE << 60))
        val = inode_val(parent, fid, self.t, mode, name, size, self.nchildren.get(fid, 0))
        self.records[(fid, J_INODE)] = (key, val)

    def put_drec(self, parent, name, fid, kind):
        key, sort = drec_key(parent, name)
        self.records[sort] = (key, struct.pack("<QQH", fid, self.t, kind))

    def drop_drec(self, parent, name):
        del self.records[drec_key(parent, name)[1]]

    def refresh_dir(self, did, parent, name):
        self.put_inode(did, parent, 0o40755, name)

    def split(self, path):
        d, _, n = path.rpartition("/")
        return self.dirs[d], n

    def parent_info(self, did):
        for p, i in self.dirs.items():
            if i == did:
                d, _, n = p.rpartition("/")
                return (self.dirs[d], n) if p else (1, "root")

    def touch_dir(self, did, delta):
        self.nchildren[did] = self.nchildren.get(did, 0) + delta
        parent, name = self.parent_info(did)
        self.refresh_dir(did, parent, name)

    # -- operations -----------------------------------------------------
    def new_id(self):
        self.next_id += 1
        return self.next_id - 1

    def mkdir(self, path):
        parent, name = self.split(path)
        did = self.new_id()
        self.dirs[path] = did
        self.nchildren[did] = 0
        self.put_inode(did, parent, 0o40755, name)
        self.put_drec(parent, name, did, 4)
        self.touch_dir(parent, 1)
        self.commit()

    def copy(self, src, path):
        data = open(src, "rb").read()
        parent, name = self.split(path)
        extents = self.alloc(-(-len(data) // BLOCK))
        pos = 0
        for s, n in extents:
            for b in range(s, s + n):
                self.disk[b] = data[pos:pos + BLOCK]
                pos += BLOCK
        fid = self.new_id()
        self.files[path] = (fid, parent, extents, len(data))
        self.put_inode(fid, parent, 0o100644, name, len(data))
        self.records[(fid, J_DSTREAM_ID)] = (struct.pack("<Q", fid | (J_DSTREAM_ID << 60)), struct.pack("<I", 1))
        logical = 0
        for s, n in extents:
            key = struct.pack("<QQ", fid | (J_EXTENT << 60), logical)
            self.records[(fid, J_EXTENT, logical)] = (key, struct.pack("<QQQ", n * BLOCK, s, 0))
            logical += n * BLOCK
        self.put_drec(parent, name, fid, 8)
        self.touch_dir(parent, 1)
        self.commit()

    def trash(self):
        if ".Trashes" not in self.dirs:
            self.mkdir(".Trashes")
            self.mkdir(".Trashes/501")
        return ".Trashes/501"

    def move(self, path, to_dir):
        parent, name = self.split(path)
        new_parent = self.dirs[to_dir]
        new_path = to_dir + "/" + name
        self.drop_drec(parent, name)
        if path in self.files:
            fid, _, extents, size = self.files.pop(path)
            self.files[new_path] = (fid, new_parent, extents, size)
            self.put_inode(fid, new_parent, 0o100644, name, size)
            self.put_drec(new_parent, name, fid, 8)
        else:
            did = self.dirs.pop(path)
            for p in [p for p in self.files if p.startswith(path + "/")]:
                self.files[new_path + p[len(path):]] = self.files.pop(p)
            for p in [p for p in self.dirs if p.startswith(path + "/")]:
                self.dirs[new_path + p[len(path):]] = self.dirs.pop(p)
            self.dirs[new_path] = did
            self.put_inode(did, new_parent, 0o40755, name)
            self.put_drec(new_parent, name, did, 4)
        self.touch_dir(parent, -1)
        self.touch_dir(new_parent, 1)
        self.commit()
        return new_path

    def remove(self, path):
        parent, name = self.split(path)
        if path in self.files:
            fid, _, extents, _ = self.files.pop(path)
            for s, n in extents:
                for b in range(s, s + n):
                    if b not in self.held:
                        self.used[b] = 0
            for k in [k for k in self.records if k[0] == fid]:
                del self.records[k]
        else:
            did = self.dirs.pop(path)
            del self.records[(did, J_INODE)]
        self.drop_drec(parent, name)
        self.touch_dir(parent, -1)
        self.commit()

    def delete(self, path):
        """Move to Trash, then Empty Trash."""
        self.remove(self.move(path, self.trash()))

    def delete_tree(self, path):
        moved = self.move(path, self.trash())
        for p in sorted([p for p in self.files if p.startswith(moved + "/")], reverse=True):
            self.remove(p)
        for p in sorted([p for p in self.dirs if p.startswith(moved + "/")], reverse=True):
            self.remove(p)
        self.remove(moved)

    # -- transactions ---------------------------------------------------
    def commit(self):
        self.xid += 1
        xid = self.xid
        # Copy-on-write: the previous transaction's metadata blocks become
        # free, but keep their contents.
        for b in self.meta:
            if b not in self.held:
                self.used[b] = 0
        self.meta = []

        entries = [self.records[k] for k in sorted(self.records)]
        leaves, cur, size = [], [], 0
        cap = BLOCK - 56 - 40 - 64
        for k, v in entries:
            if size + len(k) + len(v) + 8 > cap:
                leaves.append(cur)
                cur, size = [], 0
            cur.append((k, v))
            size += len(k) + len(v) + 8
        leaves.append(cur)
        fs_info = lambda nodes: struct.pack("<IIIIIIQQ", 0x40, BLOCK, 0, 0, 0, 0, len(entries), nodes)
        omap_entries = []
        if len(leaves) == 1:
            node = btree_node(ROOT_TREE_OID, xid, T_BTREE, T_FSTREE, NODE_ROOT | NODE_LEAF, 0,
                              leaves[0], info=fs_info(1))
            omap_entries.append((ROOT_TREE_OID, self.write_meta(node)))
        else:
            index = []
            for i, recs in enumerate(leaves):
                oid = ROOT_TREE_OID + 1 + i
                node = btree_node(oid, xid, T_NODE, T_FSTREE, NODE_LEAF, 0, recs)
                omap_entries.append((oid, self.write_meta(node)))
                index.append((recs[0][0], struct.pack("<Q", oid)))
            root = btree_node(ROOT_TREE_OID, xid, T_BTREE, T_FSTREE, NODE_ROOT, 1, index,
                              info=fs_info(1 + len(leaves)))
            omap_entries.append((ROOT_TREE_OID, self.write_meta(root)))
        self.tree = omap_entries
        kept = [(oid, sx, b) for _, sx, _, _, tree in self.snapshots for oid, b in tree]
        vol_omap = self.write_omap(xid, omap_entries, kept)
        snap_tree = self.write_snap_tree(xid) if self.snapshots else 0

        nfiles = len(self.files)
        apsb = bytearray(BLOCK - 32)
        struct.pack_into("<4sIQQQQQQQ", apsb, 0, b"APSB", 0, 0, 0, 1, self.t, 0, 0, 0)
        struct.pack_into("<III", apsb, 116 - 32, T_BTREE, PHYS | T_BTREE, PHYS | T_BTREE)
        struct.pack_into("<Q", apsb, 216 - 32, len(self.snapshots))
        struct.pack_into("<QQQQQQQQQQ", apsb, 128 - 32, vol_omap, ROOT_TREE_OID, 0, snap_tree, 0, 0,
                         self.next_id, nfiles, len(self.dirs), 0)
        struct.pack_into("<Q", apsb, 264 - 32, 1)  # unencrypted
        apsb[704 - 32:704 - 32 + len(self.label)] = self.label.encode()
        vsb = self.write_meta(obj(VOL_OID, xid, T_FS, 0, bytes(apsb)))
        self.vsb = vsb
        nx_omap = self.write_omap(xid, [(VOL_OID, vsb)])

        nx = bytearray(BLOCK - 32)
        struct.pack_into("<4sIQQQQ16sQQ", nx, 0, b"NXSB", BLOCK, self.blocks, 0, 0, 2, bytes(16), 2000, xid + 1)
        struct.pack_into("<IIQQIIIIII", nx, 104 - 32, DESC_BLOCKS, DATA_BLOCKS, DESC_BASE, DATA_BASE,
                         0, 0, self.desc_index, 2, 0, 0)
        struct.pack_into("<QQQII", nx, 152 - 32, 0, nx_omap, 0, 0, 100)
        struct.pack_into("<Q", nx, 184 - 32, VOL_OID)
        cpm = obj(self.desc_index + DESC_BASE, xid, PHYS | T_CPM, 0, struct.pack("<II", 1, 0))
        self.disk[DESC_BASE + self.desc_index] = cpm
        self.disk[DESC_BASE + (self.desc_index + 1) % DESC_BLOCKS] = obj(1, xid, EPHEMERAL | T_NX, 0, bytes(nx))
        self.desc_index = (self.desc_index + 2) % DESC_BLOCKS
        self.disk[0] = obj(1, xid, EPHEMERAL | T_NX, 0, bytes(nx))

    def snapshot(self, name):
        """Takes a snapshot of the volume as it is now, as Time Machine
        does before a backup."""
        held = {b for _, b in self.tree} | {self.vsb}
        for _, _, extents, _ in self.files.values():
            for s, n in extents:
                held.update(range(s, s + n))
        self.held |= held
        self.snapshots.append((name, self.xid, self.t, self.vsb, list(self.tree)))
        self.commit()

    def write_snap_tree(self, xid):
        entries = []
        for name, sx, t, vsb, _ in sorted(self.snapshots, key=lambda s: s[1]):
            raw = name.encode() + b"\0"
            key = struct.pack("<Q", sx | (J_SNAP_METADATA << 60))
            val = struct.pack("<QQQQQIIH", 0, vsb, t, t, 0, PHYS | T_BTREE, 0, len(raw)) + raw
            entries.append((key, val))
        info = struct.pack("<IIIIIIQQ", 0, BLOCK, 0, 0, 0, 0, len(entries), 1)
        (b, _), = self.alloc(1)
        self.disk[b] = btree_node(b, xid, PHYS | T_BTREE, T_SNAPMETATREE, NODE_ROOT | NODE_LEAF, 0,
                                  entries, info=info)
        self.meta.append(b)
        return b

    def write_omap(self, xid, mapping, kept=()):
        """`kept`: (oid, xid, block) mappings snapshots still need."""
        triples = sorted([(oid, xid, paddr) for oid, paddr in mapping] + list(kept))
        leaf_entries = [(struct.pack("<QQ", oid, x), struct.pack("<IIQ", 0, BLOCK, paddr))
                        for oid, x, paddr in triples]
        info = struct.pack("<IIIIIIQQ", 0x10, BLOCK, 16, 16, 16, 16, len(leaf_entries), 1)
        (tb, _), = self.alloc(1)
        self.disk[tb] = btree_node(tb, xid, PHYS | T_BTREE, T_OMAP, NODE_ROOT | NODE_LEAF | NODE_FIXED, 0,
                                   leaf_entries, fixed=True, info=info)
        (ob, _), = self.alloc(1)
        body = struct.pack("<IIIIQ", 0, 0, PHYS | T_BTREE, PHYS | T_BTREE, tb)
        self.disk[ob] = obj(ob, xid, PHYS | T_OMAP, 0, body)
        self.meta += [tb, ob]
        return ob

    def close(self):
        with open(self.image, "wb") as f:
            f.truncate(self.size)
            for b, data in sorted(self.disk.items()):
                f.seek(b * BLOCK)
                f.write(data)


def quick_erase(image, offset, size):
    """Erases the container at `offset` as Disk Utility does: new
    superblocks, object maps and an empty directory tree, written over the
    start of the container; everything else is left where it was."""
    import tempfile
    with tempfile.TemporaryDirectory() as tmp:
        fresh = ApfsCard(tmp + "/fresh.img", size)
        fresh.close()
        with open(tmp + "/fresh.img", "rb") as f:
            head = f.read(fresh.cursor * BLOCK)
    with open(image, "r+b") as f:
        f.seek(offset)
        f.write(head)


if __name__ == "__main__":
    # A card with two Time Machine snapshots that still hold files deleted
    # from the live volume since (gone.bin is in both, later.bin in the
    # newer one only):
    #   apfs_card.py --snapshot-demo card.img kept-file deleted-file
    import sys
    if len(sys.argv) != 5 or sys.argv[1] != "--snapshot-demo":
        sys.exit("usage: apfs_card.py --snapshot-demo IMAGE KEPT DELETED")
    image, kept, gone = sys.argv[2:]
    card = ApfsCard(image, 8 << 20)
    card.mkdir("Documents")
    card.copy(kept, "Documents/kept.bin")
    card.copy(gone, "Documents/gone.bin")
    card.snapshot("com.apple.TimeMachine.2024-05-01-120000.local")
    card.copy(kept, "Documents/later.bin")
    card.snapshot("com.apple.TimeMachine.2024-05-02-120000.local")
    card.delete("Documents/later.bin")
    card.delete("Documents/gone.bin")
    # Written after the delete: with the snapshot holding the deleted
    # file's blocks, this goes elsewhere instead of over them.
    card.copy(kept, "Documents/after.bin")
    card.close()

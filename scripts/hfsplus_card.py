"""A Mac OS Extended (HFS+, journaled) volume built the way macOS writes one.

Linux cannot write HFS+ (and the hfsprogs mkfs on Ubuntu writes broken
volumes), so this module builds the volume itself from Apple's TN1150:
volume header, allocation bitmap, extents and catalog B-trees, and a
journal. Every change to the catalog is journaled first, as macOS does,
and deleting a file from a card moves it to .Trashes/501 and then empties
the Trash, as the Finder does. Records removed from a B-tree node close
up without clearing the bytes left behind, as in Apple's B-tree code.

The volume is cross-checked with libfshfs (pyfshfs) in the tests.
"""
import struct
import unicodedata

BLOCK = 4096
NODE = 4096
HFS_EPOCH = 2082844800
ROOT_ID = 2
FIRST_ID = 16
JOURNAL_SIZE = 1 << 20

FOLDER, FILE, FOLDER_THREAD, FILE_THREAD = 1, 2, 3, 4


def hfs_name(name):
    units = unicodedata.normalize("NFD", name).encode("utf-16-be")
    return struct.pack(">H", len(units) // 2) + units


def cat_key(parent, name):
    body = struct.pack(">I", parent) + hfs_name(name)
    return struct.pack(">H", len(body)) + body


def sort_key(parent, name):
    return (parent, unicodedata.normalize("NFD", name).casefold())


def fork(size, extents):
    blocks = sum(n for _, n in extents)
    ext = b"".join(struct.pack(">II", s, n) for s, n in extents[:8])
    return struct.pack(">QII", size, BLOCK, blocks) + ext.ljust(64, b"\0")


def node(kind, height, records, flink=0, blink=0, old=None):
    buf = bytearray(old if old else bytes(NODE))
    struct.pack_into(">IIbBHH", buf, 0, flink, blink, kind, height, len(records), 0)
    pos = 14
    offsets = []
    for r in records:
        offsets.append(pos)
        buf[pos:pos + len(r)] = r
        pos += len(r)
    offsets.append(pos)
    assert pos + 2 * len(offsets) <= NODE, "node overflow"
    for i, o in enumerate(offsets):
        struct.pack_into(">H", buf, NODE - 2 * (i + 1), o)
    return bytes(buf)


def header_node(depth, root, leaf_records, first, last, total, free, max_key, attrs, used):
    rec = struct.pack(">HIIIIHHIIHIBBI", depth, root, leaf_records, first, last, NODE, max_key,
                      total, free, 0, BLOCK, 0, 0xCF, attrs).ljust(106, b"\0")
    user = bytes(128)
    bitmap = bytearray(NODE - 14 - 106 - 128 - 8)
    for n in range(used):
        bitmap[n // 8] |= 0x80 >> (n % 8)
    return node(1, 0, [rec, user, bytes(bitmap)])


class HfsCard:
    """Same interface as the other test cards in make_test_cards.py."""

    def __init__(self, image, size, label="Untitled", date=1262304000):
        self.image = image
        self.size = size
        self.label = label
        self.now = date + HFS_EPOCH
        self.blocks = size // BLOCK
        self.used = bytearray(self.blocks)
        self.next_id = FIRST_ID
        self.data = {}  # block -> bytes
        # Fixed layout, as newfs_hfs lays it out.
        self.bitmap_start, self.bitmap_blocks = 1, max(1, -(-self.blocks // (8 * BLOCK)))
        self.jib = self.bitmap_start + self.bitmap_blocks
        self.journal = self.jib + 1
        self.journal_blocks = JOURNAL_SIZE // BLOCK
        self.ext_start = self.journal + self.journal_blocks
        self.ext_blocks = 4
        self.cat_start = self.ext_start + self.ext_blocks
        self.cat_blocks = 64
        for b in range(0, self.cat_start + self.cat_blocks):
            self.used[b] = 1
        self.used[self.blocks - 1] = 1  # alternate volume header
        self.cat_nodes = [bytes(NODE)] * self.cat_blocks
        self.journal_buf = bytearray(JOURNAL_SIZE)
        self.journal_pos = BLOCK
        self.files = {}  # path -> (id, parent, extents, size)
        self.dirs = {"": ROOT_ID}
        self.records = {}  # sort key -> record bytes
        self.valence = {ROOT_ID: 0}
        self.add_folder(1, label, ROOT_ID)
        self.commit()

    # -- catalog records ------------------------------------------------
    def folder_rec(self, fid):
        return struct.pack(">hHIIIIIII", FOLDER, 0, self.valence.get(fid, 0), fid,
                           self.now, self.now, self.now, self.now, 0) + bytes(16 + 16 + 16) + bytes(8)

    def file_rec(self, fid, size, extents):
        perms = struct.pack(">IIBBHI", 501, 20, 0, 0, 0o100644, 1)
        return (struct.pack(">hHIIIIIII", FILE, 2, 0, fid, self.now, self.now, self.now, self.now, 0)
                + perms + bytes(32) + bytes(8) + fork(size, extents) + fork(0, []))

    def thread(self, kind, parent, name):
        return struct.pack(">hhI", kind, 0, parent) + hfs_name(name)

    def put(self, parent, name, data):
        self.records[sort_key(parent, name)] = cat_key(parent, name) + data

    def drop(self, parent, name):
        del self.records[sort_key(parent, name)]

    def add_folder(self, parent, name, fid):
        self.valence[fid] = self.valence.get(fid, 0)
        self.put(parent, name, self.folder_rec(fid))
        self.put(fid, "", self.thread(FOLDER_THREAD, parent, name))

    def refresh_folder(self, fid):
        for k, rec in list(self.records.items()):
            if k[1] == "" and k[0] == fid:
                _, _, parent = struct.unpack(">hhI", rec[8:16])
                name_len = struct.unpack(">H", rec[16:18])[0]
                name = rec[18:18 + 2 * name_len].decode("utf-16-be")
                if fid != ROOT_ID or parent == 1:
                    self.put(parent, name, self.folder_rec(fid))

    # -- allocation -----------------------------------------------------
    def allocate(self, count):
        """First fit, so files go around others, as on a well used card."""
        out = []
        b = 0
        while count and b < self.blocks:
            if not self.used[b]:
                self.used[b] = 1
                if out and out[-1][0] + out[-1][1] == b:
                    out[-1][1] += 1
                else:
                    out.append([b, 1])
                count -= 1
            b += 1
        assert count == 0, "card full"
        return [tuple(e) for e in out]

    # -- operations -----------------------------------------------------
    def split(self, path):
        d, _, n = path.rpartition("/")
        return self.dirs[d], n

    def mkdir(self, path):
        parent, name = self.split(path)
        fid = self.new_id()
        self.dirs[path] = fid
        self.add_folder(parent, name, fid)
        self.valence[parent] += 1
        self.refresh_folder(parent)
        self.commit()

    def new_id(self):
        self.next_id += 1
        return self.next_id - 1

    def copy(self, src, path):
        data = open(src, "rb").read()
        parent, name = self.split(path)
        extents = self.allocate(-(-len(data) // BLOCK))
        pos = 0
        for s, n in extents:
            for b in range(s, s + n):
                self.data[b] = data[pos:pos + BLOCK]
                pos += BLOCK
        fid = self.new_id()
        self.files[path] = (fid, parent, extents, len(data))
        self.put(parent, name, self.file_rec(fid, len(data), extents))
        self.put(fid, "", self.thread(FILE_THREAD, parent, name))
        self.valence[parent] += 1
        self.refresh_folder(parent)
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
        rec = self.records[sort_key(parent, name)]
        self.drop(parent, name)
        if path in self.files:
            fid, _, extents, size = self.files.pop(path)
            self.files[new_path] = (fid, new_parent, extents, size)
            self.put(fid, "", self.thread(FILE_THREAD, new_parent, name))
        else:
            fid = self.dirs.pop(path)
            self.dirs[new_path] = fid
            self.put(fid, "", self.thread(FOLDER_THREAD, new_parent, name))
            for p in [p for p in self.files if p.startswith(path + "/")]:
                f = self.files.pop(p)
                self.files[new_path + p[len(path):]] = f
            for p in [p for p in self.dirs if p.startswith(path + "/")]:
                self.dirs[new_path + p[len(path):]] = self.dirs.pop(p)
        self.records[sort_key(new_parent, name)] = cat_key(new_parent, name) + rec[2 + struct.unpack(">H", rec[:2])[0]:]
        self.valence[parent] -= 1
        self.valence[new_parent] += 1
        self.refresh_folder(parent)
        self.refresh_folder(new_parent)
        self.commit()
        return new_path

    def remove(self, path):
        parent, name = self.split(path)
        if path in self.files:
            fid, _, extents, _ = self.files.pop(path)
            for s, n in extents:
                for b in range(s, s + n):
                    self.used[b] = 0
        else:
            fid = self.dirs.pop(path)
        self.drop(parent, name)
        self.drop(fid, "")
        self.valence[parent] -= 1
        self.refresh_folder(parent)
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

    # -- B-tree and journal ---------------------------------------------
    def build_catalog(self):
        records = [self.records[k] for k in sorted(self.records)]
        leaves, cur, size = [], [], 14 + 2
        for r in records:
            if size + len(r) + 2 > NODE:
                leaves.append(cur)
                cur, size = [], 14 + 2
            cur.append(r)
            size += len(r) + 2
        leaves.append(cur)
        nodes = list(self.cat_nodes)
        n_leaves = len(leaves)
        for i, recs in enumerate(leaves):
            num = 1 + i
            flink = num + 1 if i + 1 < n_leaves else 0
            blink = num - 1 if i else 0
            nodes[num] = node(-1, 1, recs, flink, blink, old=self.cat_nodes[num])
        if n_leaves == 1:
            depth, root, used = 1, 1, 2
        else:
            index = []
            for i, recs in enumerate(leaves):
                key = recs[0][:2 + struct.unpack(">H", recs[0][:2])[0]]
                index.append(key + struct.pack(">I", 1 + i))
            root = 1 + n_leaves
            nodes[root] = node(0, 2, index, old=self.cat_nodes[root])
            depth, used = 2, root + 1
        nodes[0] = header_node(depth, root, len(records), 1, n_leaves, self.cat_blocks,
                               self.cat_blocks - used, 516, 6, used)
        return nodes

    def commit(self):
        """Writes the transaction's changed catalog nodes to the journal."""
        nodes = self.build_catalog()
        changed = [(i, n) for i, n in enumerate(nodes) if n != self.cat_nodes[i]]
        self.cat_nodes = nodes
        if not changed:
            return
        blhdr = bytearray(BLOCK)
        struct.pack_into("<HHII", blhdr, 0, 1 + len(changed), 1 + len(changed),
                         BLOCK * (1 + len(changed)), 0)
        for k, (i, _) in enumerate(changed):
            bnum = (self.cat_start + i) * (BLOCK // 512)
            struct.pack_into("<QII", blhdr, 16 + 16 * (k + 1), bnum, NODE, 0)
        self.journal_write(bytes(blhdr))
        for _, n in changed:
            self.journal_write(n)

    def journal_write(self, block):
        if self.journal_pos + len(block) > JOURNAL_SIZE:
            self.journal_pos = BLOCK
        self.journal_buf[self.journal_pos:self.journal_pos + len(block)] = block
        self.journal_pos += len(block)

    def close(self):
        img = bytearray(self.size)

        def put_block(b, data):
            img[b * BLOCK:b * BLOCK + len(data)] = data

        for b, d in self.data.items():
            put_block(b, d)
        bitmap = bytearray(self.bitmap_blocks * BLOCK)
        for b in range(self.blocks):
            if self.used[b]:
                bitmap[b // 8] |= 0x80 >> (b % 8)
        put_block(self.bitmap_start, bitmap)
        # Journal info block, then the journal: a clean header (start ==
        # end, nothing to replay) over a buffer that still holds old
        # transactions.
        jib = struct.pack(">I", 1) + bytes(32) + struct.pack(">QQ", self.journal * BLOCK, JOURNAL_SIZE)
        put_block(self.jib, jib)
        jhdr = struct.pack("<IIQQQIII", 0x4A4E4C78, 0x12345678, self.journal_pos, self.journal_pos,
                           JOURNAL_SIZE, BLOCK, 0, 512)
        self.journal_buf[:len(jhdr)] = jhdr
        put_block(self.journal, self.journal_buf)
        ext_tree = header_node(0, 0, 0, 0, 0, self.ext_blocks, self.ext_blocks - 1, 10, 2, 1)
        put_block(self.ext_start, ext_tree)
        for i, n in enumerate(self.cat_nodes):
            put_block(self.cat_start + i, n)

        free = self.blocks - sum(self.used)
        files = len(self.files)
        folders = len(self.dirs) - 1
        vh = struct.pack(">2sHI4sIIIIIIIIIIIIIIIQ", b"H+", 4, 0x80002100 & ~0x80000000, b"HFSJ", self.jib,
                         self.now, self.now, 0, 0, files, folders, BLOCK, self.blocks, free,
                         0, 65536, 65536, self.next_id, 1, 1)
        vh += bytes(32)  # Finder info
        vh += fork(self.bitmap_blocks * BLOCK, [(self.bitmap_start, self.bitmap_blocks)])
        vh += fork(self.ext_blocks * BLOCK, [(self.ext_start, self.ext_blocks)])
        vh += fork(self.cat_blocks * BLOCK, [(self.cat_start, self.cat_blocks)])
        vh += fork(0, [])  # attributes
        vh += fork(0, [])  # startup
        assert len(vh) == 512, len(vh)
        img[1024:1536] = vh
        img[self.size - 1024:self.size - 512] = vh
        with open(self.image, "wb") as f:
            f.write(img)


def quick_erase(image, offset, size):
    """Erases the volume at `offset` as Disk Utility does: new volume
    header, bitmap, journal and B-trees; file data is left where it was."""
    import tempfile
    with tempfile.TemporaryDirectory() as tmp:
        fresh = HfsCard(tmp + "/fresh.img", size)
        fresh.close()
        meta = (fresh.cat_start + fresh.cat_blocks) * BLOCK
        with open(tmp + "/fresh.img", "rb") as f:
            head = f.read(meta)
            f.seek(size - BLOCK)
            tail = f.read(BLOCK)
    with open(image, "r+b") as f:
        f.seek(offset)
        f.write(head)
        f.seek(offset + size - BLOCK)
        f.write(tail)

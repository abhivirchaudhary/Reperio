"""Plants what Windows would leave behind on an ntfs-3g volume, for
scripts/ntfs_names_check.sh. ntfs-3g keeps no $LogFile history and no
change journal, so these are written the way Windows writes them.

    scripts/ntfs_names.py capture IMG STATE.json
        Before the delete: saves the file records of Holiday/victim.bin and
        Holiday/small.txt as NTFS logs them, and the references of the
        folders involved.
    scripts/ntfs_names.py usn MOUNT STATE.json
        Writes change journal records into the stream usnstage:$J (sparse
        before them): the folder Holiday, the deletions of its files, and
        of lost_note.txt, which leaves no other trace.
    scripts/ntfs_names.py plant IMG STATE.json
        After the delete and the reuse of those records: writes $LogFile
        pages holding "free this record" steps whose undo halves are the
        saved records, and turns usnstage into $Extend\\$UsnJrnl.
"""
import json, struct, sys, time

SEQ_BITS = 30
PAGE = 4096


class Volume:
    def __init__(self, path, mode="rb"):
        self.f = open(path, mode)
        boot = self.rd(0, 512)
        sector = struct.unpack_from("<H", boot, 11)[0]
        self.cluster = sector * boot[13]
        cpr = struct.unpack_from("<b", boot, 64)[0]
        self.rsize = 1 << -cpr if cpr < 0 else cpr * self.cluster
        self.mft = struct.unpack_from("<Q", boot, 48)[0] * self.cluster
        rec0 = self.unfix(self.rd(self.mft, self.rsize))
        self.mft_runs = [r for t, r in self.data_runs(rec0) if t == ""][0]

    def rd(self, at, n):
        self.f.seek(at)
        return bytearray(self.f.read(n))

    def wr(self, at, b):
        self.f.seek(at)
        self.f.write(b)

    def record_at(self, n):
        """Disk offset of record n, following the MFT's runs."""
        pos = n * self.rsize
        for lcn, count in self.mft_runs:
            size = count * self.cluster
            if pos < size:
                return lcn * self.cluster + pos
            pos -= size
        return None

    @staticmethod
    def unfix(r):
        if r[:4] != b"FILE":
            return None
        usa, cnt = struct.unpack_from("<HH", r, 4)
        for i in range(1, cnt):
            e = i * 512
            r[e - 2:e] = r[usa + 2 * i:usa + 2 * i + 2]
        return r

    @staticmethod
    def fix(r):
        usa, cnt = struct.unpack_from("<HH", r, 4)
        usn = r[usa:usa + 2]
        for i in range(1, cnt):
            e = i * 512
            r[usa + 2 * i:usa + 2 * i + 2] = r[e - 2:e]
            r[e - 2:e] = usn
        return r

    @staticmethod
    def attrs(r):
        pos = struct.unpack_from("<H", r, 20)[0]
        while pos + 8 <= len(r):
            t, l = struct.unpack_from("<II", r, pos)
            if t == 0xFFFFFFFF or l < 16:
                return
            yield pos, t, l
            pos += l

    @classmethod
    def names(cls, r):
        out = []
        for pos, t, _ in cls.attrs(r):
            if t == 0x30:
                v = pos + struct.unpack_from("<H", r, pos + 20)[0]
                parent = struct.unpack_from("<Q", r, v)[0]
                n = r[v + 64]
                out.append((r[v + 66:v + 66 + 2 * n].decode("utf-16-le"), parent, v))
        return out

    @classmethod
    def data_runs(cls, r):
        out = []
        for pos, t, _ in cls.attrs(r):
            if t == 0x80 and r[pos + 8]:
                nl, no = r[pos + 9], struct.unpack_from("<H", r, pos + 10)[0]
                name = r[pos + no:pos + no + 2 * nl].decode("utf-16-le")
                ro = pos + struct.unpack_from("<H", r, pos + 32)[0]
                runs, lcn = [], 0
                while r[ro]:
                    h = r[ro]
                    ls, os_ = h & 15, h >> 4
                    length = int.from_bytes(r[ro + 1:ro + 1 + ls], "little")
                    if os_:
                        lcn += int.from_bytes(r[ro + 1 + ls:ro + 1 + ls + os_], "little", signed=True)
                        runs.append((lcn, length))
                    ro += 1 + ls + os_
                out.append((name, runs))
        return out

    def records(self):
        n = sum(c for _, c in self.mft_runs) * self.cluster // self.rsize
        for i in range(n):
            r = self.unfix(self.rd(self.record_at(i), self.rsize))
            if r is not None:
                yield i, r

    def find(self, name, parent=None):
        for i, r in self.records():
            for n, p, _ in self.names(r):
                if n == name and (parent is None or p & 0xFFFFFFFFFFFF == parent):
                    return i, r
        return None, None


def seq(r):
    return struct.unpack_from("<H", r, 16)[0]


def ref(n, r):
    return n | (seq(r) << 48)


def capture(img, state_path):
    v = Volume(img)
    hol, hr = v.find("Holiday", 5)
    _, root = v.find(".", 5)
    st = {"holiday": ref(hol, hr), "root": 5 | (seq(root) << 48), "images": []}
    for name in ["victim.bin", "small.txt"]:
        n, r = v.find(name, hol)
        used = struct.unpack_from("<I", r, 24)[0]
        st["images"].append({"name": name, "number": n, "ref": ref(n, r), "image": r[:used].hex()})
    rep, rr = v.find("zzz_report.zip")
    st["report"] = rep
    json.dump(st, open(state_path, "w"))


def usn_record(file, parent, reason, directory, name):
    units = name.encode("utf-16-le")
    length = (60 + len(units) + 7) // 8 * 8
    r = bytearray(length)
    ticks = int((time.time() + 11644473600) * 10_000_000)
    struct.pack_into("<IHHQQqqIIIIHH", r, 0, length, 2, 0, file, parent, 0, ticks, reason, 0, 0,
                     0x10 if directory else 0x20, len(units), 60)
    r[60:60 + len(units)] = units
    return r


def usn(mount, state_path):
    st = json.load(open(state_path))
    close = 0x80000000
    j = usn_record(st["holiday"], st["root"], close | 0x100, True, "Holiday")
    for img in st["images"]:
        j += usn_record(img["ref"], st["holiday"], close | 0x200, False, img["name"])
    j += usn_record(st["holiday"], st["root"], close | 0x200, True, "Holiday")
    j += usn_record(9999 | (1 << 48), st["root"], close | 0x200, False, "lost_note.txt")
    with open(f"{mount}/usnstage:$J", "wb") as f:
        f.seek(1 << 20)  # left sparse, as Windows drops old records
        f.write(j)


def seal(p):
    struct.pack_into("<HH", p, 4, 0x28, 9)
    p[0x28:0x2A] = b"\x5a\x00"
    for i in range(1, 9):
        e = i * 512
        p[0x28 + 2 * i:0x2A + 2 * i] = p[e - 2:e]
        p[e - 2:e] = b"\x5a\x00"


def journal(ops, pages):
    """Like logfile::tests::journal: a restart page, then records packed
    into pages from page 2, running across pages, fixups applied."""
    data_at = (0x28 + 2 * (PAGE // 512 + 1) + 7) // 8 * 8
    area = PAGE - data_at
    lsn = lambda off: (off >> 3) | (1 << (64 - SEQ_BITS))
    file_off = lambda s: (2 + s // area) * PAGE + data_at + s % area
    stream = bytearray()
    last_end = {}
    for d in ops:
        if area - len(stream) % area < 0x30:
            stream += bytes(-len(stream) % area)
        at = file_off(len(stream))
        h = bytearray(0x30)
        struct.pack_into("<Q", h, 0, lsn(at))
        struct.pack_into("<I", h, 0x18, len(d))
        struct.pack_into("<I", h, 0x20, 1)
        h[0x28] = int(len(stream) % area + 0x30 + len(d) > area)
        stream += h + d
        stream += bytes(-len(stream) % 8)
        last_end[(len(stream) - 1) // area + 2] = lsn(at)
    log = bytearray(pages * PAGE)
    for k in range(0, len(stream), area):
        p = 2 + k // area
        chunk = stream[k:k + area]
        log[p * PAGE + data_at:p * PAGE + data_at + len(chunk)] = chunk
    for p in range(2, 2 + (len(stream) + area - 1) // area):
        page = log[p * PAGE:(p + 1) * PAGE]
        page[:4] = b"RCRD"
        struct.pack_into("<Q", page, 0x20, last_end.get(p, 0))
        seal(page)
        log[p * PAGE:(p + 1) * PAGE] = page
    page = log[:PAGE]
    page[:4] = b"RSTR"
    struct.pack_into("<IIH", page, 0x10, PAGE, PAGE, 0x30)
    struct.pack_into("<I", page, 0x30 + 0x10, SEQ_BITS)
    struct.pack_into("<H", page, 0x30 + 0x26, data_at)
    seal(page)
    log[:PAGE] = page
    return log


def free_op(image, number, v):
    """A "free this record" step with the record as its undo half."""
    pos = number * v.rsize
    d = bytearray(0x20)
    struct.pack_into("<HHHHHH", d, 0, 3, 2, 0x20, 0, 0x20, len(image))
    struct.pack_into("<H", d, 0x14, (pos % v.cluster) // 512)
    struct.pack_into("<Q", d, 0x18, pos // v.cluster)
    return d + image


def plant(img, state_path):
    st = json.load(open(state_path))
    v = Volume(img, "r+b")
    # The deleted files' records must have been reused, or this would
    # check nothing new.
    for n in [i["number"] for i in st["images"]] + [st["holiday"] & 0xFFFFFFFFFFFF, st["report"]]:
        r = v.unfix(v.rd(v.record_at(n), v.rsize))
        names = [x[0] for x in v.names(r)] if r else []
        flags = struct.unpack_from("<H", r, 22)[0] if r else 0
        if not flags & 1:
            sys.exit(f"record {n} was not reused ({names})")
    ops = [free_op(bytes.fromhex(i["image"]), i["number"], v) for i in st["images"]]
    _, logrec = v.find("$LogFile", 5)
    log = journal(ops, 8)
    pos = 0
    for lcn, count in v.data_runs(logrec)[0][1]:
        n = min(count * v.cluster, len(log) - pos)
        v.wr(lcn * v.cluster, log[pos:pos + n])
        pos += n
        if pos >= len(log):
            break
    # usnstage becomes $Extend\$UsnJrnl (both names are 8 characters).
    _, ext = v.find("$Extend", 5)
    n, r = v.find("usnstage", 5)
    for _, _, at in v.names(r):
        struct.pack_into("<Q", r, at, ref(11, ext))
        r[at + 66:at + 66 + 16] = "$UsnJrnl".encode("utf-16-le")
    v.wr(v.record_at(n), v.fix(r))
    v.f.close()


if __name__ == "__main__":
    cmd, a, b = sys.argv[1:4]
    {"capture": capture, "usn": usn, "plant": plant}[cmd](a, b)

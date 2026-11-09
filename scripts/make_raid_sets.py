#!/usr/bin/env python3
"""Builds RAID member images for the RAID tests (engine/tests/raid.rs).

Makes FAT32, NTFS and ext4 volumes holding photos, documents, many small
files in nested folders and some deleted files, then lays each out over
member images as Linux md would: RAID 0 (2 to 4 members, several chunk
sizes), RAID 1, RAID 5 (all four parity layouts, some with a member
missing), RAID 10 (near 2) and JBOD. Some sets carry md superblocks
(1.2 and 0.90, written here to mdadm's format; `mdadm --examine` reads
them when it is installed), others carry none, as a hardware RAID card or
enclosure would leave them. Member files are named in a shuffled order so
the order has to be worked out.

The striping here is written independently of Reperio's, from the md
sources (raid0.c, raid5.c raid5_compute_sector, raid10.c).

Requires root (loop mounts), mkfs.vfat, mkntfs and ntfs-3g, mkfs.ext4,
python3 with Pillow.

    scripts/make_raid_sets.py OUTDIR
    REPERIO_TEST_RAID=OUTDIR cargo test -p reperio-engine --test raid -- --nocapture
"""
import hashlib
import io
import json
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import uuid

from PIL import Image

MiB = 1 << 20
VOLUME = 40 * MiB


def run(*cmd, **kw):
    subprocess.run(cmd, check=True, **kw)


def jpeg(seed, w, h):
    rnd = random.Random(seed)
    img = Image.effect_noise((w, h), 20 + seed % 60).convert("RGB")
    tint = Image.new("RGB", (w, h), tuple(rnd.randrange(256) for _ in range(3)))
    out = io.BytesIO()
    Image.blend(img, tint, 0.4).save(out, "JPEG", quality=90)
    return out.getvalue()


def png(seed, w, h):
    out = io.BytesIO()
    Image.effect_noise((w, h), 10 + seed % 40).convert("RGB").save(out, "PNG")
    return out.getvalue()


def pdf(seed):
    out = io.BytesIO()
    Image.effect_noise((600, 800), 30 + seed).convert("RGB").save(out, "PDF")
    return out.getvalue()


DELETED = [
    "DCIM/100CANON/IMG_0005.JPG",
    "DCIM/100CANON/IMG_0012.JPG",
    "DCIM/100CANON/IMG_0020.JPG",
    "Trash/old_1.png",
    "Trash/old_2.png",
]


def stage(root):
    """Writes the files to `root`; returns the manifest entries."""
    rnd = random.Random(42)
    files = {}

    def put(rel, data):
        path = os.path.join(root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)
        files[rel] = data

    for i in range(1, 25):
        w, h = rnd.choice([(640, 480), (1024, 768), (1600, 1200)])
        put(f"DCIM/100CANON/IMG_{i:04d}.JPG", jpeg(i, w, h))
    for i in range(6):
        put(f"Pictures/scan_{i}.png", png(i, 400 + 40 * i, 300))
    for i in range(3):
        put(f"Documents/report_{i}.pdf", pdf(i))
    put("Documents/archive.bin", rnd.randbytes(3 * MiB))
    # Many small files in nested folders: a big MFT, many folder blocks.
    for a in range(12):
        for b in range(4):
            for c in range(10):
                text = f"note {a}/{b}/{c}\n" * (5 + rnd.randrange(80))
                put(f"Notes/n{a:02d}/sub{b}/note_{c}.txt", text.encode())
    for i in range(4):
        put(f"Trash/old_{i}.png", png(100 + i, 300, 200))

    manifest = []
    for rel, data in sorted(files.items()):
        d, _, name = rel.rpartition("/")
        manifest.append({
            "dir": d,
            "name": name,
            "deleted": rel in DELETED,
            "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        })
    return manifest


def mounted(image, fs, work):
    """Context: the volume image mounted at a temporary folder."""
    class M:
        def __enter__(self):
            self.mnt = tempfile.mkdtemp(dir=work)
            self.loop = subprocess.check_output(
                ["losetup", "-f", "--show", image], text=True).strip()
            if fs == "ntfs":
                run("ntfs-3g", self.loop, self.mnt)
            else:
                run("mount", "-t", fs, self.loop, self.mnt)
            return self.mnt

        def __exit__(self, *a):
            os.sync()
            for _ in range(10):
                if subprocess.run(["umount", self.mnt]).returncode == 0:
                    break
                time.sleep(0.5)
            run("losetup", "-d", self.loop)
            os.rmdir(self.mnt)
    return M()


def with_mbr(volume, part_type):
    """A drive image: an MBR with one partition at 1 MiB holding `volume`."""
    sectors = len(volume) // 512
    mbr = bytearray(512)
    entry = struct.pack("<B3sB3sII", 0, b"\x00\x02\x00", part_type, b"\xff\xff\xff",
                        MiB // 512, sectors)
    mbr[446:462] = entry
    mbr[510:512] = b"\x55\xaa"
    return bytes(mbr) + bytes(MiB - 512) + volume


def make_volumes(work):
    staged = os.path.join(work, "files")
    manifest = stage(staged)
    tops = [os.path.join(staged, t) for t in sorted(os.listdir(staged))]
    vols = {}
    for kind in ["fat32", "ntfs", "ext4"]:
        img = os.path.join(work, f"{kind}.vol")
        with open(img, "wb") as f:
            f.truncate(VOLUME)
        if kind == "fat32":
            run("mkfs.vfat", "-F", "32", "-s", "1", "-n", "RAIDFAT", img,
                stdout=subprocess.DEVNULL)
            # No vfat in every kernel: fill it with mtools.
            env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
            run("mcopy", "-s", "-i", img, *tops, "::/", env=env)
            for rel in DELETED:
                run("mdel", "-i", img, "::/" + rel, env=env)
            fs = None
        elif kind == "ntfs":
            run("mkntfs", "-F", "-Q", "-q", "-L", "RAIDNTFS", "-c", "4096", img)
            fs = "ntfs"
        else:
            run("mkfs.ext4", "-q", "-F", "-L", "raidext", "-b", "1024", img)
            fs = "ext4"
        if fs:
            with mounted(img, fs, work) as mnt:
                for t in tops:
                    shutil.copytree(t, os.path.join(mnt, os.path.basename(t)))
                os.sync()
                for rel in DELETED:
                    os.remove(os.path.join(mnt, rel))
        with open(img, "rb") as f:
            data = f.read()
        # FAT and NTFS as on a PC drive, ext4 straight on the array.
        if kind == "fat32":
            data = with_mbr(data, 0x0C)
        elif kind == "ntfs":
            data = with_mbr(data, 0x07)
        os.remove(img)
        vols[kind] = (data, manifest)
    return vols


# --- md layouts --------------------------------------------------------------

def chunks_of(data, chunk):
    n = -(-len(data) // chunk)
    return [data[i * chunk:(i + 1) * chunk].ljust(chunk, b"\0") for i in range(n)]


def raid5_place(algo, stripe, dd, n):
    """(parity disk, data disk) as raid5_compute_sector computes them."""
    data_disks = n - 1
    if algo == 0:      # ALGORITHM_LEFT_ASYMMETRIC
        pd = data_disks - stripe % n
        if dd >= pd:
            dd += 1
    elif algo == 1:    # ALGORITHM_RIGHT_ASYMMETRIC
        pd = stripe % n
        if dd >= pd:
            dd += 1
    elif algo == 2:    # ALGORITHM_LEFT_SYMMETRIC
        pd = data_disks - stripe % n
        dd = (pd + 1 + dd) % n
    elif algo == 3:    # ALGORITHM_RIGHT_SYMMETRIC
        pd = stripe % n
        dd = (pd + 1 + dd) % n
    else:
        raise ValueError(algo)
    return pd, dd


def xor(blocks):
    acc = 0
    for b in blocks:
        acc ^= int.from_bytes(b, "little")
    return acc.to_bytes(len(blocks[0]), "little")


def lay_out(data, level, n, chunk, algo=2, split=None):
    """Member data (without metadata) for `data` striped as md would."""
    if level == "jbod":
        cut = split or len(data) // n
        return [data[:cut], data[cut:]]
    if level == "raid1":
        return [data] * n
    cs = chunks_of(data, chunk)
    if level == "raid0":
        rows = -(-len(cs) // n)
        members = [bytearray(rows * chunk) for _ in range(n)]
        for c, b in enumerate(cs):
            d, row = c % n, c // n
            members[d][row * chunk:(row + 1) * chunk] = b
        return [bytes(m) for m in members]
    if level == "raid5":
        stripes = -(-len(cs) // (n - 1))
        cs += [bytes(chunk)] * (stripes * (n - 1) - len(cs))
        members = [bytearray(stripes * chunk) for _ in range(n)]
        for s in range(stripes):
            row = cs[s * (n - 1):(s + 1) * (n - 1)]
            for dd, b in enumerate(row):
                pd, disk = raid5_place(algo, s, dd, n)
                members[disk][s * chunk:(s + 1) * chunk] = b
            members[pd][s * chunk:(s + 1) * chunk] = xor(row)
        return [bytes(m) for m in members]
    if level == "raid10":
        near = 2
        rows = -(-len(cs) * near // n)
        members = [bytearray(rows * chunk) for _ in range(n)]
        for c, b in enumerate(cs):
            for k in range(near):
                slot = c * near + k
                d, row = slot % n, slot // n
                members[d][row * chunk:(row + 1) * chunk] = b
        return [bytes(m) for m in members]
    raise ValueError(level)


MD_LEVEL = {"jbod": -1, "raid0": 0, "raid1": 1, "raid5": 5, "raid10": 10}


def sb_v1(level, layout, chunk, n, role, data_offset, data_size, set_uuid, name, roles):
    """An mdadm 1.2 superblock (struct mdp_superblock_1, super1.c)."""
    sb = bytearray(4096)
    struct.pack_into("<IIII", sb, 0, 0xa92b4efc, 1, 0, 0)
    sb[16:32] = set_uuid
    sb[32:64] = name.encode().ljust(32, b"\0")
    struct.pack_into("<QiIQIII", sb, 64, int(time.time()), MD_LEVEL[level], layout,
                     data_size // 512, chunk // 512, n, 0)
    struct.pack_into("<QQQ", sb, 128, data_offset // 512, data_size // 512, 8)
    struct.pack_into("<Q", sb, 152, 0xffffffffffffffff)   # recovery_offset: in sync
    struct.pack_into("<II", sb, 160, role, 0)
    sb[168:184] = uuid.uuid4().bytes
    struct.pack_into("<QQQ", sb, 192, int(time.time()), 25, 0xffffffffffffffff)
    struct.pack_into("<I", sb, 220, n)
    for i, r in enumerate(roles):
        struct.pack_into("<H", sb, 256 + 2 * i, r)
    size = 256 + 2 * n
    total = 0
    for i in range(0, size - size % 4, 4):
        total += struct.unpack_from("<I", sb, i)[0]
    if size % 4 == 2:
        total += struct.unpack_from("<H", sb, size - 2)[0]
    struct.pack_into("<I", sb, 216, ((total & 0xffffffff) + (total >> 32)) & 0xffffffff)
    return bytes(sb)


def sb_v090(level, layout, chunk, n, role, data_size, set_uuid, present):
    """An mdadm 0.90 superblock (mdp_super_t, little endian, super0.c)."""
    w = [0] * 1024
    u = struct.unpack("<IIII", set_uuid)
    w[0], w[1], w[2], w[3], w[4] = 0xa92b4efc, 0, 90, 0, 0
    w[5] = u[0]
    w[6] = int(time.time())
    w[7] = MD_LEVEL[level] & 0xffffffff
    w[8] = data_size // 1024
    w[9] = w[10] = n
    w[13], w[14], w[15] = u[1], u[2], u[3]
    w[32] = int(time.time())
    w[33] = 1                        # clean
    w[34] = w[35] = len(present)
    w[36] = n - len(present)
    w[39] = 25                       # events
    w[64] = layout
    w[65] = chunk
    for i in range(n):
        base = 128 + 32 * i
        if i in present:
            w[base:base + 5] = [i, 8, 16 * i, i, 6]   # active, in sync
        else:
            w[base:base + 5] = [i, 0, 0, i, 1 | 8]    # faulty, removed
    w[992:997] = [role, 8, 16 * role, role, 6]
    total = sum(w)
    w[38] = ((total & 0xffffffff) + (total >> 32)) & 0xffffffff
    return struct.pack("<1024I", *w)


def write_set(out, name, data, level, n, chunk=0, algo=2, md=None, missing=None,
              split=None, seed=0):
    members = lay_out(data, level, n, chunk, algo, split)
    rnd = random.Random(seed)
    files = list(range(n))
    rnd.shuffle(files)  # position i is stored in file files[i]
    set_uuid = uuid.UUID(int=rnd.getrandbits(128)).bytes
    layout = algo if level == "raid5" else (0x102 if level == "raid10" else 0)
    present = [i for i in range(n) if i != missing]
    order = []
    offsets = []
    os.makedirs(os.path.join(out, name), exist_ok=True)
    for pos, member in enumerate(members):
        if pos == missing:
            order.append(None)
            offsets.append(0)
            continue
        if md == "1.2":
            off = MiB if pos % 2 == 0 else 2 * MiB   # md picks per device
            sb = sb_v1(level, layout, chunk, n, pos, off, len(member), set_uuid,
                       f"reperio:{name}"[:32], [i if i in present else 0xfffe for i in range(n)])
            img = bytearray(off + len(member) + MiB)
            img[4096:8192] = sb
            img[off:off + len(member)] = member
        elif md == "0.90":
            off = 0
            size = len(member) + 192 * 1024
            sectors = size // 512
            at = ((sectors & ~127) - 128) * 512
            img = bytearray(size)
            img[:len(member)] = member
            img[at:at + 4096] = sb_v090(level, layout, chunk, n, pos, len(member),
                                        set_uuid, present)
        elif level == "jbod":
            # A spanned set without metadata is its drives end to end.
            off = 0
            img = bytearray(member)
        else:
            off = 0
            # A little slack at the end, as a card leaves for its metadata.
            img = bytearray(member) + bytes(64 * 1024)
        path = os.path.join(out, name, f"drive{files[pos]}.img")
        with open(path, "wb") as f:
            f.write(img)
        order.append(f"drive{files[pos]}.img")
        offsets.append(off)
        if md and shutil.which("mdadm"):
            r = subprocess.run(["mdadm", "--examine", path], capture_output=True, text=True)
            if r.returncode != 0 or "checksum" in r.stdout and "correct" not in r.stdout:
                sys.exit(f"mdadm does not accept {path}:\n{r.stdout}{r.stderr}")
    return {
        "level": level,
        "parity": ["left-asymmetric", "right-asymmetric", "left-symmetric",
                   "right-symmetric"][algo] if level == "raid5" else None,
        "chunk": chunk,
        "drives": sorted(o for o in order if o),
        "order": order,
        "offsets": offsets,
        "metadata": md,
        "missing": 0 if missing is None else 1,
    }


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    work = tempfile.mkdtemp(dir=out)
    try:
        vols = make_volumes(work)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    sets = {}
    seed = 1
    for kind, (data, manifest) in vols.items():
        with open(os.path.join(out, f"{kind}.img"), "wb") as f:
            f.write(data)
        with open(os.path.join(out, f"{kind}.json"), "w") as f:
            json.dump(manifest, f, indent=1)
        plans = [
            ("raid0-2x64k-md12", "raid0", 2, 64 << 10, 2, "1.2", None),
            ("raid0-3x16k", "raid0", 3, 16 << 10, 2, None, None),
            ("raid0-4x512k", "raid0", 4, 512 << 10, 2, None, None),
            ("raid0-2x4k-md090", "raid0", 2, 4 << 10, 2, "0.90", None),
            ("raid0-2x128k", "raid0", 2, 128 << 10, 2, None, None),
            ("raid1-2", "raid1", 2, 0, 2, None, None),
            ("raid1-2-md12", "raid1", 2, 0, 2, "1.2", None),
            ("raid5-4x64k-ls", "raid5", 4, 64 << 10, 2, None, None),
            ("raid5-4x64k-la-missing", "raid5", 4, 64 << 10, 0, None, 1),
            ("raid5-4x32k-rs", "raid5", 4, 32 << 10, 3, None, None),
            ("raid5-4x64k-ra-missing", "raid5", 4, 64 << 10, 1, None, 0),
            ("raid5-4x512k-ls-md12-missing", "raid5", 4, 512 << 10, 2, "1.2", 2),
            ("raid5-3x128k-ls-md090", "raid5", 3, 128 << 10, 2, "0.90", None),
            ("raid10-4x64k", "raid10", 4, 64 << 10, 2, None, None),
            ("raid10-4x512k-md12", "raid10", 4, 512 << 10, 2, "1.2", None),
            ("jbod-2", "jbod", 2, 0, 2, None, None),
            ("jbod-2-md12", "jbod", 2, 0, 2, "1.2", None),
        ]
        for name, level, n, chunk, algo, md, missing in plans:
            set_name = f"{kind}-{name}"
            split = (len(data) * 2 // 5) // 4096 * 4096 if level == "jbod" else None
            if level == "jbod" and md:
                split = len(data) // 2
            seed += 1
            info = write_set(out, set_name, data, level, n, chunk, algo, md, missing, split, seed)
            info["volume"] = f"{kind}.img"
            info["manifest"] = f"{kind}.json"
            sets[set_name] = info
            print(set_name, flush=True)
    with open(os.path.join(out, "sets.json"), "w") as f:
        json.dump(sets, f, indent=1)


if __name__ == "__main__":
    main()

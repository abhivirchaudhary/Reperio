#!/usr/bin/env python3
"""Builds realistic memory-card images for end-to-end recovery tests.

Each card has an MBR with one partition (FAT32, exFAT, NTFS, ext2/3/4) or
GPT (HFS+, APFS), camera folders,
photos with EXIF dates, videos, a deleted folder, long file names, and a
deleted photo that was written fragmented around another file. Some files
are deleted, as by a camera or computer, and a manifest records what a
perfect recovery returns.

Requires: python3 with Pillow and piexif, ffmpeg, mtools, dosfstools, and
for exFAT: exfatprogs, exfat-fuse and losetup (run as root); for NTFS:
ntfs-3g; for ext2/3/4: e2fsprogs and a kernel that mounts them over a loop
device (run as root).

    scripts/make_test_cards.py OUTDIR
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
import uuid
import zlib

import piexif
from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from apfs_card import ApfsCard  # noqa: E402
from hfsplus_card import HfsCard  # noqa: E402

PART_OFFSET = 1 << 20


def photo(path, date, w, h, seed, progressive=False):
    rnd = random.Random(seed)
    img = Image.effect_noise((w, h), 30 + seed % 50).convert("RGB")
    tint = Image.new("RGB", (w, h), (rnd.randrange(256), rnd.randrange(256), rnd.randrange(256)))
    img = Image.blend(img, tint, 0.5)
    thumb = io.BytesIO()
    img.resize((160, 120)).save(thumb, "JPEG")
    exif = piexif.dump({
        "0th": {piexif.ImageIFD.Make: b"Canon", piexif.ImageIFD.Model: b"PowerShot A520"},
        "Exif": {piexif.ExifIFD.DateTimeOriginal: date.encode()},
        "1st": {piexif.ImageIFD.JPEGInterchangeFormat: 0},
        "thumbnail": thumb.getvalue(),
    })
    img.save(path, "JPEG", quality=88, exif=exif, progressive=progressive)


def video(path, fmt, date, seconds=3, size="640x480"):
    subprocess.run([
        "ffmpeg", "-loglevel", "error", "-y",
        "-f", "lavfi", "-i", f"testsrc2=duration={seconds}:size={size}:rate=25",
        "-f", "lavfi", "-i", f"sine=duration={seconds}",
        "-c:v", "libx264", "-c:a", "aac", "-shortest",
        "-metadata", f"creation_time={date}", "-f", fmt, path,
    ], check=True)


def make_media(dir):
    """Creates the card's files. Returns {name: path}."""
    m = {}
    def p(name):
        m[name] = os.path.join(dir, name)
        return m[name]
    photo(p("IMG_0001.JPG"), "2004:07:15 14:03:59", 1200, 900, 1)
    photo(p("IMG_0002.JPG"), "2004:07:15 14:04:10", 1200, 900, 2)
    photo(p("IMG_0003.JPG"), "2004:07:16 09:30:00", 1200, 900, 3, progressive=True)
    photo(p("IMG_0004.JPG"), "2005:12:25 08:01:30", 1600, 1200, 4)   # kept on the card
    photo(p("IMG_0005.JPG"), "2005:12:25 08:02:00", 600, 400, 5)     # small, later deleted
    photo(p("IMG_0006.JPG"), "2005:12:25 08:05:00", 1600, 1200, 6)   # fills the gap and beyond
    photo(p("Birthday party - Mum & me.jpg"), "2006:03:02 18:20:00", 1000, 750, 7)
    photo(p("Été 2008 – plage.jpg"), "2008:08:14 16:40:00", 900, 600, 23)
    photo(p("IMG_0101.JPG"), "2007:08:01 11:00:00", 1000, 750, 8)
    photo(p("IMG_0102.JPG"), "2007:08:01 11:05:00", 1000, 750, 9)
    photo(p("IMG_0201.JPG"), "2010:05:01 10:00:00", 1200, 900, 21)  # leaves a gap
    photo(p("IMG_0202.JPG"), "2010:05:01 10:01:00", 1000, 750, 22)  # in the way
    video(p("DJI_0301.MP4"), "mp4", "2010-05-01T10:05:00Z", seconds=6, size="1280x720")
    video(p("MVI_0007.MOV"), "mov", "2006-07-04T15:30:00Z")
    video(p("VID_0008.MP4"), "mp4", "2009-03-14T09:00:00Z")
    with open(p("notes.txt"), "w") as f:
        f.write("Holiday packing list\n" * 200)
    return m


class Fat32Card:
    def __init__(self, image, size):
        self.image = image
        subprocess.run(["truncate", "-s", str(size), image], check=True)
        subprocess.run(["mkfs.fat", "-F", "32", "-s", "8", "-n", "CANON_DC", image],
                       check=True, stdout=subprocess.DEVNULL)
        os.environ["MTOOLS_SKIP_CHECK"] = "1"

    def mkdir(self, path):
        subprocess.run(["mmd", "-i", self.image, "::/" + path], check=True)

    def copy(self, src, path):
        subprocess.run(["mcopy", "-i", self.image, src, "::/" + path], check=True)

    def delete(self, path):
        subprocess.run(["mdel", "-i", self.image, "::/" + path], check=True)

    def delete_tree(self, path):
        subprocess.run(["mdeltree", "-i", self.image, "::/" + path], check=True)

    def close(self):
        pass


class NtfsCard:
    """An external drive formatted on Windows."""

    def __init__(self, image, size):
        self.image = image
        subprocess.run(["truncate", "-s", str(size), image], check=True)
        subprocess.run(["mkfs.ntfs", "-F", "-Q", "-L", "USB_DRIVE", image],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        self.mnt = tempfile.mkdtemp()
        subprocess.run(["ntfs-3g", self.loop, self.mnt], check=True)

    mkdir = lambda self, path: os.makedirs(os.path.join(self.mnt, path))

    def copy(self, src, path):
        shutil.copyfile(src, os.path.join(self.mnt, path))
        os.sync()

    def delete(self, path):
        os.remove(os.path.join(self.mnt, path))
        os.sync()

    def delete_tree(self, path):
        shutil.rmtree(os.path.join(self.mnt, path))
        os.sync()

    def close(self):
        subprocess.run(["umount", self.mnt], check=True)
        subprocess.run(["losetup", "-d", self.loop], check=True)
        os.rmdir(self.mnt)


class ExFatCard:
    def __init__(self, image, size):
        self.image = image
        subprocess.run(["truncate", "-s", str(size), image], check=True)
        subprocess.run(["mkfs.exfat", "-c", "4K", "-L", "SONY_CAM", image],
                       check=True, stdout=subprocess.DEVNULL)
        self.loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        self.mnt = tempfile.mkdtemp()
        subprocess.run(["mount.exfat-fuse", self.loop, self.mnt], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def mkdir(self, path):
        os.makedirs(os.path.join(self.mnt, path))

    def copy(self, src, path):
        shutil.copyfile(src, os.path.join(self.mnt, path))
        os.sync()

    def delete(self, path):
        os.remove(os.path.join(self.mnt, path))
        os.sync()

    def delete_tree(self, path):
        shutil.rmtree(os.path.join(self.mnt, path))
        os.sync()

    def close(self):
        subprocess.run(["fusermount", "-u", self.mnt], check=True)
        subprocess.run(["losetup", "-d", self.loop], check=True)
        os.rmdir(self.mnt)


class Ext4Card:
    """A card formatted on Linux, written and deleted from by the Linux
    kernel's own ext2/3/4 driver over a loop device, so every change goes
    through the jbd2 journal exactly as on a Linux or Android machine."""

    def __init__(self, image, size, fs="ext4", opts=()):
        self.image = image
        subprocess.run(["truncate", "-s", str(size), image], check=True)
        subprocess.run(["mkfs." + fs, "-q", "-F", "-E", "nodiscard", "-L", "PI_CARD", *opts, image],
                       check=True, stdout=subprocess.DEVNULL)
        self.loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        self.mnt = tempfile.mkdtemp()
        subprocess.run(["mount", "-t", fs, self.loop, self.mnt], check=True)

    def mkdir(self, path):
        os.makedirs(os.path.join(self.mnt, path))
        os.sync()

    def copy(self, src, path):
        shutil.copyfile(src, os.path.join(self.mnt, path))
        os.sync()

    def delete(self, path):
        os.remove(os.path.join(self.mnt, path))
        os.sync()

    def delete_tree(self, path):
        shutil.rmtree(os.path.join(self.mnt, path))
        os.sync()

    def close(self):
        subprocess.run(["umount", self.mnt], check=True)
        subprocess.run(["losetup", "-d", self.loop], check=True)
        os.rmdir(self.mnt)


class Ext3Card(Ext4Card):
    """ext3 with 1 KiB blocks: block pointers instead of extents, and the
    first data block at 1 rather than 0."""

    def __init__(self, image, size):
        super().__init__(image, size, "ext3", ["-b", "1024"])


class Ext2Card(Ext4Card):
    """ext2: no journal, so nothing keeps an old copy of a deleted inode.

    Linux's own drivers zero a deleted ext2 inode's block pointers, which
    leaves only the name to recover. Files are written by the kernel here,
    but deleted with debugfs, which frees the inode and its blocks the way
    e2fsprogs, older drivers and other systems' ext2 drivers do: block
    pointers left in place, the deletion time set. That is the case the
    ext2-style recovery from the inode table is for.
    """

    def __init__(self, image, size):
        super().__init__(image, size, "ext2")

    def _debugfs(self, commands):
        subprocess.run(["umount", self.mnt], check=True)
        subprocess.run(["debugfs", "-w", "-f", "-", self.loop], input="\n".join(commands) + "\n",
                       text=True, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["mount", "-t", "ext2", self.loop, self.mnt], check=True)

    def delete(self, path):
        self._debugfs([f'rm "/{path}"'])
        assert not os.path.exists(os.path.join(self.mnt, path)), path

    def delete_tree(self, path):
        commands = []
        for root, dirs, files in os.walk(os.path.join(self.mnt, path), topdown=False):
            rel = "/" + os.path.relpath(root, self.mnt)
            commands += [f'rm "{rel}/{f}"' for f in files] + [f'rmdir "{rel}"']
        self._debugfs(commands)
        assert not os.path.exists(os.path.join(self.mnt, path)), path


def kernel_has(fs):
    with open("/proc/filesystems") as f:
        return any(line.split()[-1] == fs for line in f if line.strip())


def with_mbr(part_image, disk_image, part_type):
    part_size = os.path.getsize(part_image)
    mbr = bytearray(512)
    mbr[446 + 4] = part_type
    struct.pack_into("<II", mbr, 446 + 8, PART_OFFSET // 512, part_size // 512)
    mbr[510:512] = b"\x55\xaa"
    with open(disk_image, "wb") as out:
        out.write(mbr + bytes(PART_OFFSET - 512))
        with open(part_image, "rb") as f:
            shutil.copyfileobj(f, out)
    os.remove(part_image)


# GPT partition type GUIDs.
EFI_GUID = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
HFS_GUID = "48465300-0000-11AA-AA11-00306543ECAC"
APFS_GUID = "7C3457EF-0000-11AA-AA11-00306543ECAC"
EFI_SIZE = 4 << 20


def with_gpt(part_image, disk_image, part_guid):
    """Lays the card out as Disk Utility does: GPT, a small EFI partition,
    then the volume."""
    part_size = os.path.getsize(part_image)
    first = (PART_OFFSET + EFI_SIZE) // 512
    last = first + part_size // 512 - 1
    total = last + 1 + 34
    entries = bytearray(128 * 128)
    for i, (guid, lo, hi, name) in enumerate([(EFI_GUID, PART_OFFSET // 512, first - 1, "EFI System Partition"),
                                              (part_guid, first, last, "Untitled")]):
        struct.pack_into("<16s16sQQQ", entries, i * 128, uuid.UUID(guid).bytes_le, uuid.uuid4().bytes_le, lo, hi, 0)
        entries[i * 128 + 56:i * 128 + 56 + 2 * len(name)] = name.encode("utf-16-le")
    hdr = bytearray(struct.pack("<8sIIIIQQQQ16sQIII", b"EFI PART", 0x10000, 92, 0, 0, 1, total - 1, 34,
                                total - 34, uuid.uuid4().bytes_le, 2, 128, 128, zlib.crc32(entries)))
    struct.pack_into("<I", hdr, 16, zlib.crc32(hdr))
    mbr = bytearray(512)
    mbr[446 + 4] = 0xEE
    struct.pack_into("<II", mbr, 446 + 8, 1, min(total - 1, 0xFFFFFFFF))
    mbr[510:512] = b"\x55\xaa"
    with open(disk_image, "wb") as out:
        out.write(mbr + bytes(hdr).ljust(512, b"\0") + bytes(entries))
        out.write(bytes(first * 512 - out.tell()))
        with open(part_image, "rb") as f:
            shutil.copyfileobj(f, out)
        out.write(bytes(34 * 512))
    os.remove(part_image)


def build(card_cls, part_type, disk_image, media, size):
    part = disk_image + ".part"
    card = card_cls(part, size)
    try:
        card.mkdir("DCIM")
        card.mkdir("DCIM/100CANON")
        cam = "DCIM/100CANON/"
        for n in ["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG", "IMG_0004.JPG", "IMG_0005.JPG",
                  "MVI_0007.MOV", "Birthday party - Mum & me.jpg", "Été 2008 – plage.jpg", "VID_0008.MP4"]:
            card.copy(media[n], cam + n)
        card.copy(media["notes.txt"], "notes.txt")
        # Free IMG_0005's clusters, then write a bigger photo: it fills the
        # gap and continues after the next file, so it is stored in pieces.
        card.delete(cam + "IMG_0005.JPG")
        card.copy(media["IMG_0006.JPG"], cam + "IMG_0006.JPG")
        # The same for a video: it fills a deleted photo's gap and continues
        # after the next photo, so it is stored in two pieces.
        card.copy(media["IMG_0201.JPG"], cam + "IMG_0201.JPG")
        card.copy(media["IMG_0202.JPG"], cam + "IMG_0202.JPG")
        card.delete(cam + "IMG_0201.JPG")
        card.copy(media["DJI_0301.MP4"], cam + "DJI_0301.MP4")
        card.mkdir("DCIM/101TRIP")
        card.copy(media["IMG_0101.JPG"], "DCIM/101TRIP/IMG_0101.JPG")
        card.copy(media["IMG_0102.JPG"], "DCIM/101TRIP/IMG_0102.JPG")

        # The accident.
        deleted = ["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG", "IMG_0006.JPG",
                   "MVI_0007.MOV", "Birthday party - Mum & me.jpg", "Été 2008 – plage.jpg", "VID_0008.MP4",
                   "IMG_0202.JPG", "DJI_0301.MP4"]
        for n in deleted:
            card.delete(cam + n)
        card.delete_tree("DCIM/101TRIP")
    finally:
        card.close()
    if isinstance(part_type, str):
        with_gpt(part, disk_image, part_type)
    else:
        with_mbr(part, disk_image, part_type)

    sha = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()
    expect = [{"name": n, "dir": "DCIM/100CANON", "sha256": sha(media[n]), "deleted": True} for n in deleted]
    expect += [{"name": n, "dir": "DCIM/101TRIP", "sha256": sha(media[n]), "deleted": True}
               for n in ["IMG_0101.JPG", "IMG_0102.JPG"]]
    expect += [{"name": "IMG_0004.JPG", "dir": "DCIM/100CANON", "sha256": sha(media["IMG_0004.JPG"]), "deleted": False},
               {"name": "notes.txt", "dir": "", "sha256": sha(media["notes.txt"]), "deleted": False}]
    return expect


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "test-cards"
    os.makedirs(out, exist_ok=True)
    media_dir = os.path.join(out, "media")
    os.makedirs(media_dir, exist_ok=True)
    media = make_media(media_dir)
    manifest = {
        "fat32.img": build(Fat32Card, 0x0C, os.path.join(out, "fat32.img"), media, 48 << 20),
    }
    if os.geteuid() == 0 and shutil.which("mount.exfat-fuse"):
        manifest["exfat.img"] = build(ExFatCard, 0x07, os.path.join(out, "exfat.img"), media, 48 << 20)
    if os.geteuid() == 0 and shutil.which("ntfs-3g"):
        manifest["ntfs.img"] = build(NtfsCard, 0x07, os.path.join(out, "ntfs.img"), media, 48 << 20)
    if os.geteuid() == 0:
        for name, cls in [("ext4", Ext4Card), ("ext3", Ext3Card), ("ext2", Ext2Card)]:
            if kernel_has(name):
                manifest[name + ".img"] = build(cls, 0x83, os.path.join(out, name + ".img"), media, 48 << 20)
    manifest["hfsplus.img"] = build(HfsCard, HFS_GUID, os.path.join(out, "hfsplus.img"), media, 48 << 20)
    manifest["apfs.img"] = build(ApfsCard, APFS_GUID, os.path.join(out, "apfs.img"), media, 48 << 20)
    with open(os.path.join(out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=1)
    print("built", ", ".join(manifest))


if __name__ == "__main__":
    main()

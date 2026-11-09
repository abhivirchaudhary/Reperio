#!/usr/bin/env python3
"""Builds disk images whose partitions are lost, for testing that Reperio
finds and reads them (see engine/src/fs/lost.rs).

The volumes are made exactly as make_test_cards.py makes its cards (real
mkfs, files written and deleted by the real drivers), then laid out on
disks whose partition tables have been wiped, edited or replaced:

  wiped_mbr.img      MBR zeroed; FAT32 at sector 63 (old DOS layout), ext4
                     and exFAT on cylinder boundaries, NTFS a track past one
  wiped_gpt.img      GPT header and entries zeroed behind the protective MBR
  deleted_entry.img  MBR still lists the first partition; the second's
                     entry was deleted
  repartitioned.img  a new table over the old layout: an unformatted
                     partition covering the old volumes' starts, and a
                     freshly formatted one after them
  damaged_boot.img   no table, and the first sector of an NTFS, a FAT32
                     and an exFAT volume zeroed: only their copies remain
  ext4_backup.img    no table, and two ext4 volumes (1 KiB and 4 KiB
                     blocks) with their superblock zeroed: found by backups
  odd_offsets.img    no table, volumes at sectors no tool would choose;
                     needs the every-sector search (--find-partitions)

A manifest (manifest.json) gives, for each image, whether the
every-sector search is needed and, for each volume, its offset and the
files a perfect recovery returns, in make_test_cards.py's format.

Requires what make_test_cards.py does, and root.

    scripts/make_lost_cards.py OUTDIR
"""
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_test_cards as cards  # noqa: E402

MIB = 1 << 20
SECTOR = 512
TRACK = 63 * SECTOR
CYLINDER = 255 * TRACK
VOL = 48 * MIB


class Ext4Big(cards.Ext4Card):
    """ext4 with 4 KiB blocks, big enough for a second block group (and
    so a superblock backup, 128 MiB in)."""

    def __init__(self, image, size):
        super().__init__(image, size, "ext4", ["-b", "4096"])


class Ext4Small(cards.Ext4Card):
    """ext4 with 1 KiB blocks: 8 MiB block groups, so a 48 MiB volume has
    superblock backups (with 4 KiB blocks it would be one group, and have
    none)."""

    def __init__(self, image, size):
        super().__init__(image, size, "ext4", ["-b", "1024"])


def volume(cls, media, tmp, size=VOL):
    """Makes a volume as make_test_cards.py does. Returns its bytes and the
    files expected back from it."""
    disk = os.path.join(tmp, "card.img")
    expect = cards.build(cls, 0x0C, disk, media, size)
    with open(disk, "rb") as f:
        f.seek(cards.PART_OFFSET)
        data = f.read()
    os.remove(disk)
    return data, expect


def mbr(entries):
    """An MBR with (type, offset, length) entries."""
    m = bytearray(512)
    for i, (typ, off, ln) in enumerate(entries):
        m[446 + i * 16 + 4] = typ
        struct.pack_into("<II", m, 446 + i * 16 + 8, off // SECTOR, ln // SECTOR)
    m[510:512] = b"\x55\xaa"
    return bytes(m)


def up(x, m):
    return -(-x // m) * m


def write_disk(path, size, pieces):
    """Writes a sparse disk image of `size` bytes with (offset, bytes)."""
    with open(path, "wb") as f:
        f.truncate(size)
        for off, data in pieces:
            f.seek(off)
            f.write(data)


def damage(data, start, end):
    b = bytearray(data)
    b[start:end] = bytes(end - start)
    return bytes(b)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "lost-cards"
    os.makedirs(out, exist_ok=True)
    tmp = tempfile.mkdtemp()
    media_dir = os.path.join(out, "media")
    os.makedirs(media_dir, exist_ok=True)
    media = cards.make_media(media_dir)

    fat32, fat32_files = volume(cards.Fat32Card, media, tmp)
    exfat, exfat_files = volume(cards.ExFatCard, media, tmp)
    ntfs, ntfs_files = volume(cards.NtfsCard, media, tmp)
    ext4, ext4_files = volume(cards.Ext4Card, media, tmp)
    ext4_small, ext4_small_files = volume(Ext4Small, media, tmp)
    ext4_big, ext4_big_files = volume(Ext4Big, media, tmp, 160 * MIB)
    manifest = {}

    def disk(name, size, pieces, volumes, find_partitions=False):
        write_disk(os.path.join(out, name), size, pieces)
        manifest[name] = {
            "find_partitions": find_partitions,
            "volumes": [{"offset": o, "fs": fs, "files": f} for o, fs, f in volumes],
        }

    # (a) The MBR zeroed (all 512 bytes, as a wipe tool does).
    at = [TRACK, 7 * CYLINDER, 14 * CYLINDER, 21 * CYLINDER + TRACK]
    disk("wiped_mbr.img", at[3] + VOL + MIB,
         [(at[0], fat32), (at[1], ext4), (at[2], exfat), (at[3], ntfs)],
         [(at[0], "FAT32", fat32_files), (at[1], "ext4", ext4_files),
          (at[2], "exFAT", exfat_files), (at[3], "NTFS", ntfs_files)])

    # (a) A GPT disk whose GPT was zeroed; the protective MBR remains.
    at = [MIB, 64 * MIB]
    size = at[1] + VOL + MIB
    disk("wiped_gpt.img", size,
         [(0, mbr([(0xEE, SECTOR, size - SECTOR)])), (at[0], exfat), (at[1], ntfs)],
         [(at[0], "exFAT", exfat_files), (at[1], "NTFS", ntfs_files)])

    # (b) The second partition's entry deleted; its volume is still there.
    at = [MIB, 64 * MIB]
    disk("deleted_entry.img", at[1] + VOL + MIB,
         [(0, mbr([(0x0C, at[0], VOL)])), (at[0], fat32), (at[1], ext4)],
         [(at[0], "FAT32", fat32_files), (at[1], "ext4", ext4_files)])

    # (c) Re-partitioned: a new, never formatted partition from 24 MiB
    # covers the old volumes' starts and much of them, and a new FAT32
    # volume follows. The old FAT32 and NTFS volumes are still there.
    at = [MIB, 50 * MIB]
    new_at, new_len = 100 * MIB, 16 * MIB
    fresh = os.path.join(tmp, "fresh.img")
    subprocess.run(["truncate", "-s", str(new_len), fresh], check=True)
    subprocess.run(["mkfs.fat", "-F", "32", "-n", "NEW", fresh], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    fresh_data = open(fresh, "rb").read()
    disk("repartitioned.img", new_at + new_len + MIB,
         [(0, mbr([(0x83, 24 * MIB, 60 * MIB), (0x0C, new_at, new_len)])),
          (at[0], fat32), (at[1], ntfs), (new_at, fresh_data)],
         [(at[0], "FAT32", fat32_files), (at[1], "NTFS", ntfs_files)])

    # (d) No table, and each volume's first sector zeroed: NTFS keeps a
    # copy in its last sector, FAT32 6 sectors in, exFAT 12 sectors in.
    at = [MIB, 50 * MIB, 100 * MIB]
    disk("damaged_boot.img", at[2] + VOL + MIB,
         [(at[0], damage(ntfs, 0, SECTOR)), (at[1], damage(fat32, 0, SECTOR)),
          (at[2], damage(exfat, 0, SECTOR))],
         [(at[0], "NTFS", ntfs_files), (at[1], "FAT32", fat32_files),
          (at[2], "exFAT", exfat_files)])

    # (e) No table, and ext4 superblocks zeroed: found by their backups.
    at = [2 * MIB, 51 * MIB]
    disk("ext4_backup.img", at[1] + 160 * MIB + MIB,
         [(at[0], damage(ext4_small, 1024, 2048)), (at[1], damage(ext4_big, 1024, 2048))],
         [(at[0], "ext4", ext4_small_files), (at[1], "ext4", ext4_big_files)])

    # Odd offsets, one with a damaged first sector: only the every-sector
    # search finds these.
    at = [12_345 * SECTOR, 60 * MIB + 77 * SECTOR, 110 * MIB + 333 * SECTOR,
          160 * MIB + 1001 * SECTOR]
    disk("odd_offsets.img", at[3] + VOL + MIB,
         [(at[0], fat32), (at[1], exfat), (at[2], damage(ntfs, 0, SECTOR)), (at[3], ext4)],
         [(at[0], "FAT32", fat32_files), (at[1], "exFAT", exfat_files),
          (at[2], "NTFS", ntfs_files), (at[3], "ext4", ext4_files)],
         find_partitions=True)

    with open(os.path.join(out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=1)
    shutil.rmtree(tmp)
    print("built", ", ".join(manifest))


if __name__ == "__main__":
    main()

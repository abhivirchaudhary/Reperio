#!/usr/bin/env python3
"""End-to-end recovery check against cards from make_test_cards.py.

For every card: recover with the reperio CLI and require every deleted file
back, byte for byte, at its original path. Then quick-format a copy of each
card and require every photo and video back by deep scan alone.

    scripts/e2e_check.py CARDS_DIR [path/to/reperio]
"""
import glob
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile

cards = sys.argv[1]
reperio = sys.argv[2] if len(sys.argv) > 2 else "target/release/reperio"
manifest = json.load(open(os.path.join(cards, "manifest.json")))
MEDIA = (".jpg", ".jpeg", ".mov", ".mp4", ".png")
# IMG_0006.JPG on the exFAT card is stored in two pieces around files that
# were deleted too; after a format only fragment repair can rebuild it.
# ext2 and ext3 map files with block pointers, and keep them in blocks
# among the file's own data: after the first 12 blocks of every file comes
# a block of pointers, and another every 256 or 1024 blocks after that. A
# format erases the inodes that say which blocks those are, so carving
# reads them as part of the photo and every photo and video over 12 blocks
# comes out broken. Carving that recognises and skips them is not done
# yet, so formatted ext2/ext3 cards are not checked. (ext4 maps files with
# extents and keeps them contiguous, and is checked.)
NO_CARVE_AFTER_FORMAT = ("ext2", "ext3")


def sha(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def recover(image, out, *extra):
    shutil.rmtree(out, ignore_errors=True)
    subprocess.run([reperio, "scan", image, "-o", out, *extra], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return {sha(p): os.path.relpath(p, out) for p in glob.glob(f"{out}/**/*", recursive=True)
            if os.path.isfile(p) and not p.endswith("report.csv")}


def quick_format(image, copy, fs):
    """Rebuilds the filesystem structures of the card's partition in place."""
    shutil.copy(image, copy)
    if fs in ("hfsplus", "apfs"):
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        quick_erase = __import__(fs + "_card").quick_erase
        offset = 5 << 20  # after the EFI partition
        quick_erase(copy, offset, os.path.getsize(copy) - offset - 34 * 512)
        return
    part = copy + ".part"
    offset = 1 << 20
    with open(copy, "rb") as f:
        f.seek(offset)
        open(part, "wb").write(f.read())
    if fs == "ntfs":
        subprocess.run(["mkfs.ntfs", "-F", "-Q", part], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    elif fs in ("ext4", "ext3", "ext2"):
        # nodiscard: discarding would zero a file-backed partition, which no
        # real quick format does.
        opts = ["-b", "1024"] if fs == "ext3" else []
        subprocess.run(["mkfs." + fs, "-q", "-F", "-E", "nodiscard", *opts, part], check=True,
                       stdout=subprocess.DEVNULL)
    elif fs == "fat32":
        subprocess.run(["mkfs.fat", "-F", "32", "-s", "8", part], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        subprocess.run(["mkfs.exfat", "-c", "4K", part], check=True, stdout=subprocess.DEVNULL)
    with open(copy, "r+b") as f:
        f.seek(offset)
        f.write(open(part, "rb").read())
    os.remove(part)


failures = []
tmp = tempfile.mkdtemp()
for image, expected in manifest.items():
    path = os.path.join(cards, image)
    got = recover(path, os.path.join(tmp, "out"))
    for e in expected:
        if not e["deleted"]:
            continue
        want = os.path.join(e["dir"], e["name"])
        if got.get(e["sha256"]) != want:
            failures.append(f"{image}: {want} -> {got.get(e['sha256'], 'MISSING')}")
    print(f"{image}: {sum(1 for e in expected if e['deleted'])} deleted files checked")

    if image.split(".")[0] in NO_CARVE_AFTER_FORMAT:
        print(f"{image} after quick format: not checked (indirect blocks, see above)")
        continue
    fmt = os.path.join(tmp, "formatted.img")
    quick_format(path, fmt, image.split(".")[0])
    got = recover(fmt, os.path.join(tmp, "out"))
    media = [e for e in expected if e["name"].lower().endswith(MEDIA)]
    for e in media:
        if e["sha256"] not in got:
            failures.append(f"{image} after format: {e['name']} MISSING")
    print(f"{image} after quick format: {len(media)} photos and videos checked")

shutil.rmtree(tmp)
if failures:
    print("FAILED:\n  " + "\n  ".join(failures))
    sys.exit(1)
print("all recovered byte for byte")

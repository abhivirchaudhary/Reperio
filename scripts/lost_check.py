#!/usr/bin/env python3
"""End-to-end check against disks from make_lost_cards.py: recover each
with the reperio CLI (a full scan, deep scan included) and require every
deleted file of every lost volume back, byte for byte, under its original
folder and name. Several volumes hold the same files, so a copy may carry
a " (2)" after its name.

    scripts/lost_check.py LOST_CARDS_DIR [path/to/reperio]
"""
import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

cards = sys.argv[1]
reperio = sys.argv[2] if len(sys.argv) > 2 else "target/release/reperio"
manifest = json.load(open(os.path.join(cards, "manifest.json")))


def sha(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def canonical(rel):
    """The path without the " (n)" recovery adds to a name already used."""
    return re.sub(r" \(\d+\)(\.[^./]*)?$", r"\1", rel)


failures = []
tmp = tempfile.mkdtemp()
for image, disk in manifest.items():
    out = os.path.join(tmp, "out")
    shutil.rmtree(out, ignore_errors=True)
    extra = ["--find-partitions"] if disk["find_partitions"] else []
    subprocess.run([reperio, "scan", os.path.join(cards, image), "-o", out, *extra], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    got = {}
    for p in glob.glob(f"{out}/**/*", recursive=True):
        if os.path.isfile(p) and not p.endswith("report.csv"):
            key = (sha(p), canonical(os.path.relpath(p, out)))
            got[key] = got.get(key, 0) + 1
    want = {}
    for v in disk["volumes"]:
        for e in v["files"]:
            if e["deleted"]:
                key = (e["sha256"], os.path.join(e["dir"], e["name"]))
                want[key] = want.get(key, 0) + 1
    for (digest, path), n in want.items():
        if got.get((digest, path), 0) < n:
            failures.append(f"{image}: {path} x{n} -> {got.get((digest, path), 0)}")
    print(f"{image}: {sum(want.values())} deleted files checked")

shutil.rmtree(tmp)
if failures:
    print("FAILED:\n  " + "\n  ".join(failures))
    sys.exit(1)
print("all recovered byte for byte")

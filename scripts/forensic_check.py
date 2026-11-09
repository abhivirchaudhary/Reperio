#!/usr/bin/env python3
"""End-to-end check of `reperio image` as a forensic acquisition.

Serves a test disk through FUSE with an unreadable stretch (reads touching
it fail with EIO, as on a failing drive), images it with the forensic
options, split into segments, and checks with Python's own hashlib that:

  * the image is the disk with the unreadable stretch as zeros,
  * the reported MD5, SHA-1 and SHA-256 (whole image and each segment)
    are right,
  * the report lists exactly the unreadable sectors, the case details and
    that the source was opened read-only,
  * verification matches, and fails (naming the sector) when the source
    changes after the copy.

    python3 scripts/forensic_check.py target/release/reperio

Needs FUSE (/dev/fuse, fusermount) and fusepy (apt install python3-fusepy,
or pip install fusepy).
"""

import errno
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

try:
    import fuse  # pip's fusepy
except ImportError:
    import fusepy as fuse  # Debian and Ubuntu's python3-fusepy

SECTOR = 512
SIZE = 9 * 1024 * 1024 + 7 * SECTOR
# 24 unreadable sectors. 4K-aligned: the engine reads devices and files in
# 4096-byte units (see source::ALIGN), so a failure costs a whole 4K block.
BAD = (2_998_272, 2_998_272 + 24 * SECTOR)
SPLIT = 4 * 1024 * 1024


class BadDisk(fuse.Operations):
    """One file, disk.img, whose reads fail across BAD."""

    def __init__(self, data):
        self.data = data
        self.bad = BAD

    def getattr(self, path, fh=None):
        if path == "/":
            return {"st_mode": 0o40555, "st_nlink": 2}
        if path == "/disk.img":
            return {"st_mode": 0o100444, "st_nlink": 1, "st_size": len(self.data)}
        raise fuse.FuseOSError(errno.ENOENT)

    def readdir(self, path, fh):
        return [".", "..", "disk.img"]

    def open(self, path, flags):
        if flags & (os.O_WRONLY | os.O_RDWR):
            raise fuse.FuseOSError(errno.EROFS)
        return 0

    def read(self, path, size, offset, fh):
        end = min(offset + size, len(self.data))
        if offset < self.bad[1] and end > self.bad[0]:
            raise fuse.FuseOSError(errno.EIO)
        return bytes(self.data[offset:end])


def pattern(n):
    out = bytearray(n)
    x = 12345
    for i in range(0, n, 4):
        x = (x * 1103515245 + 12345) & 0xFFFFFFFF
        out[i : i + 4] = x.to_bytes(4, "little")
    return out


def hashes(data):
    return {
        "md5": hashlib.md5(data).hexdigest(),
        "sha1": hashlib.sha1(data).hexdigest(),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def check(cond, what):
    if not cond:
        print(f"FAIL: {what}")
        sys.exit(1)
    print(f"ok: {what}")


def main():
    reperio = os.path.abspath(sys.argv[1])
    work = tempfile.mkdtemp(prefix="reperio-forensic-")
    mnt = os.path.join(work, "mnt")
    os.mkdir(mnt)
    disk = BadDisk(pattern(SIZE))
    # direct_io: every read reaches the filesystem, as on a real device,
    # instead of being widened and cached by the page cache.
    t = threading.Thread(
        target=lambda: fuse.FUSE(disk, mnt, foreground=True, ro=True, direct_io=True),
        daemon=True,
    )
    t.start()
    src = os.path.join(mnt, "disk.img")
    for _ in range(100):
        if os.path.exists(src):
            break
        time.sleep(0.05)
    try:
        out = os.path.join(work, "evidence.img")
        args = [
            reperio, "image", src, out, "--split", "4M", "--verify",
            "--case", "2026-0042", "--examiner", "A. Examiner",
            "--evidence", "Item 3: test disk", "--notes", "simulated bad region",
        ]
        run = subprocess.run(args, capture_output=True, text=True)
        print(run.stderr)
        check(run.returncode == 0, "imaging succeeds")

        want = bytearray(disk.data)
        want[BAD[0] : BAD[1]] = bytes(BAD[1] - BAD[0])
        segs = sorted(f for f in os.listdir(work) if f.startswith("evidence.img.0"))
        check(segs == ["evidence.img.001", "evidence.img.002", "evidence.img.003"],
              f"three segments ({segs})")
        joined = b"".join(open(os.path.join(work, s), "rb").read() for s in segs)
        check(joined == want, "image is the disk with the bad stretch zeroed")

        report = json.load(open(out + ".report.json"))
        text = open(out + ".report.txt").read()
        check(report["hashes"] == hashes(want), "whole-image hashes match hashlib")
        for i, seg in enumerate(report["image"]):
            piece = want[i * SPLIT : (i + 1) * SPLIT]
            check(seg["hashes"] == hashes(piece) and seg["size"] == len(piece),
                  f"segment {i + 1} hashes and size match hashlib")
        for name, value in hashes(want).items():
            check(value in run.stdout, f"{name} printed")
        bad = [(a["first_sector"], a["last_sector"]) for a in report["unreadable"]]
        check(bad == [(BAD[0] // SECTOR, BAD[1] // SECTOR - 1)], f"unreadable sectors listed ({bad})")
        check(f"sectors {BAD[0] // SECTOR}-{BAD[1] // SECTOR - 1}" in text, "text report lists them")
        check(report["case"] == {"case_number": "2026-0042", "examiner": "A. Examiner",
                                 "evidence": "Item 3: test disk", "notes": "simulated bad region"},
              "case details recorded")
        check(report["source"]["opened_read_only"] is True, "source recorded as read-only")
        check(report["totals"]["complete"] and report["totals"]["unreadable"] == BAD[1] - BAD[0],
              "totals recorded")
        check([p["pass"] for p in report["passes"]][:1] == ["copy"], "passes logged")
        v = report["verification"]
        check(v["matched"] and v["source_hashes"] == report["hashes"], "verification matches")
        check("MATCH" in text and report["started"].endswith("Z"), "text report and UTC times")

        # The source changes after the copy: verification must fail and say
        # where. Resuming a finished copy only hashes and verifies again.
        disk.data[5_000_000] ^= 0xFF
        run = subprocess.run(args + ["--resume", "--report", os.path.join(work, "second")],
                             capture_output=True, text=True)
        print(run.stderr)
        check(run.returncode == 2, "changed source fails verification (exit 2)")
        second = json.load(open(os.path.join(work, "second.json")))
        diff = second["verification"]["differing"]
        check([d["first_sector"] for d in diff] == [5_000_000 // SECTOR], f"differing sector named ({diff})")
        check(second["hashes"] == report["hashes"], "image hashes unchanged")
        print("All forensic checks passed.")
    finally:
        subprocess.run(["fusermount", "-u", mnt])
        if os.environ.get("KEEP"):
            print("kept", work)
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()

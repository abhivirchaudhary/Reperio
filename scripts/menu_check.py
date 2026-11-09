#!/usr/bin/env python3
"""Drives `reperio menu` through loop devices, the way a person would.

Builds a FAT32 "internal disk" holding files (one of them deleted) and an
ext4 "USB drive", attaches both as loop devices, starts the menu and types
its answers:

  1. recover everything from the internal disk to the USB drive,
  2. make a forensic image of the internal disk onto the USB drive,

then checks that the files arrived intact, that the source image was not
modified, that the source disk is never offered as a destination, that the
drive was unmounted afterwards, and that the image hashes match hashlib.

    sudo python3 scripts/menu_check.py target/release/reperio

Needs root, losetup, mkfs.vfat, mkfs.ext4 and mtools.
"""

import hashlib
import json
import os
import re
import select
import subprocess
import sys
import tempfile
import time


def sh(*args, **kw):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kw).stdout


def check(cond, what):
    if not cond:
        print(f"FAIL: {what}")
        sys.exit(1)
    print(f"ok: {what}")


def mount_ro(dev, where):
    """Mounts a drive read-only to inspect it, with ntfs-3g where the
    kernel has no NTFS driver."""
    if subprocess.run(["mount", "-o", "ro", dev, where], capture_output=True).returncode:
        sh("ntfs-3g", dev, where, "-o", "ro")


class Menu:
    def __init__(self, exe, env):
        self.p = subprocess.Popen([exe, "menu"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, env=env, bufsize=0)
        self.seen = ""

    def until(self, pattern, timeout=300):
        """Reads output until `pattern` appears after what was already
        consumed; returns the text up to and including the match."""
        end = time.time() + timeout
        while True:
            m = re.search(pattern, self.seen)
            if m:
                text, self.seen = self.seen[: m.end()], self.seen[m.end():]
                return text
            if time.time() > end:
                print(self.seen)
                check(False, f"timed out waiting for {pattern!r}")
            r, _, _ = select.select([self.p.stdout], [], [], 1)
            if r:
                chunk = os.read(self.p.stdout.fileno(), 65536)
                if not chunk:
                    print(self.seen)
                    check(False, f"menu ended while waiting for {pattern!r}")
                self.seen += chunk.decode(errors="replace")

    def send(self, text):
        self.p.stdin.write((text + "\n").encode())

    def pick(self, title, wanted):
        """Answers a numbered list: the option whose line matches `wanted`.
        Returns the whole screen text."""
        screen = self.until(re.escape(title) + r"[\s\S]*?Type a number and press Enter: ")
        for line in screen.splitlines():
            m = re.match(r"\s*(\d+)\) (.*)", line)
            if m and re.search(wanted, m.group(2)):
                self.send(m.group(1))
                return screen
        print(screen)
        check(False, f"no option matching {wanted!r} under {title!r}")


def main():
    exe = os.path.abspath(sys.argv[1])
    work = tempfile.mkdtemp(prefix="reperio-menu-")
    src_img, dst_img = f"{work}/internal.img", f"{work}/usb.img"
    loops = []
    try:
        # The "internal disk": FAT32 with a few files, one deleted.
        sh("truncate", "-s", "48M", src_img)
        sh("mkfs.vfat", "-F", "32", "-S", "512", "-s", "1", src_img)
        files = {"notes.txt": os.urandom(30_000), "holiday.dat": os.urandom(200_000),
                 "deleted.dat": os.urandom(70_000)}
        for name, data in files.items():
            with open(f"{work}/{name}", "wb") as f:
                f.write(data)
            sh("mcopy", "-i", src_img, f"{work}/{name}", "::")
        sh("mdel", "-i", src_img, "::deleted.dat")
        before = hashlib.sha256(open(src_img, "rb").read()).hexdigest()

        # The "USB drive": ext4.
        sh("truncate", "-s", "400M", dst_img)
        sh("mkfs.ext4", "-q", "-F", dst_img)
        src_dev = sh("losetup", "-f", "--show", src_img).strip()
        loops.append(src_dev)
        dst_dev = sh("losetup", "-f", "--show", dst_img).strip()
        loops.append(dst_dev)
        src_name, dst_name = os.path.basename(src_dev), os.path.basename(dst_dev)
        mnt = f"{work}/mnt"
        env = dict(os.environ, REPERIO_MENU_SHOW_LOOP="1", REPERIO_MENU_SCRIPTED="1",
                   REPERIO_MOUNT_ROOT=mnt)

        def recover_run(dev, fsname):
            name = os.path.basename(dev)
            m = Menu(exe, env)
            m.until(r"Reperio rescue")
            m.pick("Which drive do you want to recover files from?", rf"; {src_name}\)")
            m.pick("What would you like to do?", "Find and recover files")
            m.pick("How thorough", "Quick")
            m.until(r"\[Y/n\] "); m.send("y")
            m.until(r"\[y/N\] "); m.send("n")
            m.until(r"Found \d+ files")
            m.pick("What do you want to bring back?", "Everything")
            screen = m.pick("Where should the files be saved?", rf"{name} {fsname}")
            check(f"{src_name} FAT32" not in screen and f"; {src_name})" not in screen,
                  "the source disk is not offered as a destination")
            m.pick("How should the recovered files be arranged?", "Original folders")
            done = m.until(r"Done: \d+ files saved to \S+")
            check("3 files" in done or "Done: 3" in done, f"three files recovered ({done.strip().splitlines()[-1]})")
            m.until(r"(safe to unplug|Could not unmount)")
            m.until(r"What would you like to do\?")
            m.send("b"); m.until(r"Which drive do you want"); m.send("q")
            m.p.wait(timeout=30)
            check(m.p.returncode == 0, "menu exits cleanly")
            check(subprocess.run(["mountpoint", "-q", f"{mnt}/{name}"]).returncode != 0,
                  "the destination was unmounted")
            sh("mkdir", "-p", f"{work}/check")
            mount_ro(dev, f"{work}/check")
            try:
                got = {}
                for root, _, names in os.walk(f"{work}/check/Reperio recovered"):
                    for n in names:
                        if n != "report.csv":
                            got[n] = open(os.path.join(root, n), "rb").read()
                # FAT loses the first letter of a deleted file's name ("_eleted.dat").
                check(sorted(got.values()) == sorted(files.values()),
                      f"recovered files are identical, deleted one included ({sorted(got)})")
            finally:
                sh("umount", f"{work}/check")

        recover_run(dst_dev, "ext4")

        # The same again onto an NTFS drive, mounted with ntfs-3g.
        ntfs_img = f"{work}/usb-ntfs.img"
        sh("truncate", "-s", "300M", ntfs_img)
        sh("mkntfs", "-F", "-Q", "-q", ntfs_img)
        ntfs_dev = sh("losetup", "-f", "--show", ntfs_img).strip()
        loops.append(ntfs_dev)
        recover_run(ntfs_dev, "NTFS")

        # --- Forensic image ---
        m = Menu(exe, env)
        m.until(r"Reperio rescue")
        m.pick("Which drive do you want to recover files from?", rf"; {src_name}\)")
        m.pick("What would you like to do?", "complete copy")
        m.until(r"Case number \(optional\): "); m.send("2026-77")
        m.until(r"Your name \(optional\): "); m.send("Test Examiner")
        m.until(r"Notes \(optional\): "); m.send("b q notes stay free text")
        m.until(r"\[y/N\] "); m.send("y")
        m.pick("Where should the files be saved?", rf"{dst_name} ext4")
        m.until(r"The copy is in (\S+)", timeout=300)
        m.until(r"(safe to unplug|Could not unmount)")
        m.until(r"What would you like to do\?")
        m.send("b"); m.until(r"Which drive do you want"); m.send("q")
        m.p.wait(timeout=30)
        mount_ro(dst_dev, f"{work}/check")
        try:
            base = f"{work}/check/Reperio images/{src_name}.img"
            data = open(base, "rb").read()
            report = json.load(open(base + ".report.json"))
            check(data == open(src_img, "rb").read(), "the image equals the internal disk")
            check(report["hashes"]["sha256"] == hashlib.sha256(data).hexdigest(), "SHA-256 in the report is right")
            check(report["hashes"]["md5"] == hashlib.md5(data).hexdigest(), "MD5 in the report is right")
            check(report["case"]["case_number"] == "2026-77" and report["case"]["notes"] == "b q notes stay free text",
                  "case details and free-text notes kept")
            check(report["verification"]["matched"], "verification matched")
        finally:
            sh("umount", f"{work}/check")

        after = hashlib.sha256(open(src_img, "rb").read()).hexdigest()
        check(before == after, "the internal disk image was not modified")
        print("All menu checks passed.")
    finally:
        for dev in loops:
            subprocess.run(["losetup", "-d", dev])
        subprocess.run(["rm", "-rf", work])


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Compares Reperio with PhotoRec on the same card images.

Each tool recovers everything it can from the image into a temporary
folder. Then, against a manifest of the files that should come back:

  files     files the tool wrote (PhotoRec's extracted EXIF thumbnails,
            t*.jpg, are counted separately as "thumbs")
  exact     expected files recovered byte for byte (sha256), of all expected
  deleted   the same, counting only files that had been deleted
  named     exact matches that also kept their original file name
  pathed    ...and their original folder
  partial   files that start like an expected file but differ from it:
            truncated, merged with other data, or damaged
  other     anything else (files not in the manifest, embedded previews)
  time      wall-clock seconds

    scripts/compare_tools.py IMAGE [IMAGE...] [--manifest manifest.json]
                             [--formatted] [--reperio target/release/reperio]
                             [--keep DIR]

With test cards from make_test_cards.py, the manifest is found next to the
images, and --formatted also runs both tools on a quick-formatted copy of
each card, as e2e_check.py does. For a real card, make the manifest before
deleting anything with --make-manifest FOLDER (see docs/real-card-testing.md).

The manifest is either {"image.img": [entry, ...]} or a plain list of
entries: {"name": ..., "dir": ..., "sha256": ..., "deleted": true/false}.

Needs PhotoRec (`apt-get install testdisk`, `brew install testdisk`).
Everything is read-only on the image; formatting is done on a copy.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PREFIX = 512  # bytes compared to call a file a partial copy of another


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def head(path, n=PREFIX):
    with open(path, "rb") as f:
        return f.read(n)


def files_under(root, skip=("report.csv", "report.xml")):
    out = []
    for dirpath, _, names in os.walk(root):
        for n in names:
            if n not in skip:
                out.append(os.path.join(dirpath, n))
    return sorted(out)


def run_reperio(binary, image, out):
    """Recovers deleted and existing files, as PhotoRec does both."""
    cmd = [binary, "scan", image, "-o", out, "--existing"]
    began = time.monotonic()
    proc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    elapsed = time.monotonic() - began
    if proc.returncode != 0:
        print(f"  reperio failed: {proc.stderr.strip()[-300:]}", file=sys.stderr)
    return files_under(out), elapsed


# The whole image as one partition, all file types, corrupt files
# discarded (PhotoRec's interactive default). PhotoRec's ext2/ext3 mode
# needs the partition selected instead: "1,options,mode_ext2,wholespace,search".
PHOTOREC_OPTIONS = "partition_none,options,keep_corrupted_file_no,search"


def run_photorec(image, out, options=PHOTOREC_OPTIONS):
    """PhotoRec without its interactive interface."""
    work = tempfile.mkdtemp(prefix="photorec-")
    dest = os.path.join(work, "recup")
    cmd = ["photorec", "/log", "/d", dest, "/cmd", os.path.abspath(image), options]
    began = time.monotonic()
    proc = subprocess.run(cmd, cwd=work, stdout=subprocess.DEVNULL,
                          stderr=subprocess.PIPE, stdin=subprocess.DEVNULL, text=True)
    elapsed = time.monotonic() - began
    if proc.returncode != 0:
        print(f"  photorec failed: {proc.stderr.strip()[-300:]}", file=sys.stderr)
    # PhotoRec writes to recup.1, recup.2, ... (a new folder per 500 files).
    os.makedirs(out, exist_ok=True)
    for d in sorted(os.listdir(work)):
        if d.startswith("recup."):
            for p in files_under(os.path.join(work, d)):
                shutil.move(p, os.path.join(out, os.path.basename(p)))
    shutil.rmtree(work, ignore_errors=True)
    return files_under(out), elapsed


def score(files, out, expected, thumbs_prefix=None):
    """Counts how a tool's output measures up to the manifest."""
    by_sha = {}
    for p in files:
        by_sha.setdefault(sha256(p), []).append(p)
    exact = deleted = named = pathed = 0
    matched = set()
    for e in expected:
        hits = by_sha.get(e["sha256"], [])
        if not hits:
            continue
        exact += 1
        deleted += bool(e["deleted"])
        matched.update(hits)
        want = os.path.join(e["dir"], e["name"])
        if any(os.path.basename(p) == e["name"] for p in hits):
            named += 1
        if any(os.path.relpath(p, out) == want for p in hits):
            pathed += 1
    # Expected files' first bytes, to recognise partial or damaged copies.
    heads = {bytes.fromhex(e["head"]) for e in expected if e.get("head")}
    thumbs = partial = other = 0
    for p in files:
        if p in matched:
            continue
        if thumbs_prefix and os.path.basename(p).startswith(thumbs_prefix):
            thumbs += 1
        elif head(p) in heads:
            partial += 1
        else:
            other += 1
    return {"files": len(files) - thumbs, "thumbs": thumbs, "exact": exact,
            "deleted": deleted, "named": named, "pathed": pathed,
            "partial": partial, "other": other}


def quick_format(image, copy, fs):
    """Rebuilds the card's filesystem on a copy, as e2e_check.py does."""
    shutil.copy(image, copy)
    if fs in ("hfsplus", "apfs"):
        sys.path.insert(0, HERE)
        quick_erase = __import__(fs + "_card").quick_erase
        offset = 5 << 20  # after the EFI partition
        quick_erase(copy, offset, os.path.getsize(copy) - offset - 34 * 512)
        return True
    # nodiscard: discarding would zero a file-backed partition, which no
    # real quick format does.
    tools = {"ntfs": ["mkfs.ntfs", "-F", "-Q"], "fat32": ["mkfs.fat", "-F", "32", "-s", "8"],
             "exfat": ["mkfs.exfat", "-c", "4K"],
             "ext4": ["mkfs.ext4", "-q", "-F", "-E", "nodiscard"],
             "ext3": ["mkfs.ext3", "-q", "-F", "-E", "nodiscard", "-b", "1024"],
             "ext2": ["mkfs.ext2", "-q", "-F", "-E", "nodiscard"]}
    if fs not in tools or not shutil.which(tools[fs][0]):
        return False
    part = copy + ".part"
    offset = 1 << 20
    with open(copy, "rb") as f:
        f.seek(offset)
        with open(part, "wb") as p:
            shutil.copyfileobj(f, p)
    subprocess.run(tools[fs] + [part], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with open(copy, "r+b") as f, open(part, "rb") as p:
        f.seek(offset)
        shutil.copyfileobj(p, f)
    os.remove(part)
    return True


def load_manifest(path, image):
    data = json.load(open(path))
    if isinstance(data, dict):
        data = data.get(os.path.basename(image))
        if data is None:
            return None
    return data


def make_manifest(folder):
    """Lists every file under `folder` (a mounted card) with its hash."""
    out = []
    for p in files_under(folder, skip=()):
        rel = os.path.relpath(p, folder)
        name = os.path.basename(rel)
        if name.startswith("._") or name == ".DS_Store" or rel.startswith((".Spotlight", ".fseventsd", ".Trashes")):
            continue
        out.append({"name": name, "dir": os.path.dirname(rel), "sha256": sha256(p),
                    "size": os.path.getsize(p), "head": head(p).hex(), "deleted": False})
    return out


COLUMNS = ["files", "thumbs", "exact", "deleted", "named", "pathed", "partial", "other", "time"]


def print_table(rows):
    header = f"{'image':<26} {'tool':<9} " + " ".join(f"{c:>8}" for c in COLUMNS)
    print(header)
    print("-" * len(header))
    for image, tool, r, total, total_deleted in rows:
        cells = []
        for c in COLUMNS:
            v = r[c]
            if c == "time":
                cells.append(f"{v:>7.1f}s")
            elif c == "exact":
                cells.append(f"{v:>3}/{total:<4}")
            elif c == "deleted":
                cells.append(f"{v:>3}/{total_deleted:<4}")
            else:
                cells.append(f"{v:>8}")
        print(f"{image:<26} {tool:<9} " + " ".join(cells))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("images", nargs="*")
    ap.add_argument("--manifest")
    ap.add_argument("--formatted", action="store_true", help="also compare on quick-formatted copies")
    ap.add_argument("--reperio", default=os.path.join(HERE, "..", "target", "release", "reperio"))
    ap.add_argument("--keep", help="keep each tool's output under this folder")
    ap.add_argument("--photorec-options", default=PHOTOREC_OPTIONS,
                    help=f"PhotoRec /cmd options (default: {PHOTOREC_OPTIONS})")
    ap.add_argument("--json", help="write the results here too")
    ap.add_argument("--make-manifest", metavar="FOLDER",
                    help="print a manifest of the files under FOLDER (a mounted card) and exit")
    args = ap.parse_args()

    if args.make_manifest:
        json.dump(make_manifest(args.make_manifest), sys.stdout, indent=1)
        print()
        return
    if not args.images:
        ap.error("give at least one image")
    if not shutil.which("photorec"):
        sys.exit("photorec not found: apt-get install testdisk (or brew install testdisk)")
    if not os.path.exists(args.reperio):
        sys.exit(f"{args.reperio} not found: cargo build --release -p reperio, or pass --reperio")

    rows = []
    tmp = tempfile.mkdtemp(prefix="compare-")
    try:
        for image in args.images:
            manifest = args.manifest or os.path.join(os.path.dirname(image), "manifest.json")
            expected = load_manifest(manifest, image) if os.path.exists(manifest) else None
            if expected is None:
                sys.exit(f"{image}: no manifest entry (pass --manifest)")
            expected = [dict(e) for e in expected]
            variants = [(os.path.basename(image), image, expected)]
            if args.formatted:
                fmt = os.path.join(tmp, "formatted-" + os.path.basename(image))
                if quick_format(image, fmt, os.path.basename(image).split(".")[0]):
                    # After a format no file is still on the card: everything
                    # counts as deleted.
                    variants.append((os.path.basename(image) + " (format)", fmt,
                                     [dict(e, deleted=True) for e in expected]))
            # Test cards keep their original files in media/.
            media = os.path.join(os.path.dirname(image), "media")
            for e in expected:
                p = os.path.join(media, e["name"])
                if "head" not in e and os.path.exists(p):
                    e["head"] = head(p).hex()
            for label, path, exp in variants:
                total, total_deleted = len(exp), sum(1 for e in exp if e["deleted"])
                for tool in ("reperio", "photorec"):
                    out = os.path.join(args.keep or tmp, label.replace(" ", "_"), tool)
                    shutil.rmtree(out, ignore_errors=True)
                    os.makedirs(out)
                    if tool == "reperio":
                        files, elapsed = run_reperio(args.reperio, path, out)
                        r = score(files, out, exp)
                    else:
                        files, elapsed = run_photorec(path, out, args.photorec_options)
                        r = score(files, out, exp, thumbs_prefix="t")
                    r["time"] = elapsed
                    rows.append((label, tool, r, total, total_deleted))
                    print(f"  {label} {tool}: {r['exact']}/{total} exact in {elapsed:.1f}s", file=sys.stderr)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print()
    print_table(rows)
    if args.json:
        json.dump([{"image": i, "tool": t, **r, "expected": n, "expected_deleted": d}
                   for i, t, r, n, d in rows], open(args.json, "w"), indent=1)


if __name__ == "__main__":
    main()

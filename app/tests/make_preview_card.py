#!/usr/bin/env python3
"""Makes a FAT card image for checking the viewer and photo grouping.

On it, then deleted: a burst of three similar photos, a photo saved again
smaller and recompressed, an exact copy of another photo, unrelated
photos, a VP9 video in MP4 (WebKit on Linux plays VP9 with GStreamer's
own plugins; H.264 needs gst-libav), an MP3, a text file and a PDF.

    python3 app/tests/make_preview_card.py OUT.img

Needs Pillow, ffmpeg, mkfs.vfat and mtools.
"""
import io
import os
import random
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFilter

out = sys.argv[1]
work = tempfile.mkdtemp()


def scene(seed, w=1600, h=1200, shift=0):
    """A made-up landscape: sky, hills, a sun and some trees."""
    rnd = random.Random(seed)
    img = Image.new("RGB", (w + 200, h))
    d = ImageDraw.Draw(img)
    sky = [rnd.randint(60, 200) for _ in range(3)]
    for y in range(h):
        t = y / h
        d.line([(0, y), (w + 200, y)], fill=tuple(int(c * (1 - t) + 230 * t) for c in sky))
    sx, sy, r = rnd.randint(200, w - 200), rnd.randint(100, 400), rnd.randint(60, 140)
    d.ellipse([sx - r, sy - r, sx + r, sy + r], fill=(255, rnd.randint(180, 240), 120))
    for k in range(3):
        base = h * (0.55 + 0.12 * k)
        pts = [(x, base - 120 * abs(((x / (w / rnd.uniform(1, 4))) % 2) - 1) * rnd.uniform(0.5, 1.5)) for x in range(0, w + 201, 40)]
        col = tuple(rnd.randint(20, 120) for _ in range(3))
        d.polygon(pts + [(w + 200, h), (0, h)], fill=col)
    for _ in range(rnd.randint(4, 12)):
        x, y = rnd.randint(0, w + 200), rnd.randint(int(h * 0.6), h)
        s = rnd.randint(30, 90)
        d.polygon([(x, y - 3 * s), (x - s, y), (x + s, y)], fill=(20, rnd.randint(70, 130), 40))
    img = img.filter(ImageFilter.GaussianBlur(1.2))
    return img.crop((100 + shift, 0, 100 + shift + w, h))


def jpeg(img, quality=90):
    b = io.BytesIO()
    img.save(b, "JPEG", quality=quality)
    return b.getvalue()


files = {}
base = scene(1)
files["DCIM/100CANON/IMG_0101.JPG"] = jpeg(base, 92)
files["DCIM/100CANON/IMG_0102.JPG"] = jpeg(scene(1, shift=18), 92)  # burst
files["DCIM/100CANON/IMG_0103.JPG"] = jpeg(scene(1, shift=36), 92)  # burst
files["Shared/IMG_0101 small.jpg"] = jpeg(base.resize((800, 600), Image.LANCZOS), 60)
copy = jpeg(scene(5), 88)
files["DCIM/100CANON/IMG_0110.JPG"] = copy
files["Backup/IMG_0110.JPG"] = copy
for n, seed in enumerate([11, 12, 13, 14]):
    files[f"DCIM/100CANON/IMG_02{n:02}.JPG"] = jpeg(scene(seed), 90)

clip = os.path.join(work, "clip.mp4")
subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=6",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=6", "-c:v", "libvpx-vp9", "-b:v", "600k",
                "-c:a", "libopus", "-shortest", "-y", clip], check=True)
files["DCIM/100CANON/MVI_0120.MP4"] = open(clip, "rb").read()
# WebKitGTK builds without H.264/VP9 plugins still play VP8 in WebM.
webm = os.path.join(work, "clip.webm")
subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=6",
                "-c:v", "libvpx", "-b:v", "600k", "-an", "-y", webm], check=True)
files["DCIM/100CANON/MVI_0121.WEBM"] = open(webm, "rb").read()
song = os.path.join(work, "song.mp3")
subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=330:duration=4",
                "-c:a", "libmp3lame", "-y", song], check=True)
files["Music/Voice memo.mp3"] = open(song, "rb").read()
files["Documents/notes.txt"] = "Shopping list\n- bread\n- olive oil\n- tomatoes\n\nCall the plumber on Tuesday.\n".encode()
pdf_page = b"BT /F1 18 Tf 72 720 Td (Reperio test document) Tj 0 -28 Td (Holiday plans: Rome in May) Tj ET"
pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n" \
      b"3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >> endobj\n" \
      b"4 0 obj << /Length " + str(len(pdf_page)).encode() + b" >>\nstream\n" + pdf_page + \
      b"\nendstream endobj\n5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n"
files["Documents/Plans.pdf"] = pdf

subprocess.run(["truncate", "-s", "64M", out], check=True)
subprocess.run(["mkfs.vfat", "-F", "32", "-n", "TESTCARD", out], check=True, stdout=subprocess.DEVNULL)
env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
for d in sorted({os.path.dirname(p) for p in files} | {"DCIM"}):
    parts = d.split("/")
    for i in range(1, len(parts) + 1):
        subprocess.run(["mmd", "-i", out, "::/" + "/".join(parts[:i])], env=env, stderr=subprocess.DEVNULL)
for path, data in files.items():
    local = os.path.join(work, "f")
    open(local, "wb").write(data)
    subprocess.run(["mcopy", "-i", out, local, "::/" + path], check=True, env=env)
for path in files:
    subprocess.run(["mdel", "-i", out, "::/" + path], check=True, env=env)
print(f"{out}: {len(files)} files written and deleted")

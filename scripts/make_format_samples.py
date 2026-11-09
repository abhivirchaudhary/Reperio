#!/usr/bin/env python3
"""Writes real sample files of every deep-scan format into OUTDIR, made by
the tools that make such files in the wild (Pillow, ImageMagick, ffmpeg,
zipfile, LibreOffice, rar, 7z, sqlite3, fontTools), for the engine's
format tests:

    scripts/make_format_samples.py OUTDIR
    REPERIO_TEST_SAMPLES=OUTDIR cargo test -p reperio-engine --test samples
"""
import io
import os
import random
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import zipfile
import zlib

from PIL import Image, ImageDraw

out = sys.argv[1] if len(sys.argv) > 1 else "format-samples"
os.makedirs(out, exist_ok=True)
random.seed(11)


def picture(w=640, h=480):
    img = Image.new("RGB", (w, h), (30, 90, 160))
    d = ImageDraw.Draw(img)
    for _ in range(40):
        x, y, r = random.randrange(w), random.randrange(h), random.randrange(10, 90)
        d.ellipse((x - r, y - r, x + r, y + r), fill=tuple(random.randrange(256) for _ in range(3)))
    return img


def path(name):
    return os.path.join(out, name)


def ffmpeg(*args):
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", *args], check=True)


# Images
picture().save(path("drawing.gif"))
frames = [picture(320, 240) for _ in range(4)]
frames[0].save(path("animation.gif"), save_all=True, append_images=frames[1:], duration=200, loop=0)
picture().save(path("scan.bmp"))
subprocess.run(["convert", path("scan.bmp"), path("layers.psd")], check=True)
picture().save(path("letter.pdf"))
pages = [picture(612, 792) for _ in range(3)]
pages[0].save(path("album.pdf"), save_all=True, append_images=pages[1:])

# Documents and archives
def office(name, folder, content_type):
    with zipfile.ZipFile(path(name), "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", f'<?xml version="1.0"?><Types><Override ContentType="{content_type}"/></Types>')
        z.writestr("_rels/.rels", "<Relationships/>")
        z.writestr(f"{folder}/document.xml", "<doc>" + "Holiday plans. " * 400 + "</doc>")
        b = io.BytesIO()
        picture(400, 300).save(b, "PNG")
        z.writestr(f"{folder}/media/image1.png", b.getvalue())

office("report.docx", "word", "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml")
office("budget.xlsx", "xl", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml")
office("slides.pptx", "ppt", "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml")
with zipfile.ZipFile(path("photos.zip"), "w", zipfile.ZIP_STORED) as z:
    for i in range(3):
        b = io.BytesIO()
        picture(300, 200).save(b, "JPEG")
        z.writestr(f"IMG_{i:04}.JPG", b.getvalue())

# Audio and video
ffmpeg("-f", "lavfi", "-i", "sine=frequency=440:duration=6", "-c:a", "libmp3lame", "-b:a", "128k",
       "-metadata", "title=Birthday song", path("song.mp3"))
ffmpeg("-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25", "-f", "lavfi", "-i", "sine=duration=3",
       "-c:v", "libvpx", "-b:v", "300k", "-c:a", "libvorbis", "-shortest", path("clip.webm"))
ffmpeg("-f", "lavfi", "-i", "testsrc2=duration=3:size=320x240:rate=25", "-f", "lavfi", "-i", "sine=duration=3",
       "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest", path("movie.mkv"))

# Legacy Office documents, written by LibreOffice
work = tempfile.mkdtemp()
with open(os.path.join(work, "notes.txt"), "w") as f:
    f.write("Holiday plans. " * 2000)
with open(os.path.join(work, "budget.csv"), "w") as f:
    for _ in range(800):
        f.write(",".join(str(random.randrange(1000)) for _ in range(8)) + "\n")
with open(os.path.join(work, "deck.fodp"), "w") as f:
    f.write('<?xml version="1.0" encoding="UTF-8"?>'
            '<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" '
            'xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" '
            'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" '
            'xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" office:version="1.2" '
            'office:mimetype="application/vnd.oasis.opendocument.presentation"><office:body><office:presentation>'
            + "".join(f'<draw:page draw:name="P{i}"><draw:frame svg:x="2cm" svg:y="2cm" svg:width="20cm" '
                      f'svg:height="5cm"><draw:text-box><text:p>Slide {i}: holiday plans</text:p>'
                      '</draw:text-box></draw:frame></draw:page>' for i in range(5))
            + "</office:presentation></office:body></office:document>")
for src, fmt, name in [("notes.txt", "doc", "notes.doc"), ("budget.csv", "xls", "budget.xls"),
                       ("deck.fodp", "ppt", "deck.ppt")]:
    subprocess.run(["soffice", "--headless", "--convert-to", fmt, "--outdir", work, os.path.join(work, src)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.copy(os.path.join(work, os.path.splitext(src)[0] + "." + fmt), path(name))


def compound_file(streams):
    """A version 3 OLE2 compound file holding `streams` (name -> bytes) in
    its root, laid out as the specification describes. Nothing here writes
    Outlook messages, so the .msg sample is built by hand."""
    ss, end, free = 512, 0xFFFFFFFE, 0xFFFFFFFF
    sectors, fat = [], []

    def alloc(data):
        first = len(sectors)
        n = max(1, -(-len(data) // ss))
        for i in range(n):
            sectors.append(data[i * ss:(i + 1) * ss].ljust(ss, b"\0"))
            fat.append(end if i + 1 == n else first + i + 1)
        return first

    mini, mini_fat, entries = b"", [], []
    for name, data in streams.items():
        if len(data) >= 4096:
            first = alloc(data)
        else:
            first = len(mini) // 64
            n = -(-len(data) // 64)
            mini_fat += [end if i + 1 == n else first + i + 1 for i in range(n)]
            mini += data.ljust(n * 64, b"\0")
        entries.append((name, 2, first, len(data)))
    mini_start = alloc(mini) if mini else end
    mini_fat_data = struct.pack(f"<{len(mini_fat)}I", *mini_fat)
    mini_fat_start = alloc(mini_fat_data) if mini_fat else end

    def entry(name, typ, child, right, first, size):
        n = (name + "\0").encode("utf-16-le")
        return (n.ljust(64, b"\0") + struct.pack("<HBBIII", len(n), typ, 1, free, right, child)
                + bytes(16 + 4 + 16) + struct.pack("<IQ", first, size))

    d = entry("Root Entry", 5, 1 if entries else free, free, mini_start, len(mini))
    for i, (name, typ, first, size) in enumerate(entries):
        d += entry(name, typ, free, i + 2 if i + 1 < len(entries) else free, first, size)
    d = d.ljust(-(-len(d) // ss) * ss, b"\0")
    dir_start = alloc(d)
    # FAT sectors go last; each FAT sector is itself marked in the FAT.
    n_fat = 1
    while len(fat) + n_fat > n_fat * ss // 4:
        n_fat += 1
    fat_locs = list(range(len(sectors), len(sectors) + n_fat))
    fat += [0xFFFFFFFD] * n_fat
    fat += [free] * (n_fat * ss // 4 - len(fat))
    table = struct.pack(f"<{len(fat)}I", *fat)
    sectors += [table[i * ss:(i + 1) * ss] for i in range(n_fat)]
    assert n_fat <= 109
    difat = fat_locs + [free] * (109 - n_fat)
    header = (bytes.fromhex("D0CF11E0A1B11AE1") + bytes(16)
              + struct.pack("<HHHHH6sIIIIIIIII", 0x3E, 3, 0xFFFE, 9, 6, bytes(6), 0, n_fat, dir_start, 0, 4096,
                            mini_fat_start, -(-len(mini_fat_data) // ss), end, 0)
              + struct.pack("<109I", *difat))
    return header + b"".join(sectors)


def utf16(s):
    return s.encode("utf-16-le")


with open(path("message.msg"), "wb") as f:
    f.write(compound_file({
        "__properties_version1.0": bytes(32) + bytes(range(256)) * 2,
        "__substg1.0_0037001F": utf16("Holiday photos"),
        "__substg1.0_0C1A001F": utf16("Grandma"),
        "__substg1.0_1000001F": utf16("Here are the photos from the beach. " * 200),
        "__substg1.0_001A001F": utf16("IPM.Note"),
    }))

# Archives
for i in range(3):
    picture(300, 200).save(os.path.join(work, f"IMG_{i:04}.JPG"))
files = [f"IMG_{i:04}.JPG" for i in range(3)] + ["notes.txt"]
subprocess.run(["rar", "a", "-ma5", "-idq", "-ep", path("photos5.rar")] + [os.path.join(work, n) for n in files],
               check=True)
subprocess.run(["7z", "a", "-bd", "-bso0", path("photos.7z")] + [os.path.join(work, n) for n in files], check=True)


def rar4(members):
    """A RAR 4 archive storing `members` (name -> bytes) uncompressed. RAR
    5 and later can no longer write this format, so it is built by hand."""
    def block(typ, flags, body, add=b""):
        size = 7 + len(body)
        h = struct.pack("<BHH", typ, flags, size) + body
        return struct.pack("<H", zlib.crc32(h) & 0xFFFF) + h + add

    out = b"Rar!\x1a\x07\x00" + block(0x73, 0, bytes(6))
    for name, data in members.items():
        n = name.encode()
        body = struct.pack("<IIBIIBBHI", len(data), len(data), 2, zlib.crc32(data), 0x5A2E6B4C, 20, 0x30,
                           len(n), 0x20) + n
        out += block(0x74, 0x8000, body, data)
    return out + block(0x7B, 0x4000, b"")


with open(path("photos4.rar"), "wb") as f:
    f.write(rar4({n: open(os.path.join(work, n), "rb").read() for n in files}))

# A database, as phones and apps keep them
db = sqlite3.connect(path("contacts.sqlite"))
db.execute("create table people (name text, phone text, note text)")
db.executemany("insert into people values (?, ?, ?)",
               [(f"Person {i}", f"555-{i:04}", "met at the beach " * random.randrange(1, 20)) for i in range(2000)])
db.commit()
db.close()


def pst(unicode):
    """An empty Outlook data file: a valid header, then zeros up to the end
    of file it states. Nothing here writes PST files, so it is built from
    the [MS-PST] header layout by hand."""
    eof = 0x4400 + 0x3E000

    def weak_crc(b):
        return ~zlib.crc32(b, 0xFFFFFFFF) & 0xFFFFFFFF

    h = bytearray(564 if unicode else 512)
    h[0:4] = b"!BDN"
    h[8:10] = b"SM"
    struct.pack_into("<HHBB", h, 10, 23 if unicode else 14, 19, 1, 1)
    if unicode:
        struct.pack_into("<QQ", h, 184, eof, 0x4400)
        h[512], h[513] = 0x80, 1
        struct.pack_into("<I", h, 4, weak_crc(h[8:8 + 471]))
        struct.pack_into("<I", h, 524, weak_crc(h[8:8 + 516]))
    else:
        struct.pack_into("<II", h, 168, eof, 0x4400)
        h[460], h[461] = 0x80, 1
        struct.pack_into("<I", h, 4, weak_crc(h[8:8 + 471]))
    return bytes(h).ljust(eof, b"\0")


with open(path("mailbox.pst"), "wb") as f:
    f.write(pst(True))
with open(path("old-mailbox.pst"), "wb") as f:
    f.write(pst(False))

# Fonts
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.t2CharStringPen import T2CharStringPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTCollection, TTFont


def font(name, cff):
    fb = FontBuilder(1000, isTTF=not cff)
    glyphs = [".notdef"] + [f"g{c}" for c in range(65, 91)]
    fb.setupGlyphOrder(glyphs)
    fb.setupCharacterMap({c: f"g{c}" for c in range(65, 91)})
    shapes = {}
    for g in glyphs:
        pen = T2CharStringPen(600, None) if cff else TTGlyphPen(None)
        w = random.randrange(100, 500)
        pen.moveTo((50, 0)); pen.lineTo((50 + w, 0)); pen.lineTo((50 + w, 700)); pen.lineTo((50, 700))
        pen.closePath()
        shapes[g] = pen.getCharString() if cff else pen.glyph()
    if cff:
        fb.setupCFF(name, {"FullName": name}, shapes, {})
    else:
        fb.setupGlyf(shapes)
    fb.setupHorizontalMetrics({g: (600, 50) for g in glyphs})
    fb.setupHorizontalHeader(ascent=800, descent=-200)
    fb.setupNameTable({"familyName": name, "styleName": "Regular"})
    fb.setupOS2()
    fb.setupPost()
    return fb.font


font("Holiday", False).save(path("holiday.ttf"))
font("Beach", True).save(path("beach.otf"))
collection = TTCollection()
collection.fonts = [TTFont(path("holiday.ttf")), font("Mountain", False)]
collection.save(path("family.ttc"))

# Professional and Windows media, and lossless audio
ffmpeg("-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25", "-f", "lavfi", "-i", "sine=duration=3",
       "-c:v", "mpeg2video", "-b:v", "1M", "-c:a", "pcm_s16le", "-ar", "48000", "-shortest", "-f", "mxf",
       path("camera.mxf"))
ffmpeg("-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25", "-f", "lavfi", "-i", "sine=duration=3",
       "-c:v", "wmv2", "-b:v", "300k", "-c:a", "wmav2", "-shortest", path("party.wmv"))
ffmpeg("-f", "lavfi", "-i", "sine=frequency=330:duration=5", "-c:a", "wmav2", path("voice.wma"))
ffmpeg("-f", "lavfi", "-i", "sine=frequency=550:duration=5", "-f", "lavfi", "-i", "anoisesrc=d=5:a=0.1",
       "-filter_complex", "[0][1]amerge=inputs=2", "-ac", "2", "-c:a", "flac", path("concert.flac"))
shutil.rmtree(work)

print("wrote", len(os.listdir(out)), "samples to", out)

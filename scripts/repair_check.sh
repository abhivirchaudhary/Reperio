#!/bin/bash
# Checks photo, document and video repair end to end with real tools.
#
#   scripts/repair_check.sh [path/to/reperio]
#
# 1. Videos with no reference. H.264 and H.265 clips with the parameter
#    sets repeated in the stream and AAC sound, cut where the index would
#    be and cut at 60%. `reperio repair` must rebuild them from the stream
#    alone: every video frame identical and in order, nearly all sound
#    packets identical, and a clean decode. A DJI-looking file must be
#    recognised as one.
# 2. Photos: a JPEG without its tables (repaired with another photo of the
#    same size) must decode to exactly the original pixels; one cut short
#    keeps its top; one whose picture is gone gives up its embedded preview.
# 3. Documents: DOCX, XLSX and PPTX with the index lost or the main part cut,
#    a PDF with a damaged table, and a Word 97 file with a zeroed header.
# 4. A FAT card image holding those files deleted, scanned and recovered,
#    including a DJI video that can't be repaired being replaced by its
#    .LRF copy.
#
# Needs ffmpeg (libx264, libx265), python3 with Pillow, LibreOffice,
# pdftotext, and mtools with mkfs.vfat.
set -uo pipefail
export RUST_BACKTRACE=0
reperio=$(realpath "${1:-target/release/reperio}")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
if [ -n "${KEEP:-}" ]; then trap - EXIT; echo "files kept in $work"; fi
failed=0
pass() { echo "ok   $*"; }
fail() { echo "FAIL $*"; failed=1; }
quiet() { "$@" > /dev/null 2>&1; }

# ------------------------------------------------------------- videos

vframes() { ffmpeg -v error -i "$1" -map 0:v -f framemd5 - | grep -v '^#' | awk -F, '{print $6}'; }
apackets() { ffmpeg -v error -i "$1" -map 0:a -c copy -f framemd5 - | grep -v '^#' | awk -F, '{print $6}'; }

# makes orig.mp4 (10 s, 640x360, repeated parameter sets, AAC) and cut_end/cut_mid
make_clip() {
  ffmpeg -loglevel error -y -f lavfi -i testsrc=duration=10:size=640x360:rate=30 \
    -f lavfi -i sine=duration=10:sample_rate=48000 -ac 2 \
    -c:v "$@" -pix_fmt yuv420p -g 30 -c:a aac -shortest orig.mp4
  python3 - <<'PY'
import struct
d = open("orig.mp4", "rb").read()
pos, boxes = 0, {}
while pos < len(d):
    size, typ = struct.unpack(">I4s", d[pos:pos + 8])
    boxes[typ] = (pos, size)
    pos += size
moov, mdat = boxes[b"moov"], boxes[b"mdat"]
assert moov[0] > mdat[0]
open("cut_end.mp4", "wb").write(d[:moov[0]])
open("cut_mid.mp4", "wb").write(d[:mdat[0] + int(mdat[1] * 0.6)])
PY
}

check_video() {
  local name=$1 cut=$2 want_min=$3
  rm -f fixed.mp4
  if ! "$reperio" repair "$cut.mp4" -o fixed.mp4 > repair.log 2>&1; then
    fail "$name $cut: repair failed: $(cat repair.log)"; return
  fi
  local errors; errors=$(ffmpeg -v error -i fixed.mp4 -f null - 2>&1 | wc -l)
  vframes fixed.mp4 > got_v.txt
  local n; n=$(wc -l < got_v.txt)
  local sound; sound=$(apackets fixed.mp4 | wc -l)
  python3 - "$n" "$want_min" <<'PY' > /dev/null
import sys
sys.exit(0 if int(sys.argv[1]) >= int(sys.argv[2]) else 1)
PY
  local enough=$?
  if [ "$enough" -ne 0 ] || ! diff -q <(head -n "$n" want_v.txt) got_v.txt > /dev/null; then
    fail "$name $cut: $n video frames, order or content differs"; return
  fi
  apackets fixed.mp4 > got_a.txt
  local share
  share=$(python3 - <<'PY'
want = open("want_a.txt").read().split()
got = open("got_a.txt").read().split()
w = set(want)
print(sum(1 for g in got if g in w) / max(1, len(got)), len(got))
PY
)
  local ratio=${share% *} count=${share#* }
  if python3 -c "import sys; sys.exit(0 if $ratio >= 0.97 and $count >= $((want_min * 3 / 2)) else 1)"; then
    pass "$name $cut: $n frames identical, $count sound packets ($ratio identical), $errors decode messages"
  else
    fail "$name $cut: sound packets $count, $ratio identical"
  fi
  if [ "$errors" -gt 12 ]; then fail "$name $cut: $errors decode messages"; fi
}

for spec in "H.264|libx264 -x264-params repeat-headers=1" \
            "H.265|libx265 -tag:v hvc1 -x265-params log-level=error:repeat-headers=1"; do
  name=${spec%%|*}
  # shellcheck disable=SC2086
  make_clip ${spec#*|}
  vframes orig.mp4 > want_v.txt
  apackets orig.mp4 > want_a.txt
  check_video "$name" cut_end 290
  check_video "$name" cut_mid 150
done

# A file that looks like a DJI Osmo Pocket recording: its first box after
# the type box is a "free" box naming the camera, as DJI writes.
python3 - <<'PY'
d = open("cut_end.mp4", "rb").read()
ftyp = int.from_bytes(d[:4], "big")
free = b"\0\0\0\x20free" + b"DJI OsmoPocket 3 FC2101\0" + b"\0" * 1
free = len(free).to_bytes(4, "big") + free[4:]
open("DJI_0001.MP4", "wb").write(d[:ftyp] + free + d[ftyp:])
PY
"$reperio" repair DJI_0001.MP4 -o dji_fixed.mp4 > dji.log 2>&1
if grep -q "DJI Osmo Pocket" dji.log && [ "$(ffmpeg -v error -i dji_fixed.mp4 -f null - 2>&1 | wc -l)" -le 12 ]; then
  pass "DJI file recognised as a DJI Osmo Pocket"
else
  fail "DJI file: $(cat dji.log)"
fi

# Without parameter sets in the stream, a good clip from the same camera
# is the guide (the sound is rebuilt too).
mkdir guide && cd guide || exit 1
make_clip libx264
vframes orig.mp4 > want_v.txt
apackets orig.mp4 > want_a.txt
ffmpeg -loglevel error -y -f lavfi -i testsrc2=duration=3:size=640x360:rate=30 -f lavfi -i sine=duration=3:sample_rate=48000 -ac 2 \
  -c:v libx264 -pix_fmt yuv420p -g 30 -c:a aac -shortest ref.mp4
if "$reperio" repair cut_end.mp4 -r ref.mp4 -o guided.mp4 > guided.log 2>&1; then
  vframes guided.mp4 > got_v.txt
  apackets guided.mp4 > got_a.txt
  if diff -q <(head -n "$(wc -l < got_v.txt)" want_v.txt) got_v.txt > /dev/null && [ "$(wc -l < got_v.txt)" -ge 290 ] && [ "$(wc -l < got_a.txt)" -ge 450 ]; then
    pass "video with a guide (no parameter sets in the stream): $(wc -l < got_v.txt) frames identical, $(wc -l < got_a.txt) sound packets"
  else
    fail "video with a guide: $(wc -l < got_v.txt) frames, $(wc -l < got_a.txt) sound packets"
  fi
else
  fail "video with a guide: $(cat guided.log)"
fi
if "$reperio" repair cut_end.mp4 -o noguide.mp4 > noguide.log 2>&1; then
  fail "video without parameter sets or a guide should not be repaired"
else
  pass "video without parameter sets or a guide: refused with a message ($(head -c 90 noguide.log))"
fi
cd ..

# --------------------------------------------------------------- photos

python3 - <<'PY'
from PIL import Image, ImageDraw
import io, struct

def scene(seed):
    im = Image.new("RGB", (1600, 1200))
    px = im.load()
    for y in range(1200):
        for x in range(1600):
            px[x, y] = ((x * 3 + seed * 40) % 256, (y * 2 + seed) % 256, ((x ^ y) + seed) % 256)
    d = ImageDraw.Draw(im)
    for k in range(40):
        d.ellipse([k * 37, k * 25, k * 37 + 90, k * 25 + 90], outline=(255, 255 - k * 5, k * 6), width=4)
    return im

def exif(make, model):
    # little-endian TIFF with Make and Model
    mk, md = make.encode() + b"\0", model.encode() + b"\0"
    n = 2
    ifd = 8
    data_at = ifd + 2 + 12 * n + 4
    entries = struct.pack("<HHII", 0x010F, 2, len(mk), data_at) + struct.pack("<HHII", 0x0110, 2, len(md), data_at + len(mk))
    tiff = b"II*\0" + struct.pack("<I", ifd) + struct.pack("<H", n) + entries + struct.pack("<I", 0) + mk + md
    return b"Exif\0\0" + tiff

a, b = scene(1), scene(2)
for name, im in (("a.jpg", a), ("b.jpg", b)):
    im.save(name, quality=88, exif=exif("TestCam", "T-1"))
# a photo with a smaller copy of itself inside, as many cameras store
small = a.resize((320, 240))
a.save("mpo.jpg", format="MPO", save_all=True, append_images=[small], quality=88)
PY

python3 - <<'PY'
d = open("a.jpg", "rb").read()
sos = d.find(b"\xff\xda")
# wipe the quantisation and Huffman tables, keep the scan header and data
tables_end = sos
start = d.find(b"\xff\xdb")
b = bytearray(d)
for i in range(start, tables_end):
    b[i] = 0
open("a_notables.jpg", "wb").write(bytes(b))
open("a_cut.jpg", "wb").write(d[: len(d) * 55 // 100] + bytes(5000))
m = open("mpo.jpg", "rb").read()
second = m.find(b"\xff\xd8\xff", 4 + m[4 + 2] * 256 + m[4 + 3])
assert second > 0, "no second image"
mb = bytearray(m)
for i in range(0, second - 2000):
    mb[i] = 0x5A
open("mpo_broken.jpg", "wb").write(bytes(mb))
PY

"$reperio" repair a_notables.jpg -r b.jpg -o a_fixed.jpg > photo.log 2>&1
python3 - <<'PY' && pass "JPEG without tables: pixels identical to the original" || fail "JPEG without tables: $(cat photo.log)"
from PIL import Image, ImageChops
x = Image.open("a_fixed.jpg"); x.load()
y = Image.open("a.jpg"); y.load()
assert x.size == y.size
assert ImageChops.difference(x, y).getbbox() is None
PY

"$reperio" repair a_cut.jpg -o a_cut_fixed.jpg > photo.log 2>&1
python3 - <<'PY' && pass "cut-off JPEG: decodes at full size, top part identical" || fail "cut-off JPEG: $(cat photo.log)"
from PIL import Image, ImageChops
x = Image.open("a_cut_fixed.jpg"); x.load()
y = Image.open("a.jpg"); y.load()
assert x.size == y.size
top = (0, 0, 1600, 400)
assert ImageChops.difference(x.crop(top), y.crop(top)).getbbox() is None
PY

"$reperio" repair mpo_broken.jpg -o mpo_preview.jpg > photo.log 2>&1
python3 - <<'PY' && pass "damaged photo: embedded preview rescued" || fail "damaged photo: $(cat photo.log)"
from PIL import Image
x = Image.open("mpo_preview.jpg"); x.load()
assert x.size == (320, 240), x.size
PY

# ------------------------------------------------------------ documents

python3 - <<'PY'
paras = "".join(f"<p>Paragraph {i}: the quick brown fox jumps over the lazy dog, number {i}.</p>" for i in range(300))
open("report.html", "w").write(f"<html><body><h1>Quarterly report</h1>{paras}</body></html>")
open("sheet.csv", "w").write("name,qty\n" + "\n".join(f"item{i},{i}" for i in range(2000)))
open("deck.fodp", "w").write('''<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" office:version="1.2" office:mimetype="application/vnd.oasis.opendocument.presentation">
<office:body><office:presentation>
<draw:page draw:name="p1"><draw:frame svg:width="20cm" svg:height="3cm" svg:x="2cm" svg:y="2cm"><draw:text-box><text:p>Slide one title text</text:p></draw:text-box></draw:frame></draw:page>
<draw:page draw:name="p2"><draw:frame svg:width="20cm" svg:height="3cm" svg:x="2cm" svg:y="2cm"><draw:text-box><text:p>Slide two body text</text:p></draw:text-box></draw:frame></draw:page>
</office:presentation></office:body></office:document>''')
PY
quiet soffice --headless --convert-to "docx:MS Word 2007 XML" report.html
quiet soffice --headless --convert-to xlsx sheet.csv
quiet soffice --headless --convert-to pptx deck.fodp
quiet soffice --headless --convert-to "doc:MS Word 97" report.html
quiet soffice --headless --convert-to pdf report.docx

damage() { python3 "$here/repair_damage.py" "$@"; }

# Opens a file with LibreOffice and writes its text (or CSV) to out/.
office_text() {
  rm -rf out; mkdir out
  quiet soffice --headless --convert-to "${2:-txt:Text}" --outdir out "$1"
  cat out/* 2> /dev/null
}

damage report.docx nodir r_nodir.docx
damage report.docx cut r_cut.docx
damage sheet.xlsx nodir s_nodir.xlsx
damage sheet.xlsx cut s_cut.xlsx
damage deck.pptx nodir d_nodir.pptx
damage report.pdf xref p_xref.pdf
damage report.pdf cut p_cut.pdf
damage report.doc zero w_zero.doc

"$reperio" repair r_nodir.docx -o r_nodir_fixed.docx > doc.log 2>&1
if office_text r_nodir_fixed.docx | grep -q "Paragraph 299:"; then pass "DOCX without its index: all 300 paragraphs"; else fail "DOCX without its index: $(cat doc.log)"; fi

"$reperio" repair r_cut.docx -o r_cut_fixed.docx > doc.log 2>&1
if office_text r_cut_fixed.docx | grep -q "Paragraph 0:" && [ -s r_cut_fixed.txt ] && grep -q "Paragraph 0:" r_cut_fixed.txt; then
  pass "DOCX with its text cut: opens with $(office_text r_cut_fixed.docx | grep -c Paragraph) paragraphs, text kept beside"
else
  fail "DOCX with its text cut: $(cat doc.log)"
fi

"$reperio" repair s_nodir.xlsx -o s_nodir_fixed.xlsx > doc.log 2>&1
if office_text s_nodir_fixed.xlsx csv | grep -q "item1999,1999"; then pass "XLSX without its index: all 2000 rows"; else fail "XLSX without its index: $(cat doc.log)"; fi

"$reperio" repair s_cut.xlsx -o s_cut_fixed.xlsx > doc.log 2>&1
rows=$(office_text s_cut_fixed.xlsx csv | wc -l)
if [ "$rows" -gt 100 ]; then pass "XLSX with its sheet cut: opens with $rows rows"; else fail "XLSX with its sheet cut: $rows rows. $(cat doc.log)"; fi

"$reperio" repair d_nodir.pptx -o d_nodir_fixed.pptx > doc.log 2>&1
rm -rf out; mkdir out
quiet soffice --headless --convert-to pdf --outdir out d_nodir_fixed.pptx
if [ -f out/d_nodir_fixed.pdf ] && pdftotext out/d_nodir_fixed.pdf - 2> /dev/null | grep -q "Slide two body text"; then pass "PPTX without its index: both slides"; else fail "PPTX without its index: $(cat doc.log)"; fi

"$reperio" repair p_xref.pdf -o p_xref_fixed.pdf > doc.log 2>&1
# Strict readers go by the table: its start must point at it and every
# entry at the object it names. (Poppler rebuilds a bad table silently.)
xref_ok() {
  python3 - "$1" <<'PY'
import re, sys
d = open(sys.argv[1], "rb").read()
m = list(re.finditer(rb"startxref\s+(\d+)", d))
if not m:
    sys.exit(1)
at = int(m[-1].group(1))
if d[at:at + 4] != b"xref":
    sys.exit(1)
pos = at + 4
n = 0
while True:
    h = re.match(rb"\s*(\d+) (\d+)\s*\n", d[pos:pos + 40])
    if not h:
        break
    first, count = int(h.group(1)), int(h.group(2))
    pos += h.end()
    for k in range(count):
        e = d[pos:pos + 20]
        pos += 20
        if e[17:18] == b"n":
            off = int(e[:10])
            if not re.match(rb"%d \d+ obj" % (first + k), d[off:off + 30]):
                sys.exit(1)
            n += 1
sys.exit(0 if n > 10 else 1)
PY
}
if ! xref_ok p_xref.pdf && xref_ok p_xref_fixed.pdf && pdftotext p_xref_fixed.pdf - 2> /dev/null | grep -q "Paragraph 299:"; then
  pass "PDF with a damaged table: table valid after repair, all text there"
else
  fail "PDF with a damaged table: $(cat doc.log)"
fi

"$reperio" repair p_cut.pdf -o p_cut_fixed.pdf > doc.log 2>&1
if [ -s p_cut_fixed.pdf ] && [ "$(pdftotext p_cut_fixed.pdf /dev/null 2>&1 | wc -l)" -eq 0 ] || { [ -s p_cut_fixed.txt ] && grep -q "Paragraph" p_cut_fixed.txt; }; then
  pass "PDF cut short: opens, text kept beside"
else
  fail "PDF cut short: $(cat doc.log)"
fi

"$reperio" repair w_zero.doc -o w_zero_text.txt > doc.log 2>&1
if grep -q "Paragraph 150:" w_zero_text.txt 2> /dev/null; then pass "Word 97 file with a zeroed header: text rescued"; else fail "Word 97 file: $(cat doc.log)"; fi

# -------------------------------------------------------------- card scan

# A FAT card with deleted files. DJI_0001: parameter sets in the stream, so
# repairable alone. DJI_0002: none, so only its .LRF copy can be offered.
ffmpeg -loglevel error -y -f lavfi -i testsrc=duration=6:size=160x90:rate=30 -f lavfi -i sine=duration=6 \
  -c:v libx264 -pix_fmt yuv420p -g 30 -c:a aac -shortest lrf.mp4
ffmpeg -loglevel error -y -f lavfi -i testsrc2=duration=6:size=640x360:rate=30 -f lavfi -i sine=duration=6 \
  -c:v libx264 -pix_fmt yuv420p -g 30 -c:a aac -shortest plain.mp4
python3 - <<'PY'
import struct
def cut(src, dst):
    d = open(src, "rb").read()
    pos, boxes = 0, {}
    while pos < len(d):
        size, typ = struct.unpack(">I4s", d[pos:pos + 8]); boxes[typ] = pos; pos += size
    open(dst, "wb").write(d[:boxes[b"moov"]])
cut("plain.mp4", "DJI_0002.MP4")
PY
cp cut_end.mp4 DJI_0001.MP4
cp lrf.mp4 DJI_0001.LRF
cp lrf.mp4 DJI_0002.LRF
cp a_notables.jpg IMG_0001.JPG
cp b.jpg IMG_0002.JPG
cp a_cut.jpg IMG_0003.JPG
cp r_nodir.docx report.docx.broken
cp p_xref.pdf notes.pdf

truncate -s 96M card.img
mkfs.vfat -F 32 -n CARD card.img > /dev/null
export MTOOLS_SKIP_CHECK=1
mmd -i card.img ::DCIM ::DCIM/100MEDIA ::DOCS
for f in DJI_0001.MP4 DJI_0001.LRF DJI_0002.MP4 DJI_0002.LRF IMG_0001.JPG IMG_0002.JPG IMG_0003.JPG; do mcopy -i card.img "$f" ::DCIM/100MEDIA/; done
mcopy -i card.img r_nodir.docx ::DOCS/Report.docx
mcopy -i card.img s_cut.xlsx ::DOCS/Sheet.xlsx
mcopy -i card.img p_xref.pdf ::DOCS/Notes.pdf
mdel -i card.img ::DCIM/100MEDIA/* ::DOCS/*

rm -rf rec
"$reperio" scan card.img -o rec --layout by-type > scan.log 2>&1 || { fail "scan: $(tail -5 scan.log)"; }
find rec -type f | sort | sed 's/^/     /'

count_files() { find rec -type f -iname "$1" | wc -l; }
dji=$(find rec -type f -iname '*0001*.mp4' ! -iname '*low*' | head -1)
if [ -n "$dji" ] && [ "$(ffmpeg -v error -i "$dji" -f null - 2>&1 | wc -l)" -le 12 ] && [ "$(apackets "$dji" | wc -l)" -gt 100 ]; then
  pass "scan: DJI_0001 recovered from its own stream, with sound"
else
  fail "scan: DJI_0001 not recovered (found: ${dji:-nothing})"
fi
lowres=$(find rec -type f -iname '*0002*low-resolution*' | head -1)
if [ -n "$lowres" ] && [ "$(ffmpeg -v error -i "$lowres" -f null - 2>&1 | wc -l)" -eq 0 ]; then
  pass "scan: DJI_0002 could not be repaired; its .LRF offered as the low-resolution copy"
else
  fail "scan: no low-resolution copy for DJI_0002"
fi
photos=0 decoded=0
while IFS= read -r -d '' f; do
  photos=$((photos + 1))
  python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).load()" "$f" 2> /dev/null && decoded=$((decoded + 1))
done < <(find rec -type f -iname '*.jpg' -print0)
if [ "$photos" -ge 3 ] && [ "$decoded" -eq "$photos" ]; then pass "scan: $photos photos recovered (one without its tables, one cut short), all decode"; else fail "scan: $decoded of $photos photos decode"; fi
docs=$(find rec -type f \( -iname '*.docx' -o -iname '*.pdf' -o -iname '*.xlsx' \) | wc -l)
if [ "$docs" -ge 3 ] && python3 -c "import zipfile,sys; assert zipfile.ZipFile(sys.argv[1]).testzip() is None" rec/Documents/Report.docx; then
  pass "scan: $docs documents recovered, Report.docx is a sound ZIP"
else
  fail "scan: $docs documents recovered"
fi

exit $failed

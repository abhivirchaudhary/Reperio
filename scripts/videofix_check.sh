#!/bin/bash
# Checks video repair end to end with real encoders.
#
#   scripts/videofix_check.sh [path/to/reperio]
#
# For H.264 and H.265 (both MP4 sample entry types), encodes a reference
# video and a ten-second clip with B-frames and AAC sound, then cuts the
# clip's index off (as when it was lost) and cuts the recording at 60% (as
# when the battery died). Each is repaired with the reference as a guide
# and must decode without errors, every frame identical to the original
# and in the original order. Needs ffmpeg with libx264 and libx265.
set -euo pipefail
reperio=$(realpath "${1:-target/release/reperio}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
failed=0

frames() { ffmpeg -v error -i "$1" -map 0:v -f framemd5 - | grep -v '^#' | awk -F, '{print $6}'; }

check() {
  local name=$1; shift
  ffmpeg -loglevel error -y -f lavfi -i testsrc2=duration=4:size=640x360:rate=30 -f lavfi -i sine=duration=4 \
    -c:v "$@" -pix_fmt yuv420p -g 30 -c:a aac -shortest ref.mp4
  ffmpeg -loglevel error -y -f lavfi -i testsrc=duration=10:size=640x360:rate=30 -f lavfi -i sine=duration=10 \
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
assert moov[0] > mdat[0], "expected the index after the media data"
open("cut_end.mp4", "wb").write(d[:moov[0]])
open("cut_mid.mp4", "wb").write(d[:mdat[0] + int(mdat[1] * 0.6)])
PY
  frames orig.mp4 > want.txt
  for cut in cut_end cut_mid; do
    rm -f fixed.mp4
    "$reperio" fix-video "$cut.mp4" -r ref.mp4 -o fixed.mp4 > /dev/null
    errors=$(ffmpeg -v error -i fixed.mp4 -f null - 2>&1 | wc -l)
    frames fixed.mp4 > got.txt
    n=$(wc -l < got.txt)
    if [ "$errors" -ne 0 ] || [ "$n" -lt 30 ] || ! diff -q <(head -n "$n" want.txt) got.txt > /dev/null; then
      echo "FAIL $name $cut: $errors decode errors, $n frames, order or content differs"
      failed=1
    else
      echo "ok   $name $cut: $n frames identical, in order"
    fi
  done
}

check "H.264" libx264
check "H.265 hvc1" libx265 -tag:v hvc1 -x265-params log-level=error
check "H.265 hev1" libx265 -x265-params log-level=error
exit $failed

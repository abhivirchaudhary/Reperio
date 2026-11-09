#!/bin/sh
# End to end: a device served by a source plug-in is scanned and a photo is
# recovered from it, byte for byte. Needs python3 with Pillow.
#
#   cargo build -p reperio && scripts/source_plugin_check.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin="$here/target/debug/reperio"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/dumps"

python3 - "$work/dumps/card.img" "$work/photo.jpg" <<'PY'
import io, sys
from PIL import Image
buf = io.BytesIO()
Image.effect_noise((640, 480), 60).convert("RGB").save(buf, "JPEG", quality=85)
jpg = buf.getvalue()
disk = bytearray(8 << 20)
disk[1 << 20:(1 << 20) + len(jpg)] = jpg   # a photo in otherwise empty space
open(sys.argv[1], "wb").write(disk)
open(sys.argv[2], "wb").write(jpg)
PY

export REPERIO_PLUGINS="$here/plugins/examples" REPERIO_EXAMPLE_DUMPS="$work/dumps"
"$bin" devices | grep -q "plugin:source-file-example:file:card.img"
"$bin" scan "plugin:source-file-example:file:card.img" -o "$work/out" >/dev/null
found=$(find "$work/out" -name '*.jpg')
[ -n "$found" ] && cmp "$found" "$work/photo.jpg"
echo "source plug-in: scanned a served device and recovered the photo, identical"

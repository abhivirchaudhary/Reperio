#!/bin/sh
# End to end: one recipe chains a source plug-in (serves a device), an analyzer
# plug-in (offers a file nothing else could find) and the normal scan, and the
# recovered files are byte for byte right. Needs python3 with Pillow.
#
#   cargo build -p reperio && scripts/recipe_check.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin="$here/target/debug/reperio"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/dumps" "$work/plugins"

python3 - "$work" <<'PY'
import io, sys
from PIL import Image
w = sys.argv[1]
buf = io.BytesIO()
Image.effect_noise((640, 480), 60).convert("RGB").save(buf, "JPEG", quality=85)
jpg = buf.getvalue()
disk = bytearray(8 << 20)
disk[1 << 20:(1 << 20) + len(jpg)] = jpg                       # a photo the deep scan finds
secret = b"SECRETFILE:hello from the chain\n"
disk[2 << 20:(2 << 20) + len(secret)] = secret                 # a file only the analyzer knows about
open(w + "/dumps/card.img", "wb").write(disk)
open(w + "/photo.jpg", "wb").write(jpg)
open(w + "/secret.txt", "wb").write(secret)
PY

cp "$here/plugins/examples/source_file.py" "$work/plugins/"
cp "$here/plugins/sdk/python/_reperio.py" "$work/plugins/"
cat > "$work/plugins/marker-finder.py" <<'PY'
import _reperio as reperio
plugin = reperio.Plugin(name="marker-finder", kind="analyzer", targets=["image"],
                        options=[reperio.Option("marker", "Marker", default="NOTHING")])

@plugin.analyze
def analyze(run):
    marker = run.options["marker"].encode()
    block = 1 << 20
    for off in range(0, run.image_len, block):
        data = run.read(off, min(block, run.image_len - off))
        at = data.find(marker)
        if at >= 0:
            end = data.find(b"\n", at) + 1
            run.file("notes/secret.txt", end - at, [(off + at, end - at)])
            run.finding("Found it", f"at {off + at}")

plugin.run()
PY
chmod +x "$work/plugins/"*.py

cat > "$work/recipe.json" <<JSON
{
  "source": "plugin:source-file-example:file:card.img",
  "analyzers": ["marker-finder"],
  "options": { "marker-finder": { "marker": "SECRETFILE" } },
  "recover": "$work/out",
  "reports": "$work/reports"
}
JSON

REPERIO_PLUGINS="$work/plugins" REPERIO_EXAMPLE_DUMPS="$work/dumps" REPERIO_PLUGIN_QUIET=1 "$bin" recipe run "$work/recipe.json" >/dev/null 2>&1
cmp "$work/out/notes/secret.txt" "$work/secret.txt"
photo=$(find "$work/out" -name '*.jpg' | head -n 1)
[ -n "$photo" ] && cmp "$photo" "$work/photo.jpg"
echo "recipe: a source plug-in, an analyzer and the scan chained; both files recovered, identical"

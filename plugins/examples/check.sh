#!/bin/sh
# Builds every example plug-in, runs Reperio with each on a test image and
# checks that the deleted file comes back byte for byte.
#
#     plugins/examples/check.sh            (from the repo root, after `cargo build --release`)
set -e
here=$(cd "$(dirname "$0")" && pwd)
reperio="${REPERIO:-$here/../../target/release/reperio}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
python3 "$here/make_toy_image.py" "$work/toy.img" >/dev/null

mkdir "$work/bin"
cp "$here/toyfs.py" "$work/bin/"
rustc --edition 2021 -O "$here/toyfs.rs" -o "$work/bin/toyfs-rust"
cc -O2 -o "$work/bin/toyfs-c" "$here/toyfs.c"
c++ -std=c++17 -O2 -o "$work/bin/toyfs-cpp" "$here/toyfs.cpp"

fail=0
for p in toyfs.py toyfs-rust toyfs-c toyfs-cpp; do
  plugins="$work/p-$p"; mkdir "$plugins"; cp "$work/bin/$p" "$plugins/"
  REPERIO_PLUGINS="$plugins" "$reperio" scan "$work/toy.img" -o "$work/out-$p" --damaged >/dev/null 2>&1 || true
  got=$(find "$work/out-$p" -name beach.jpg 2>/dev/null | head -1)
  if [ -n "$got" ] && dd if="$work/toy.img" bs=1 skip=30000 count=6000 2>/dev/null | cmp -s - "$got"; then
    echo "ok    $p"
  else
    echo "FAIL  $p"; fail=1
  fi
done
exit $fail

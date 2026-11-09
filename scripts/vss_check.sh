#!/bin/bash
# Checks that files deleted from a Windows volume after a shadow copy was
# taken come back from the shadow copy, byte for byte.
#
#   sudo scripts/vss_check.sh [path/to/reperio]
#
# Builds an NTFS volume with mkfs.ntfs and ntfs-3g holding a large file, a
# small one (stored inside the MFT) and a placeholder store file, keeps a
# copy of it as it is, then deletes the two files and writes over their
# space. scripts/vss_store.py then adds the shadow copy Windows would have
# kept. Needs root, mkfs.ntfs and ntfs-3g.
set -euo pipefail
reperio=$(realpath "${1:-target/release/reperio}")
store=$(realpath "$(dirname "$0")/vss_store.py")
work=$(mktemp -d)
trap 'umount "$work/m" 2>/dev/null || true; rm -rf "$work"' EXIT
cd "$work"
truncate -s 64M vol.img
mkfs.ntfs -Q -q -F -c 4096 -L shadow vol.img > /dev/null 2>&1
mkdir m
ntfs-3g vol.img m
python3 - <<'PY'
import os, random
r = random.Random(7)
open("gone.bin", "wb").write(bytes(r.getrandbits(8) for _ in range(300_000)))
open("note.txt", "wb").write(b"A short note, small enough to live in its MFT record.\n")
for name in ("gone.bin", "note.txt"):
    os.makedirs("m/Users/me", exist_ok=True)
    open(f"m/Users/me/{name}", "wb").write(open(name, "rb").read())
open("m/keep.bin", "wb").write(bytes(r.getrandbits(8) for _ in range(100_000)))
# The store's space, set aside before the shadow copy, as Windows does.
os.makedirs("m/System Volume Information")
with open("m/System Volume Information/store", "wb") as f:
    for i in range(400):
        piece = bytearray(b"\0" * 0x4000)
        mark = b"REPERIO-VSS-PIECE"
        piece[:len(mark)] = mark
        piece[-len(mark):] = mark
        f.write(piece)
PY
umount m
cp vol.img before.img

ntfs-3g vol.img m
rm m/Users/me/gone.bin m/Users/me/note.txt
python3 - <<'PY'
import os
# Fill the free space so the deleted files' clusters are reused.
with open("m/filler.bin", "wb") as f:
    try:
        while True:
            f.write(b"\xEE" * (1 << 20))
            f.flush()
    except OSError:
        pass
PY
umount m
python3 "$store" before.img vol.img

"$reperio" scan --list vol.img
"$reperio" scan -o out vol.img > /dev/null 2>&1
fail=0
# gone.bin's clusters were written over, so only the shadow copy has it.
# note.txt's MFT record may still hold it, and then it is listed once, as
# an ordinary deleted file.
for name in gone.bin note.txt; do
  where=out/Snapshots
  [ "$name" = note.txt ] && where=out
  got=$(find "$where" -path "*/Users/me/$name" | head -1)
  if [ -n "$got" ] && cmp -s "$got" "$name"; then
    echo "ok: $name ($got)"
  else
    echo "FAIL: $name not recovered intact"
    fail=1
  fi
done
if find out/Snapshots -name keep.bin | grep -q .; then
  echo "FAIL: keep.bin is still on the volume but listed from the snapshot"
  fail=1
fi
exit $fail

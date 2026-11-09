#!/bin/bash
# Checks that deleted NTFS files whose file records have been reused still
# come back, from the journals and old folder index entries.
#
#   sudo scripts/ntfs_names_check.sh [path/to/reperio]
#
# On an ntfs-3g volume: deletes the folder Holiday (a file stored in
# clusters and one stored inside its record) and Docs/zzz_report.zip,
# then makes new files until their records are reused. Then plants what
# Windows would have logged (see scripts/ntfs_names.py) and checks:
#   - Holiday/victim.bin and Holiday/small.txt come back byte for byte
#     from $LogFile, in the folder the change journal names;
#   - the deep scan's find of zzz_report.zip takes its name and folder
#     from the stale entry ntfs-3g left in Docs' index.
# Needs root, mkfs.ntfs and ntfs-3g.
set -euo pipefail
reperio=$(realpath "${1:-target/release/reperio}")
names=$(realpath "$(dirname "$0")/ntfs_names.py")
# KEEP=dir keeps the image and logs there, to look into a failure.
if [ -n "${KEEP:-}" ]; then
  work=$(realpath -m "$KEEP"); rm -rf "$work"; mkdir -p "$work"
  trap 'umount "$work/m" 2>/dev/null || true' EXIT
else
  work=$(mktemp -d)
  trap 'umount "$work/m" 2>/dev/null || true; rm -rf "$work"' EXIT
fi
cd "$work"
truncate -s 256M vol.img
mkfs.ntfs -Q -q -F -L names vol.img > /dev/null 2>&1
mkdir m
ntfs-3g -o streams_interface=windows vol.img m
python3 - <<'PY'
import os, random, zipfile
rnd = random.Random(7)
os.makedirs("m/Holiday")
open("m/Holiday/victim.bin", "wb").write(bytes(rnd.getrandbits(8) for _ in range(300_000)))
open("m/Holiday/small.txt", "wb").write(b"remember the milk\n" * 22)
os.makedirs("m/Docs")
for i in range(150):
    open(f"m/Docs/f{i:03}.txt", "wb").write(b"x" * (10 + i))
with zipfile.ZipFile("m/Docs/zzz_report.zip", "w", zipfile.ZIP_STORED) as z:
    z.writestr("report.bin", bytes(rnd.getrandbits(8) for _ in range(60_000)))
os.makedirs("expect")
for f in ["Holiday/victim.bin", "Holiday/small.txt", "Docs/zzz_report.zip"]:
    os.makedirs(os.path.dirname(f"expect/{f}"), exist_ok=True)
    open(f"expect/{f}", "wb").write(open(f"m/{f}", "rb").read())
PY
umount m
python3 "$names" capture vol.img state.json

ntfs-3g -o streams_interface=windows vol.img m
# The change journal's data is written first, while the files still hold
# their clusters, so it cannot land in the space they are about to free.
touch m/usnstage
python3 "$names" usn m state.json
rm -rf m/Holiday m/Docs/zzz_report.zip
# New small files, stored inside their records, reuse the freed records
# without touching the deleted files' clusters.
for i in $(seq 1 40); do echo "new $i" > "m/new$i.txt"; done
umount m
python3 "$names" plant vol.img state.json

failed=0
same() {
  if cmp -s "expect/$2" "$1/$2"; then echo "ok   $3: $2 byte for byte"
  else echo "FAIL $3: $2 missing or different"; failed=1; fi
}
"$reperio" scan --quick vol.img -o quick > quick.log 2>&1 || true
same quick Holiday/victim.bin "\$LogFile"
same quick Holiday/small.txt "\$LogFile"
if grep -q "2 whole from the journal, 1 names from the change journal" quick.log; then
  echo "ok   counts: $(grep -o 'Records of reused files.*' quick.log)"
else
  echo "FAIL counts: $(grep -o 'Records of reused files.*' quick.log || echo none)"; failed=1
fi
"$reperio" scan vol.img -o deep > deep.log 2>&1 || true
same deep Docs/zzz_report.zip "\$I30 slack"
exit $failed

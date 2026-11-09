#!/bin/bash
# Checks that NTFS volumes with damaged or unusual structures still give
# back their deleted files.
#
#   sudo scripts/ntfs_damage_check.sh [path/to/reperio]
#
# Builds an NTFS volume with 30,000 small files (most of them stored inside
# the MFT itself), deletes 2,500, then breaks one structure at a time (see
# scripts/ntfs_damage.py) and all of them together. Every variant must give
# back all 2,500 deleted files, byte for byte. Needs root, mkfs.ntfs and
# ntfs-3g.
set -euo pipefail
reperio=$(realpath "${1:-target/release/reperio}")
damage=$(realpath "$(dirname "$0")/ntfs_damage.py")
work=$(mktemp -d)
trap 'umount "$work/m" 2>/dev/null || true; rm -rf "$work"' EXIT
cd "$work"
truncate -s 4G vol.img
mkfs.ntfs -Q -q -F -L check vol.img > /dev/null 2>&1
mkdir m
ntfs-3g vol.img m
python3 - <<'PY'
import os, shutil
for d in range(60):
    os.makedirs(f"m/dir{d}")
    for i in range(500):
        with open(f"m/dir{d}/f{i}.txt", "wb") as f:
            f.write(b"x" * (200 + i))
for d in range(5):
    shutil.rmtree(f"m/dir{d}")
PY
umount m

verify() {
  python3 - "$1" <<'PY'
import os, re, sys
good = bad = 0
for dp, _, fs in os.walk(sys.argv[1]):
    for f in fs:
        m = re.match(r"f(\d+)\.txt$", f)
        if m:
            ok = open(os.path.join(dp, f), "rb").read() == b"x" * (200 + int(m.group(1)))
            good, bad = good + ok, bad + (not ok)
print(good, bad)
PY
}

failed=0
for kinds in none split-mft no-record0 no-bitmap no-boot "split-mft no-record0 no-bitmap no-boot"; do
  if [ "$kinds" = none ]; then cp --sparse=always vol.img dmg.img; else python3 "$damage" vol.img dmg.img $kinds; fi
  rm -rf out
  "$reperio" scan --quick dmg.img -o out > /dev/null 2>&1 || true
  read -r good bad < <(verify out)
  if [ "$good" -eq 2500 ] && [ "$bad" -eq 0 ]; then
    echo "ok   $kinds: 2500 deleted files byte for byte"
  else
    echo "FAIL $kinds: $good byte for byte, $bad wrong, of 2500"
    failed=1
  fi
done
exit $failed

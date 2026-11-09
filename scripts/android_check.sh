#!/bin/bash
# Tests `reperio android` against a fake `adb` (no phone needed):
#   scripts/android_check.sh        (after cargo build --release)
set -euo pipefail
cd "$(dirname "$0")/.."
reperio="${REPERIO:-target/release/reperio}"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# The fake phone: files under $work/phone/sdcard, a partition in $work/userdata.img
mkdir -p "$work/phone/sdcard/DCIM/.thumbnails" "$work/phone/sdcard/Pictures" "$work/phone/sdcard/DCIM/Camera"
echo "photo bytes" > "$work/phone/sdcard/Pictures/.trashed-1893456000-IMG_0001.jpg"
echo "other photo" > "$work/phone/sdcard/DCIM/Camera/.trashed-1893456001-IMG_0002.jpg"
echo "thumb" > "$work/phone/sdcard/DCIM/.thumbnails/1234"
echo "alive" > "$work/phone/sdcard/DCIM/Camera/kept.jpg"
head -c 3000000 /dev/urandom > "$work/userdata.img"
mkdir -p "$work/phone/parts"
head -c 100000 /dev/urandom > "$work/phone/parts/boot"
head -c 200000 /dev/urandom > "$work/phone/parts/system"
head -c 3500000 /dev/urandom > "$work/phone/parts/disk"

cp scripts/mock_adb.py "$work/adb"
chmod +x "$work/adb"
export ADB="$work/adb" MOCK_PHONE="$work/phone" MOCK_IMG="$work/userdata.img"
fail=0; ok() { echo "ok    $1"; }; bad() { echo "FAIL  $1"; fail=1; }

# 1. No root: trash is put back under the original names, and nothing on the phone changes.
before=$(find "$work/phone" -type f | sort | xargs md5sum | md5sum)
MOCK_ROOT=0 MOCK_CRYPTO=encrypted "$reperio" android pull "$work/out" > "$work/pull.txt"
after=$(find "$work/phone" -type f | sort | xargs md5sum | md5sum)
[ "$before" = "$after" ] && ok "phone untouched" || bad "phone changed"
[ "$(cat "$work/out/Recovered from trash/IMG_0001.jpg")" = "photo bytes" ] && [ -f "$work/out/Recovered from trash/IMG_0002.jpg" ] && ok "trash put back under original names" || bad "trash not restored"
[ -f "$work/out/left-behind/DCIM/.thumbnails/1234" ] && ok "thumbnails copied" || bad "thumbnails missing"
[ ! -e "$work/out/Recovered from trash/kept.jpg" ] && ok "live files left alone" || bad "copied live file"
grep -q "2030-" "$work/pull.txt" && ok "says when Android will delete them" || bad "no expiry date"

# 2. Encrypted phone: imaging refused, nothing written.
if MOCK_ROOT=1 MOCK_CRYPTO=encrypted "$reperio" android image "$work/enc.img" >/dev/null 2>"$work/err.txt"; then bad "encrypted image allowed"; else
  [ ! -e "$work/enc.img" ] && grep -q "encrypted" "$work/err.txt" && ok "encrypted phone refused" || bad "encrypted refusal"; fi

# 3. No root: imaging refused.
if MOCK_ROOT=0 MOCK_CRYPTO=unencrypted "$reperio" android image "$work/x.img" >/dev/null 2>&1; then bad "unrooted image allowed"; else ok "unrooted phone refused"; fi

# 4. Rooted, unencrypted: an exact image with correct hashes, then a scan of it.
MOCK_ROOT=1 MOCK_CRYPTO=unencrypted "$reperio" android image "$work/ok.img" --scan-to "$work/scanned" > "$work/image.txt" 2>&1
cmp "$work/ok.img" "$work/userdata.img" && ok "image is byte-for-byte the partition" || bad "image differs"
want=$(sha256sum "$work/userdata.img" | cut -d' ' -f1)
grep -qi "$want" "$work/ok.img.hashes.txt" && ok "SHA-256 in the report is right" || bad "hash wrong"
grep -q "Scanning the image" "$work/image.txt" && grep -q "Scanned\|Found\|scan" "$work/image.txt" && ok "scan ran on the image" || bad "scan did not run"

# 5. The whole storage chip, and every partition.
MOCK_ROOT=1 MOCK_CRYPTO=unencrypted "$reperio" android image "$work/chip.img" --whole >/dev/null 2>&1
cmp "$work/chip.img" "$work/phone/parts/disk" && ok "whole storage chip imaged, exact" || bad "whole chip differs"
MOCK_ROOT=1 MOCK_CRYPTO=unencrypted "$reperio" android image "$work/all" --all >/dev/null 2>&1
if cmp "$work/all/userdata.img" "$work/userdata.img" && cmp "$work/all/boot.img" "$work/phone/parts/boot" && cmp "$work/all/system.img" "$work/phone/parts/system"; then ok "every partition imaged, exact"; else bad "partition images differ"; fi
grep -q "$(sha256sum "$work/phone/parts/boot" | cut -d' ' -f1)" "$work/all/MANIFEST.txt" && ok "manifest has each partition's hash" || bad "manifest hash"
MOCK_ROOT=1 MOCK_CRYPTO=encrypted "$reperio" android image "$work/enc-all" --all >/dev/null 2>&1 && bad "encrypted --all allowed" || { [ ! -e "$work/enc-all" ] && ok "encrypted phone refused for --all"; }
MOCK_ROOT=1 MOCK_CRYPTO=unencrypted "$reperio" android image "$work/bad.img" --whole --all >/dev/null 2>&1 && bad "--whole with --all" || ok "conflicting options refused"

# 6. A full logical copy of a phone without root has a manifest of fingerprints.
MOCK_ROOT=0 MOCK_CRYPTO=encrypted "$reperio" android pull "$work/logical" --everything >/dev/null 2>&1
want=$(sha256sum "$work/phone/sdcard/DCIM/Camera/kept.jpg" | cut -d' ' -f1)
grep -q "$want" "$work/logical/MANIFEST.sha256.txt" && [ -f "$work/logical/phone-storage/DCIM/Camera/kept.jpg" ] && ok "full copy has a hash manifest" || bad "no manifest"

# 7. Forensic plug-ins (analyzers): the example runs, with a report, hashes and an audit log.
mkdir "$work/plugins"; cp plugins/examples/analyzer_device_report.py "$work/plugins/"
export REPERIO_PLUGINS="$work/plugins"
MOCK_ROOT=0 MOCK_CRYPTO=encrypted "$reperio" android analyze "$work/analysis" > "$work/analyze.txt" 2>&1 || true
rep="$work/analysis/device-report-python"
grep -q "Files in Android's trash" "$work/analyze.txt" && ok "analyzer reports findings" || bad "no findings"
python3 - "$rep" <<'PY' && ok "report.json has the plug-in hash and fingerprints of its files" || bad "report.json"
import hashlib, json, sys
r = json.load(open(sys.argv[1] + "/report.json"))
assert r["plugin"] == "device-report-python" and len(r["plugin_sha256"]) == 64, r
assert r["error"] is None and r["refused"] == 0, r
o = {x["path"]: x for x in r["outputs"]}
assert set(o) == {"trashed-media.csv", "device-report.json"}, o
for p, x in o.items():
    assert hashlib.sha256(open(sys.argv[1] + "/" + p, "rb").read()).hexdigest() == x["sha256"]
PY
grep -q "shell \"pm list packages -3\"" "$rep/audit.log" && grep -q "pull /sdcard/Pictures/.trashed" "$rep/audit.log" && ok "audit log records what it asked the phone" || bad "audit log"
ls "$rep"/pulled/trashed/*IMG_000* >/dev/null 2>&1 && ok "plug-in copied a file off the phone" || bad "pull"
[ -f "$rep/MANIFEST.sha256.txt" ] && ok "manifest written" || bad "manifest"

# 8. Reperio does not limit plug-ins: any command goes through, and everything is shown.
rm -rf "$work/plugins"/*; cp scripts/free_analyzer.py "$work/plugins/"
MOCK_ROOT=1 MOCK_CRYPTO=unencrypted "$reperio" android analyze "$work/free-out" > "$work/free.txt" 2>&1 || true
[ -f "$work/phone/commands.log" ] && grep -q "touch /sdcard/made-by-plugin" "$work/phone/commands.log" && ok "any command the plug-in asks for is run on the phone" || bad "command not run"
grep -q "everything ran" "$work/free.txt" && ok "nothing was refused (shell, root, any adb command)" || bad "something refused: $(grep -i refused "$work/free.txt")"
grep -q "plain text from the plug-in" "$work/free.txt" && grep -q "a warning on the error output" "$work/free.txt" && ok "everything the plug-in printed is shown" || bad "console not shown"
grep -q "plain text from the plug-in" "$work/free-out/free/console.log" && grep -q "a warning on the error output" "$work/free-out/free/console.log" && ok "and kept in console.log" || bad "console.log"
grep -q "adb \[\"get-state\"\]" "$work/free-out/free/audit.log" && grep -q "shell (root)" "$work/free-out/free/audit.log" && ok "the audit log still records every request" || bad "audit log"

# 8b. A tool plug-in: any program, with arguments, no phone or image needed.
rm -rf "$work/plugins"/*; cp scripts/free_tool.py "$work/plugins/"
"$reperio" run free-tool -o "$work/tool-out" one "two words" --flag > "$work/tool.txt" 2>&1 || true
grep -q "tool started with args: \['one', 'two words', '--flag'\]" "$work/tool.txt" && ok "tool plug-in gets its arguments and its output is shown" || bad "tool output ($(tail -3 "$work/tool.txt"))"
grep -q "args=one,two words,--flag" "$work/tool-out/free-tool/result.txt" && ok "tool's file is kept" || bad "tool file"
python3 - "$work/tool-out/free-tool/report.json" <<'PY' && ok "the tool's file is hashed and listed, and exit 0 is success" || bad "tool report"
import json, sys
r = json.load(open(sys.argv[1]))
assert r["error"] is None and r["exit_code"] == 0, r
assert [o["path"] for o in r["outputs"]] == ["result.txt"] and len(r["outputs"][0]["sha256"]) == 64, r
PY

# 9. The same plug-in on an image reads through Reperio.
rm -rf "$work/plugins"/*; cp plugins/examples/analyzer_device_report.py "$work/plugins/"
"$reperio" analyze "$work/ok.img" "$work/img-analysis" > "$work/img.txt" 2>&1 || true
grep -q "Image size" "$work/img.txt" && grep -q "3,000,000" "$work/img.txt" && ok "analyzer works on an image" || bad "image analysis ($(tail -3 "$work/img.txt"))"
unset REPERIO_PLUGINS

# 10. Will not overwrite.
if MOCK_ROOT=1 MOCK_CRYPTO=unencrypted "$reperio" android image "$work/ok.img" >/dev/null 2>&1; then bad "overwrote an image"; else ok "refuses to overwrite"; fi
exit $fail

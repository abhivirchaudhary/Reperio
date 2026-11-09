#!/bin/sh
# Makes a starter plug-in of every kind in both languages and runs `reperio
# plugin test` on each, to keep the SDK and the protocol in step.
#
#   cargo build -p reperio && scripts/plugin_sdk_check.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin="$here/target/debug/reperio"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for lang in py js; do
  [ "$lang" = js ] && ! command -v node >/dev/null && continue
  for kind in tool analyzer reader source; do
    "$bin" plugin new "t-$kind" --kind "$kind" --lang "$lang" --dir "$work/$lang" >/dev/null
    "$bin" plugin test "$work/$lang/t-$kind.$lang" >/dev/null
    echo "ok: $kind ($lang)"
  done
done

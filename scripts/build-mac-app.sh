#!/bin/bash
# Builds Reperio.app and a .dmg installer on a Mac, and can install the app.
#
#   scripts/build-mac-app.sh             build Reperio.app and the .dmg
#   scripts/build-mac-app.sh --install   build, then put Reperio.app in /Applications and open it
#
# Needs the Xcode command line tools (xcode-select --install) and Rust
# (https://rustup.rs). The first build takes several minutes.
#
# With --install, INSTALL_DIR chooses another folder than /Applications
# (INSTALL_DIR=~/Applications) and NO_OPEN=1 installs without opening the app.
set -euo pipefail
cd "$(dirname "$0")/.."

install=0
for arg in "$@"; do
  case "$arg" in
    --install) install=1 ;;
    -h | --help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "Unknown option: $arg (see --help)" >&2; exit 2 ;;
  esac
done

# Where cargo puts its output: the usual target folder, or CARGO_TARGET_DIR.
# Made absolute, because the build below runs in a subfolder.
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
case "$target_dir" in /*) ;; *) target_dir="$PWD/$target_dir" ;; esac
export CARGO_TARGET_DIR="$target_dir"

if ! xcode-select -p >/dev/null 2>&1; then
  echo "Installing the Xcode command line tools first; run this script again when that finishes."
  xcode-select --install
  exit 1
fi
if ! command -v cargo >/dev/null; then
  echo "Rust is not installed. Install it with:"
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  exit 1
fi
if ! cargo tauri --version >/dev/null 2>&1; then
  echo "Installing the Tauri build tool (one time)..."
  cargo install tauri-cli --version "^2" --locked
fi

# One app that runs natively on both Apple silicon (M1 and later) and
# Intel Macs, so the .dmg can be given to any Mac.
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
# Signing. With a Developer ID certificate in the keychain and
# APPLE_SIGNING_IDENTITY set (e.g. "Developer ID Application: Name (TEAMID)"),
# the app is signed with it, and notarized by Apple when APPLE_ID,
# APPLE_PASSWORD (an app-specific password) and APPLE_TEAM_ID are set too.
# Otherwise it is signed ad hoc, which is fine on the Mac that built it.
config=()
if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
  echo "Signing with: $APPLE_SIGNING_IDENTITY"
  config=(--config "{\"bundle\":{\"macOS\":{\"signingIdentity\":\"$APPLE_SIGNING_IDENTITY\"}}}")
  if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
    echo "The app will be notarized by Apple (this takes a few minutes)."
  else
    echo "Not notarizing: set APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID to notarize."
  fi
fi
(cd app/src-tauri && cargo tauri build --target universal-apple-darwin ${config[@]+"${config[@]}"})

out="$target_dir/universal-apple-darwin/release/bundle"
app="$out/macos/Reperio.app"
echo
echo "Built $app (Apple silicon + Intel)"
ls "$out"/dmg/*.dmg 2>/dev/null | sed 's/^/Installer: /' || true

if [ "$install" = 0 ]; then
  echo "Drag Reperio.app to Applications, or run this again with --install to do it for you."
  exit 0
fi

# --install: put the app in Applications (or ~/Applications if that isn't writable).
dest="${INSTALL_DIR:-}"
if [ -z "$dest" ]; then
  if [ -w /Applications ]; then
    dest=/Applications
  else
    dest="$HOME/Applications"
    echo "/Applications isn't writable by you, so installing in $dest instead."
  fi
fi
mkdir -p "$dest"

# A copy that is still running keeps running the old code, so ask it to quit.
running() { pgrep -f "$dest/Reperio.app/Contents/MacOS/" >/dev/null 2>&1; }
if running; then
  echo "Quitting the Reperio that is running..."
  osascript -e 'tell application "Reperio" to quit' >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do running || break; sleep 1; done
fi

# Copy beside the old one first, so a failed copy never leaves you without an app.
staging="$dest/.Reperio.app.installing"
rm -rf "$staging"
ditto "$app" "$staging"
rm -rf "$dest/Reperio.app"
mv "$staging" "$dest/Reperio.app"
echo "Installed $dest/Reperio.app"
echo "The first time you open a card, macOS asks you to allow Reperio under"
echo "System Settings > Privacy & Security > Full Disk Access; Reperio shows the steps."
if [ -z "${NO_OPEN:-}" ]; then
  open "$dest/Reperio.app"
fi

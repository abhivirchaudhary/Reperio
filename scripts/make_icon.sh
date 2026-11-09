#!/bin/bash
# Renders the app icon from app/src-tauri/icons/icon.svg and writes every
# size the app bundles (PNGs, .icns for macOS, .ico for Windows).
#
#   scripts/make_icon.sh
#
# Needs Node with Playwright (and its Chromium) to draw the SVG, and
# Python with Pillow to write the sizes.
set -euo pipefail
icons=$(realpath "$(dirname "$0")/../app/src-tauri/icons")
NODE_PATH=${NODE_PATH:-$(npm root -g)} node - "$icons" <<'JS'
const { chromium } = require("playwright");
const fs = require("fs");
(async () => {
  const dir = process.argv[2];
  const svg = fs.readFileSync(`${dir}/icon.svg`, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1024, height: 1024 } });
  await page.setContent(`<html><body style="margin:0;background:transparent">${svg}</body></html>`);
  await page.locator("svg").screenshot({ path: `${dir}/icon.png`, omitBackground: true });
  await browser.close();
})();
JS
python3 - "$icons" <<'PY'
import os, sys
from PIL import Image
d = sys.argv[1]
icon = Image.open(os.path.join(d, "icon.png")).convert("RGBA")
for size, name in [(32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png")]:
    icon.resize((size, size), Image.LANCZOS).save(os.path.join(d, name))
icon.save(os.path.join(d, "icon.icns"))
icon.save(os.path.join(d, "icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
# The About section in Settings shows it too.
icon.resize((256, 256), Image.LANCZOS).save(os.path.join(d, "..", "..", "ui", "icon.png"))
PY
echo "icons written to $icons"

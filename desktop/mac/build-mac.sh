#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# build-mac.sh — TT Station for macOS (the desktop app, for a Mac that connects
# to a QuietBox over the network). Builds on Linux or macOS.
#
# The app is the same Electron shell the box runs; its screens come from the
# box itself (/app), so only the shell is packaged: electron/, assets/ and
# bonjour-service (to find boxes on the network).
#
#   desktop/mac/build-mac.sh            # Apple Silicon and Intel
#   desktop/mac/build-mac.sh arm64      # one architecture
#
# Output: desktop/mac/dist/TT-Station-<version>-mac-<arch>.zip
# Signing: ad hoc (rcodesign on Linux, codesign on macOS) — not notarized, so
# the first open needs right-click → Open (see README.md).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DESKTOP="$(dirname "$HERE")"
ARCHES=("${@:-arm64 x64}")
read -r -a ARCHES <<< "${ARCHES[*]}"
ELECTRON_VERSION="$(node -p "require('$DESKTOP/node_modules/electron/package.json').version")"
# TT Station's own version (the workspace's), so the Mac app and the box match.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$DESKTOP")/Cargo.toml" | head -1)"
STAGE="$HERE/.stage"
OUT="$HERE/dist"

echo "→ staging the app (Electron $ELECTRON_VERSION)"
rm -rf "$STAGE" && mkdir -p "$STAGE/electron" "$STAGE/assets" "$OUT"
cp "$DESKTOP"/electron/*.cjs "$DESKTOP"/electron/*.html "$STAGE/electron/"
cp "$DESKTOP"/assets/*.png "$STAGE/assets/"
cat > "$STAGE/package.json" <<EOF
{
  "name": "tt-station",
  "productName": "TT Station",
  "version": "$VERSION",
  "description": "TT Station for your QuietBox, on your Mac.",
  "license": "Apache-2.0",
  "main": "electron/main.cjs",
  "dependencies": { "bonjour-service": "1.3.0" }
}
EOF
(cd "$STAGE" && npm install --omit=dev --no-audit --no-fund --silent)

echo "→ icon"
python3 - "$DESKTOP/assets/icon.png" "$HERE/icon.icns" <<'EOF'
import sys
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGBA")
img.save(sys.argv[2], sizes=[(16, 16), (32, 32), (64, 64), (128, 128), (256, 256), (512, 512), (1024, 1024)])
EOF

for arch in "${ARCHES[@]}"; do
  echo "→ packaging for $arch"
  npx --yes @electron/packager@18.3.6 "$STAGE" "TT Station" \
    --platform=darwin --arch="$arch" --electron-version="$ELECTRON_VERSION" \
    --icon="$HERE/icon.icns" --app-bundle-id=com.tenstorrent.ttstation.desktop \
    --app-version="$VERSION" --app-category-type=public.app-category.developer-tools \
    --extend-info="$HERE/Info.extra.plist" --asar --overwrite --out="$HERE/.build" --quiet
  APP="$HERE/.build/TT Station-darwin-$arch/TT Station.app"

  echo "→ signing (ad hoc)"
  if command -v codesign >/dev/null; then
    codesign --force --deep --sign - "$APP"
  else
    rcodesign sign "$APP" >/dev/null 2>&1
  fi

  ZIP="$OUT/TT-Station-$VERSION-mac-$arch.zip"
  rm -f "$ZIP"
  # -y keeps the frameworks' symlinks as symlinks (the bundle breaks otherwise).
  (cd "$(dirname "$APP")" && zip -qry "$ZIP" "TT Station.app")
  echo "✓ $ZIP ($(du -h "$ZIP" | cut -f1))"
done

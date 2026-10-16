#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# build-mac.sh — TT Station for macOS (the desktop app, for a Mac that connects
# to a Tenstorrent box over the network). Builds on Linux or macOS.
#
# The app is the same Electron shell the box runs; its screens come from the
# box itself (/app), so only the shell is packaged: electron/, assets/ and
# bonjour-service (to find boxes on the network).
#
#   desktop/mac/build-mac.sh            # Apple Silicon and Intel
#   desktop/mac/build-mac.sh arm64      # one architecture
#
# Output: desktop/mac/dist/TT-Station-<version>-mac-<arch>.zip (the app, and
# its licenses in Licenses/ beside it and in the app's Contents/Resources/Licenses)
# Fuses: the app runs only its own app.asar (no ELECTRON_RUN_AS_NODE,
# NODE_OPTIONS or --inspect), with its cookies encrypted; flipped before signing.
# Signing: ad hoc (rcodesign on Linux, codesign on macOS) — not notarized, so
# the first open needs right-click → Open (see README.md).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DESKTOP="$(dirname "$HERE")"
REPO="$(dirname "$DESKTOP")"
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
  "description": "TT Station for your Tenstorrent box, on your Mac.",
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

  echo "→ licenses"
  # TT Station's license, notice and third-party list, and Electron's and Chromium's
  # (the packager leaves those beside the app; else this checkout's Electron's).
  LIC="$HERE/.build/Licenses-$arch"
  rm -rf "$LIC" && mkdir -p "$LIC"
  for f in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do cp "$REPO/$f" "$LIC/$f"; done
  ELECTRON_LIC="$(dirname "$APP")"
  [ -f "$ELECTRON_LIC/LICENSES.chromium.html" ] || ELECTRON_LIC="$DESKTOP/node_modules/electron/dist"
  cp "$ELECTRON_LIC/LICENSE" "$LIC/LICENSE.electron"
  cp "$ELECTRON_LIC/LICENSES.chromium.html" "$LIC/LICENSES.chromium.html"
  rm -rf "$APP/Contents/Resources/Licenses" && cp -R "$LIC" "$APP/Contents/Resources/Licenses"

  echo "→ fuses"
  # Before signing: flipping them changes the framework binary.
  npx --yes @electron/fuses@1.8.0 write --app "$APP" \
    RunAsNode=off EnableNodeCliInspectArguments=off EnableNodeOptionsEnvironmentVariable=off \
    EnableCookieEncryption=on OnlyLoadAppFromAsar=on >/dev/null

  echo "→ signing (ad hoc)"
  if command -v codesign >/dev/null; then
    codesign --force --deep --sign - "$APP"
  else
    rcodesign sign "$APP" >/dev/null 2>&1
  fi

  ZIP="$OUT/TT-Station-$VERSION-mac-$arch.zip"
  rm -f "$ZIP"
  # -y keeps the frameworks' symlinks as symlinks (the bundle breaks otherwise).
  rm -rf "$(dirname "$APP")/Licenses" && cp -R "$LIC" "$(dirname "$APP")/Licenses"
  (cd "$(dirname "$APP")" && zip -qry "$ZIP" "TT Station.app" Licenses)
  echo "✓ $ZIP ($(du -h "$ZIP" | cut -f1))"
done

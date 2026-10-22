#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# build-deb.sh — TT Station for Linux (the desktop app, for a computer that
# connects to a Tenstorrent box over the network), as a .deb for Ubuntu and Debian.
#
# Like the Mac app, only the Electron shell is packaged: its screens come from
# the box itself (/app). The box's own install is the separate `tt-station`
# package (../../build-deb.sh); this one is `tt-station-desktop`.
#
#   desktop/linux/build-deb.sh          # amd64, from desktop/node_modules
#
# Output: desktop/linux/dist/tt-station-desktop_<version>_amd64.deb
#
# The shell goes in as resources/app.asar, and the binary's Electron fuses are
# flipped so it runs only that app (no ELECTRON_RUN_AS_NODE, NODE_OPTIONS or
# --inspect), with its cookies encrypted.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DESKTOP="$(dirname "$HERE")"
REPO="$(dirname "$DESKTOP")"
ELECTRON_DIST="$DESKTOP/node_modules/electron/dist"
[ -f "$DESKTOP/node_modules/electron/install.js" ] || { echo "Run 'npm ci' in $DESKTOP first." >&2; exit 1; }
# Electron 44 doesn't fetch its binary at install time (only on first run), so a fresh
# `npm ci` has none yet: fetch it now.
[ -x "$ELECTRON_DIST/electron" ] || (cd "$DESKTOP" && node node_modules/electron/install.js)
[ -x "$ELECTRON_DIST/electron" ] || { echo "Couldn't get Electron's binary (see above)." >&2; exit 1; }
[ "$(uname -m)" = x86_64 ] || { echo "Builds amd64 from this machine's Electron; run it on x86_64." >&2; exit 1; }
ELECTRON_VERSION="$(cat "$ELECTRON_DIST/version")"
# TT Station's own version (the workspace's), so the app and the box match.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
PKG=tt-station-desktop
ROOT="$HERE/.stage/root"
OPT="$ROOT/opt/$PKG"
DOC="$ROOT/usr/share/doc/$PKG"
OUT="$HERE/dist"

echo "→ staging Electron $ELECTRON_VERSION"
rm -rf "$HERE/.stage" && mkdir -p "$OPT" "$OUT"
cp -a "$ELECTRON_DIST/." "$OPT/"
mv "$OPT/electron" "$OPT/$PKG"
rm -f "$OPT/resources/default_app.asar"
# Electron's own license goes by its name, beside TT Station's (below).
mv "$OPT/LICENSE" "$OPT/LICENSE.electron"

echo "→ the app"
APP="$HERE/.stage/app"
mkdir -p "$APP/electron" "$APP/assets"
cp "$DESKTOP"/electron/*.cjs "$DESKTOP"/electron/*.html "$APP/electron/"
cp "$DESKTOP"/assets/*.png "$APP/assets/"
# "name" is the window class the dock groups by (StartupWMClass below).
cat > "$APP/package.json" <<EOF
{
  "name": "tt-station",
  "productName": "TT Station",
  "version": "$VERSION",
  "description": "TT Station for your Tenstorrent box, on Linux.",
  "license": "Apache-2.0",
  "main": "electron/main.cjs",
  "dependencies": { "bonjour-service": "1.3.0" }
}
EOF
(cd "$APP" && npm install --omit=dev --no-audit --no-fund --silent)
npx --yes @electron/asar@3.2.17 pack "$APP" "$OPT/resources/app.asar"

echo "→ fuses"
npx --yes @electron/fuses@1.8.0 write --app "$OPT/$PKG" \
  RunAsNode=off EnableNodeCliInspectArguments=off EnableNodeOptionsEnvironmentVariable=off \
  EnableCookieEncryption=on OnlyLoadAppFromAsar=on >/dev/null

echo "→ licenses"
# TT Station's license, notice and third-party list, and Electron's and
# Chromium's: beside the app, and where Debian keeps a package's.
install -d "$DOC"
for f in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do
  install -m 644 "$REPO/$f" "$OPT/$f"
  install -m 644 "$REPO/$f" "$DOC/$f"
done
install -m 644 "$ELECTRON_DIST/LICENSE" "$DOC/LICENSE.electron"
install -m 644 "$ELECTRON_DIST/LICENSES.chromium.html" "$DOC/LICENSES.chromium.html"
cat > "$DOC/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: TT Station
Upstream-Contact: Tenstorrent <software@tenstorrent.com>

Files: *
Copyright: 2026 Tenstorrent USA, Inc.
License: Apache-2.0
 Licensed under the Apache License, Version 2.0: see
 /usr/share/common-licenses/Apache-2.0 and /usr/share/doc/$PKG/LICENSE.
 The npm packages bundled with the app (bonjour-service and its
 dependencies) are listed with their licenses in THIRD_PARTY_LICENSES.md.

Files: opt/$PKG/$PKG opt/$PKG/chrome* opt/$PKG/*.so* opt/$PKG/*.pak
 opt/$PKG/*.bin opt/$PKG/*.dat opt/$PKG/*.json opt/$PKG/locales/*
Copyright: Electron contributors; 2013-2020 GitHub Inc.
License: MIT
 Electron $ELECTRON_VERSION: see /usr/share/doc/$PKG/LICENSE.electron. It
 includes Chromium, Node.js and their dependencies under their own licenses,
 listed in /usr/share/doc/$PKG/LICENSES.chromium.html.
EOF

echo "→ launcher, icons, sandbox profile"
install -D -m 755 /dev/stdin "$ROOT/usr/bin/$PKG" <<EOF
#!/bin/sh
exec /opt/$PKG/$PKG --ozone-platform-hint=auto "\$@"
EOF
# Named tt-station.desktop: the app sets CHROME_DESKTOP to it, so the dock
# matches its windows to this launcher.
install -D -m 644 /dev/stdin "$ROOT/usr/share/applications/tt-station.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=TT Station
GenericName=Tenstorrent box control
Comment=Run, watch and connect everything on your Tenstorrent box
Exec=$PKG %U
Icon=tt-station
Terminal=false
Categories=Development;Utility;System;
Keywords=Tenstorrent;QuietBox;Galaxy;Wormhole;Blackhole;AI;LLM;
StartupWMClass=tt-station
StartupNotify=true
Actions=tori;

[Desktop Action tori]
Name=Tori
Exec=$PKG --tori
EOF
for n in 32 48 64 128 256 512; do
  install -D -m 644 "$DESKTOP/assets/icon-$n.png" "$ROOT/usr/share/icons/hicolor/${n}x${n}/apps/tt-station.png"
done
# Ubuntu 24.04+ lets only programs with an AppArmor profile create the user
# namespaces Chromium's sandbox needs; without one the app aborts at launch.
# Like Ubuntu's own profiles for VS Code and Chrome: everything, plus userns.
# It's written for AppArmor 4 (abi 4.0, userns), which AppArmor 3 (Ubuntu
# 22.04, Debian 12) may reject, and those don't need it: the package keeps it
# here and postinst copies it into /etc/apparmor.d only where abi 4.0 exists.
install -D -m 644 /dev/stdin "$ROOT/usr/share/$PKG/apparmor/$PKG" <<EOF
# TT Station desktop: allows everything; exists to grant the userns Chromium's sandbox needs.
abi <abi/4.0>,
include <tunables/global>

profile $PKG /opt/$PKG/$PKG flags=(unconfined) {
  userns,
  include if exists <local/$PKG>
}
EOF

echo "→ package metadata"
mkdir -p "$ROOT/DEBIAN"
SIZE="$(du -sk "$ROOT" | cut -f1)"
cat > "$ROOT/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: amd64
Maintainer: Tenstorrent <software@tenstorrent.com>
Installed-Size: $SIZE
Depends: libgtk-3-0t64 | libgtk-3-0, libnss3, libasound2t64 | libasound2, libgbm1, libnotify4, libsecret-1-0, libxkbfile1, xdg-utils, openssh-client
Recommends: netcat-openbsd
Section: utils
Priority: optional
Description: TT Station for your Tenstorrent box, on Linux
 Find a Tenstorrent box on your network (a QuietBox, a Galaxy server, a
 workstation with Tenstorrent cards: anything running TT Station), pair once,
 and use everything on it from this computer: chat, Code, Studio, Research,
 models, hardware and settings.
 The screens come from the box itself, so they always match its version.
EOF
# No conffiles: /etc/apparmor.d/$PKG is postinst's copy (only where AppArmor 4
# reads it), not a file dpkg installs; postrm deletes it on purge.
cat > "$ROOT/DEBIAN/postinst" <<EOF
#!/bin/sh
set -e
# Chromium's setuid sandbox helper, for kernels without unprivileged userns.
chown root:root /opt/$PKG/chrome-sandbox
chmod 4755 /opt/$PKG/chrome-sandbox
PROFILE=/usr/share/$PKG/apparmor/$PKG
if [ -e /etc/apparmor.d/abi/4.0 ]; then
  install -m 644 "\$PROFILE" /etc/apparmor.d/$PKG
  if command -v apparmor_parser >/dev/null && [ -d /sys/kernel/security/apparmor ]; then
    apparmor_parser -r /etc/apparmor.d/$PKG || true
  fi
elif [ -e /etc/apparmor.d/$PKG ] && cmp -s "\$PROFILE" /etc/apparmor.d/$PKG; then
  # An older package's copy, which this AppArmor can't read.
  rm -f /etc/apparmor.d/$PKG
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
EOF
cat > "$ROOT/DEBIAN/postrm" <<EOF
#!/bin/sh
set -e
if [ "\$1" = remove ] || [ "\$1" = purge ]; then
  if [ -e /etc/apparmor.d/$PKG ] && command -v apparmor_parser >/dev/null && [ -d /sys/kernel/security/apparmor ]; then
    apparmor_parser -R /etc/apparmor.d/$PKG 2>/dev/null || true
  fi
fi
if [ "\$1" = purge ]; then
  rm -f /etc/apparmor.d/$PKG
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
EOF
chmod 755 "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/postrm"

DEB="$OUT/${PKG}_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$ROOT" "$DEB" >/dev/null
echo "✓ $DEB ($(du -h "$DEB" | cut -f1))"

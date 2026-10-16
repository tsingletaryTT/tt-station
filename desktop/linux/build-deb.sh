#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# build-deb.sh — TT Station for Linux (the desktop app, for a computer that
# connects to a QuietBox over the network), as a .deb for Ubuntu and Debian.
#
# Like the Mac app, only the Electron shell is packaged: its screens come from
# the box itself (/app). The box's own install is the separate `tt-station`
# package (../../build-deb.sh); this one is `tt-station-desktop`.
#
#   desktop/linux/build-deb.sh          # amd64, from desktop/node_modules
#
# Output: desktop/linux/dist/tt-station-desktop_<version>_amd64.deb
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DESKTOP="$(dirname "$HERE")"
ELECTRON_DIST="$DESKTOP/node_modules/electron/dist"
[ -x "$ELECTRON_DIST/electron" ] || { echo "Run 'npm install' in $DESKTOP first." >&2; exit 1; }
[ "$(uname -m)" = x86_64 ] || { echo "Builds amd64 from this machine's Electron; run it on x86_64." >&2; exit 1; }
ELECTRON_VERSION="$(cat "$ELECTRON_DIST/version")"
# TT Station's own version (the workspace's), so the app and the box match.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$DESKTOP")/Cargo.toml" | head -1)"
PKG=tt-station-desktop
ROOT="$HERE/.stage/root"
OPT="$ROOT/opt/$PKG"
OUT="$HERE/dist"

echo "→ staging Electron $ELECTRON_VERSION"
rm -rf "$HERE/.stage" && mkdir -p "$OPT" "$OUT"
cp -a "$ELECTRON_DIST/." "$OPT/"
mv "$OPT/electron" "$OPT/$PKG"
rm -f "$OPT/resources/default_app.asar"

echo "→ the app"
APP="$OPT/resources/app"
mkdir -p "$APP/electron" "$APP/assets"
cp "$DESKTOP"/electron/*.cjs "$DESKTOP"/electron/*.html "$APP/electron/"
cp "$DESKTOP"/assets/*.png "$APP/assets/"
# "name" is the window class the dock groups by (StartupWMClass below).
cat > "$APP/package.json" <<EOF
{
  "name": "tt-station",
  "productName": "TT Station",
  "version": "$VERSION",
  "description": "TT Station for your QuietBox, on Linux.",
  "license": "Apache-2.0",
  "main": "electron/main.cjs",
  "dependencies": { "bonjour-service": "1.3.0" }
}
EOF
(cd "$APP" && npm install --omit=dev --no-audit --no-fund --silent)

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
GenericName=QuietBox control
Comment=Run, watch and connect everything on your QuietBox
Exec=$PKG %U
Icon=tt-station
Terminal=false
Categories=Development;Utility;System;
Keywords=Tenstorrent;QuietBox;AI;LLM;Blackhole;
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
install -D -m 644 /dev/stdin "$ROOT/etc/apparmor.d/$PKG" <<EOF
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
Maintainer: Tenstorrent <support@tenstorrent.com>
Installed-Size: $SIZE
Depends: libgtk-3-0t64 | libgtk-3-0, libnss3, libasound2t64 | libasound2, libgbm1, libnotify4, libsecret-1-0, libxkbfile1, xdg-utils, openssh-client
Recommends: netcat-openbsd
Section: utils
Priority: optional
Homepage: https://github.com/tsingletaryTT/tt-station
Description: TT Station for your QuietBox, on Linux
 Find a QuietBox on your network, pair once, and use everything on it from
 this computer: chat, Code, Studio, Research, models, hardware and settings.
 The screens come from the box itself, so they always match its version.
EOF
echo "/etc/apparmor.d/$PKG" > "$ROOT/DEBIAN/conffiles"
cat > "$ROOT/DEBIAN/postinst" <<EOF
#!/bin/sh
set -e
# Chromium's setuid sandbox helper, for kernels without unprivileged userns.
chown root:root /opt/$PKG/chrome-sandbox
chmod 4755 /opt/$PKG/chrome-sandbox
if command -v apparmor_parser >/dev/null && [ -d /sys/kernel/security/apparmor ]; then
  apparmor_parser -r /etc/apparmor.d/$PKG || true
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
EOF
cat > "$ROOT/DEBIAN/postrm" <<EOF
#!/bin/sh
set -e
if [ "\$1" = remove ] || [ "\$1" = purge ]; then
  if command -v apparmor_parser >/dev/null && [ -d /sys/kernel/security/apparmor ]; then
    apparmor_parser -R /etc/apparmor.d/$PKG 2>/dev/null || true
  fi
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
EOF
chmod 755 "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/postrm"

DEB="$OUT/${PKG}_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$ROOT" "$DEB" >/dev/null
echo "✓ $DEB ($(du -h "$DEB" | cut -f1))"

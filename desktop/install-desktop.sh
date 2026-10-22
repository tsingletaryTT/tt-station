#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# Install TT Station as a desktop app for this user: its icon, a launcher in
# the app grid (pin it to the dock from there), and nothing system-wide.
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
ELECTRON="$HERE/node_modules/electron/dist/electron"
[ -f "$HERE/node_modules/electron/install.js" ] || { echo "Run 'npm ci' in $HERE first." >&2; exit 1; }
# Electron 44 fetches its binary on first run, not at install time: fetch it now.
[ -x "$ELECTRON" ] || (cd "$HERE" && node node_modules/electron/install.js)
[ -x "$ELECTRON" ] || { echo "Couldn't get Electron's binary (see above)." >&2; exit 1; }
for n in 32 48 64 128 256 512; do
  install -D -m 644 "$HERE/assets/icon-$n.png" "$HOME/.local/share/icons/hicolor/${n}x${n}/apps/tt-station.png"
done
install -D -m 644 "$HERE/assets/icon.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/tt-station.png"
mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/tt-station.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=TT Station
GenericName=QuietBox control
Comment=Run, watch and connect everything on your QuietBox
Exec=env CHROME_DESKTOP=tt-station.desktop "$ELECTRON" "$HERE" --ozone-platform-hint=auto %U
Icon=tt-station
Terminal=false
Categories=Development;Utility;System;
Keywords=Tenstorrent;QuietBox;AI;LLM;Blackhole;
StartupWMClass=tt-station
StartupNotify=true
Actions=tori;

[Desktop Action tori]
Name=Tori
Exec=env CHROME_DESKTOP=tt-station.desktop "$ELECTRON" "$HERE" --ozone-platform-hint=auto --tori
DESKTOP
update-desktop-database -q "$HOME/.local/share/applications" 2>/dev/null || true

# Ubuntu 24.04+ only lets apps with an AppArmor profile create the user
# namespaces Chromium's sandbox needs; without one, TT Station aborts at launch
# ("SUID sandbox helper ... not configured correctly"). Like Ubuntu's profiles
# for VS Code and Chrome, this one allows everything plus userns.
if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = "1" ] && [ ! -f /etc/apparmor.d/tt-station ]; then
  echo "TT Station needs an AppArmor profile to start on this Ubuntu (asks for your password once)."
  sudo tee /etc/apparmor.d/tt-station >/dev/null <<PROFILE
# TT Station: allows everything; exists to grant the userns Chromium's sandbox needs.
abi <abi/4.0>,
include <tunables/global>

profile tt-station $ELECTRON flags=(unconfined) {
  userns,
  include if exists <local/tt-station>
}
PROFILE
  sudo apparmor_parser -r /etc/apparmor.d/tt-station
fi
gtk-update-icon-cache -q -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Installed. Find TT Station in your apps; right-click it in the dock to pin it."

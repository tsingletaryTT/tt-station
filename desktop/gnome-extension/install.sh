#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
# Install Tori's top-bar dropdown (a GNOME Shell extension). On Wayland,
# GNOME loads new extensions at the next login.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="$HOME/.local/share/gnome-shell/extensions/tori@tenstorrent.com"
mkdir -p "$(dirname "$DEST")"
ln -sfn "$HERE/tori@tenstorrent.com" "$DEST"
gnome-extensions enable tori@tenstorrent.com 2>/dev/null || \
  gsettings set org.gnome.shell enabled-extensions "$(gsettings get org.gnome.shell enabled-extensions | sed "s/]$/, 'tori@tenstorrent.com']/; s/\[, /[/")"
echo "Installed. Log out and back in once, then Tori lives in the top bar."

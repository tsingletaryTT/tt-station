#!/usr/bin/env bash
# Build TTStation (Release) and install it to ~/Applications (default) or
# /Applications (with --system, needs sudo). Run from anywhere.
#
#   macos/install.sh            # → ~/Applications/TTStation.app
#   macos/install.sh --system   # → /Applications/TTStation.app (sudo)
#
# The app is a MenuBarExtra (LSUIElement) — after install it appears in the
# menu bar, not the Dock. Version comes from AppShell/project.yml's
# MARKETING_VERSION (flowed into Info.plist via $(MARKETING_VERSION)).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
proj="$here/TTStation/AppShell"
dest_dir="$HOME/Applications"
use_sudo=""
if [[ "${1:-}" == "--system" ]]; then
  dest_dir="/Applications"
  use_sudo="sudo"
fi
dest="$dest_dir/TTStation.app"

echo "==> Generating project + building Release"
( cd "$proj" && xcodegen generate >/dev/null )
xcodebuild -project "$proj/TTStation.xcodeproj" -scheme TTStation \
  -configuration Release -destination 'platform=macOS' build >/dev/null

app="$(xcodebuild -project "$proj/TTStation.xcodeproj" -scheme TTStation \
  -configuration Release -showBuildSettings 2>/dev/null \
  | awk '/ BUILT_PRODUCTS_DIR /{d=$3} /FULL_PRODUCT_NAME/{p=$3} END{print d"/"p}')"
ver="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"

echo "==> Installing TTStation $ver → $dest"
osascript -e 'tell application "TTStation" to quit' 2>/dev/null || true
pkill -x TTStation 2>/dev/null || true
sleep 1
$use_sudo mkdir -p "$dest_dir"
$use_sudo rm -rf "$dest"
$use_sudo cp -R "$app" "$dest"
# Locally built, but clear any quarantine + ad-hoc re-sign so Gatekeeper
# never kills it on a fresh machine copy.
$use_sudo xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true
$use_sudo codesign --force --deep --sign - "$dest" >/dev/null 2>&1 || true

echo "==> Launching"
open "$dest"
echo "Done — TTStation $ver is in $dest_dir (look for the icon in the menu bar)."

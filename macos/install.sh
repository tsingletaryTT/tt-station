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

# Seed the tt-vscode-toolkit .vsix into the app's local cache so the Workbench's
# VS Code launcher installs the extension from a file (gallery-independent) instead
# of the marketplace ID — the ID silently no-ops when VS Code isn't pointed at a
# marketplace that carries it. Best-effort: skip on any failure (offline, no gh)
# so the build always proceeds; the app falls back to the marketplace ID.
vsix_cache="$HOME/Library/Application Support/TTStation/vsix"
seed_vsix() {
  local repo="tenstorrent/tt-vscode-toolkit"
  command -v gh >/dev/null 2>&1 || { echo "   (skip vsix: gh not found)"; return 0; }
  local tag
  tag="$(gh release view --repo "$repo" --json tagName --jq .tagName 2>/dev/null)" || {
    echo "   (skip vsix: couldn't reach GitHub releases)"; return 0; }
  # Already have this release's vsix? Don't re-download ~80 MB. The asset name
  # carries the bare version (tt-vscode-toolkit-0.0.518.vsix), while the tag is
  # v-prefixed (v0.0.518) — match on the version without the leading 'v'.
  local ver="${tag#v}"
  if ls "$vsix_cache/"*"$ver"*.vsix >/dev/null 2>&1; then
    echo "   vsix $tag already cached"; return 0
  fi
  mkdir -p "$vsix_cache"
  rm -f "$vsix_cache"/*.vsix 2>/dev/null || true   # keep only the latest
  echo "   downloading tt-vscode-toolkit $tag .vsix → $vsix_cache"
  gh release download "$tag" --repo "$repo" --pattern '*.vsix' --dir "$vsix_cache" --clobber 2>/dev/null \
    || echo "   (skip vsix: download failed)"
}
echo "==> Seeding tt-vscode-toolkit .vsix (best-effort)"
seed_vsix

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

#!/usr/bin/env bash
# Build a distributable, arm64 TTStation DMG with the `tt` CLI embedded inside
# the app bundle. Ad-hoc signed only (no Apple Developer license / no
# notarization) — downloaders must strip quarantine once; see FIRST-RUN.txt.
#
#   macos/make-release.sh              # build dist/TTStation-<ver>-arm64.dmg
#   macos/make-release.sh --publish    # also create/upload a GitHub Release (needs gh)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/.." && pwd)"
proj="$here/TTStation/AppShell"
dist="$here/dist"
stage="$dist/stage"
publish=""
[[ "${1:-}" == "--publish" ]] && publish="1"

ver="$(awk -F'"' '/MARKETING_VERSION:/{print $2; exit}' "$proj/project.yml")"
[[ -n "$ver" ]] || { echo "error: could not read MARKETING_VERSION"; exit 1; }
dmg="$dist/TTStation-$ver-arm64.dmg"
echo "==> Building TTStation $ver (arm64)"

# 1. Build the arm64 tt CLI.
echo "==> cargo build --release -p tt (aarch64-apple-darwin)"
( cd "$repo_root" && cargo build --release -p tt --target aarch64-apple-darwin )
tt_bin="$repo_root/target/aarch64-apple-darwin/release/tt"
[[ -x "$tt_bin" ]] || { echo "error: tt binary not found at $tt_bin"; exit 1; }

# 2. Build the Release app.
echo "==> xcodegen + xcodebuild (Release)"
( cd "$proj" && xcodegen generate >/dev/null )
xcodebuild -project "$proj/TTStation.xcodeproj" -scheme TTStation \
  -configuration Release -destination 'platform=macOS' \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=NO build >/dev/null
app_src="$(xcodebuild -project "$proj/TTStation.xcodeproj" -scheme TTStation \
  -configuration Release -showBuildSettings 2>/dev/null \
  | awk '/ BUILT_PRODUCTS_DIR /{d=$3} /FULL_PRODUCT_NAME/{p=$3} END{print d"/"p}')"
[[ -d "$app_src" ]] || { echo "error: built app not found ($app_src)"; exit 1; }

# 3. Stage a clean copy and embed tt.
echo "==> Embedding tt into app bundle + ad-hoc signing"
rm -rf "$stage" && mkdir -p "$stage"
cp -R "$app_src" "$stage/TTStation.app"
mkdir -p "$stage/TTStation.app/Contents/Resources/bin"
cp "$tt_bin" "$stage/TTStation.app/Contents/Resources/bin/tt"
chmod +x "$stage/TTStation.app/Contents/Resources/bin/tt"

# 4. Ad-hoc sign the whole bundle AFTER embedding so the nested tt is covered.
codesign --force --deep --sign - "$stage/TTStation.app"
codesign --verify --deep --strict "$stage/TTStation.app" || {
  echo "error: codesign verification failed"; exit 1; }

# Enforce the arm64-only constraint on the app executable and the embedded
# tt (catches a toolchain/target regression before it ships).
for macho in "$stage/TTStation.app/Contents/MacOS/TTStation" \
             "$stage/TTStation.app/Contents/Resources/bin/tt"; do
  archs="$(lipo -archs "$macho" 2>/dev/null || true)"
  [[ "$archs" == "arm64" ]] || { echo "error: $macho is not arm64-only (archs: ${archs:-none})"; exit 1; }
done

# 5. Assemble the DMG payload: app + /Applications alias + first-run note.
ln -sf /Applications "$stage/Applications"
cat > "$stage/FIRST-RUN.txt" <<EOF
TTStation $ver — first run
==========================

1. Drag TTStation.app onto the Applications folder in this window.

2. TTStation is signed ad-hoc (no Apple Developer certificate yet), so macOS
   quarantines it and may say "TTStation is damaged." Clear the quarantine
   once, in Terminal:

       xattr -dr com.apple.quarantine /Applications/TTStation.app

3. Launch TTStation from Applications. On first run it offers to add the \`tt\`
   command to ~/.local/bin. The app works whether or not you accept.

TTStation lives in the menu bar (no Dock icon). Look for its icon up top.
EOF

# 6. Build the compressed DMG.
echo "==> hdiutil create $dmg"
rm -f "$dmg"
hdiutil create -volname "TTStation $ver" -srcfolder "$stage" \
  -fs HFS+ -format UDZO "$dmg" >/dev/null
sha="$(shasum -a 256 "$dmg" | awk '{print $1}')"
echo "==> Done: $dmg"
echo "    sha256: $sha"

# 7. Optional publish.
if [[ -n "$publish" ]]; then
  command -v gh >/dev/null || { echo "error: gh not found for --publish"; exit 1; }
  tag="v$ver"
  # In CI a git tag triggered this run (GITHUB_REF=refs/tags/<tag>); make sure
  # it agrees with project.yml so we never publish to a different tag than the
  # one pushed. Manual dispatch / local runs just use the project version.
  if [[ "${GITHUB_REF:-}" == refs/tags/* ]]; then
    gitver="${GITHUB_REF#refs/tags/}"
    if [[ "$gitver" != "$tag" ]]; then
      echo "error: pushed tag $gitver disagrees with project.yml $tag — bump MARKETING_VERSION to match"; exit 1
    fi
  fi
  echo "==> Publishing GitHub Release $tag"
  notes="TTStation $ver (arm64). Ad-hoc signed — after installing run:

    xattr -dr com.apple.quarantine /Applications/TTStation.app

sha256: $sha"
  if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$dmg" --clobber
  else
    gh release create "$tag" "$dmg" --title "TTStation $ver" --notes "$notes"
  fi
fi

#!/usr/bin/env bash
# install-dev.sh — build, ad-hoc sign, install and activate the DEV Blackhole dext.
#
#   macos/TTStationDriver/scripts/install-dev.sh            # build + install + activate
#   macos/TTStationDriver/scripts/install-dev.sh --build    # build + sign only (no SIP needed)
#   macos/TTStationDriver/scripts/install-dev.sh --force    # try to activate even with SIP on
#
# An ad-hoc-signed dext only loads with System Integrity Protection disabled (tinygrad's
# TinyGPU install_nosip.sh enforces the same). Whether `systemextensionsctl developer on`
# alone is enough with SIP on is UNVERIFIED — --force lets you find out; expect activation
# error 4 ("missing entitlements") if it isn't. Re-enable SIP (`csrutil enable` from
# Recovery) when you are done experimenting.
#
# See docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md ("Dev install").
set -euo pipefail

MODE="install"
case "${1:-}" in
  --build) MODE="build" ;;
  --force) MODE="force" ;;
  "") ;;
  *) echo "usage: $0 [--build|--force]" >&2; exit 2 ;;
esac

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

APP="build/Debug/TTStationDriver.app"
DEXT="$APP/Contents/Library/SystemExtensions/com.tenstorrent.ttstation.driver.dext"

if [[ "$MODE" == "install" ]] && csrutil status 2>&1 | grep -q enabled; then
  cat >&2 <<'EOF'
╔══ SIP is enabled — an ad-hoc dext will not load.
║  To disable: shut down, hold the power button → Options → Terminal → `csrutil disable`, reboot.
║  Or try without disabling: `systemextensionsctl developer on`, then re-run with --force.
║  Build only (no install): re-run with --build.
╚══
EOF
  exit 1
fi

echo "── generate + build"
xcodegen generate --quiet
xcodebuild -project TTStationDriver.xcodeproj -alltargets -configuration Debug \
  SYMROOT="$HERE/build" build -quiet

echo "── ad-hoc sign (dev entitlements)"
# Inside-out: the dext first, then the app that embeds it (signing the app seals the dext).
codesign --sign - --force --entitlements Driver/TTBlackholeDriver.Dev.entitlements "$DEXT"
codesign --sign - --force --entitlements Host/TTStationDriver.entitlements "$APP"
codesign --sign - --force build/Debug/tt-station-bh-probe

if [[ "$MODE" == "build" ]]; then
  echo "built: $APP  +  build/Debug/tt-station-bh-probe"
  exit 0
fi

echo "── install to /Applications"
rm -rf /Applications/TTStationDriver.app
cp -R "$APP" /Applications/

echo "── activate"
set +e
/Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver install
rc=$?
set -e
case $rc in
  0) echo "activated — now run: build/Debug/tt-station-bh-probe" ;;
  4) echo "approve the extension in System Settings, then re-run this script" ;;
  *) echo "activation failed (exit $rc) — see: log show --last 5m --predicate 'eventMessage CONTAINS \"ttbh:\" OR subsystem == \"com.apple.sx\"'" ;;
esac
exit $rc

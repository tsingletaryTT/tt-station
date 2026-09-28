#!/usr/bin/env bash
# install-dev.sh: build, sign, install and activate the Blackhole dext.
#
#   scripts/install-dev.sh --team <TEAMID>   # development-signed via Xcode automatic signing.
#                                            #   The SIP-ON path. Needs a paid Apple Developer team
#                                            #   signed in to Xcode (Settings > Accounts).
#   scripts/install-dev.sh                   # ad-hoc signed. Only loads with SIP DISABLED.
#   scripts/install-dev.sh --build           # ad-hoc build + sign only, install nothing
#
# Why two paths: an ad-hoc dext needs SIP off (tinygrad's TinyGPU install_nosip.sh enforces the
# same), and `systemextensionsctl developer on` refuses to run with SIP on ("this tool cannot be
# used if System Integrity Protection is enabled", seen 2026-09-28). A development-signed dext
# from a paid team loads with SIP on, per Apple DTS (developer.apple.com/forums/thread/809202).
# Either way macOS asks for one approval click in System Settings. No reboot.
#
# See docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md ("Dev install").
set -euo pipefail

MODE="adhoc"; TEAM=""
case "${1:-}" in
  --team)  MODE="team"; TEAM="${2:?--team needs a Team ID (10 chars, Xcode > Settings > Accounts)}" ;;
  --build) MODE="build" ;;
  "") ;;
  *) echo "usage: $0 [--team TEAMID | --build]" >&2; exit 2 ;;
esac

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

APP="build/Debug/TTStationDriver.app"
DEXT="$APP/Contents/Library/SystemExtensions/com.tenstorrent.ttstation.driver.dext"

if [[ "$MODE" == "adhoc" ]] && csrutil status 2>&1 | grep -q enabled; then
  cat >&2 <<'EOF'
╔══ SIP is enabled, so an ad-hoc dext will not load.
║  SIP-on route: sign with a paid Apple Developer team → re-run with --team <TEAMID>
║  Build only:   re-run with --build
╚══
EOF
  exit 1
fi

echo "── generate"
xcodegen generate --quiet

if [[ "$MODE" == "team" ]]; then
  echo "── build (development-signed, team $TEAM)"
  # -allowProvisioningUpdates lets xcodebuild register this Mac and fetch or create the
  # development profiles (including the DriverKit development entitlements) for the team.
  xcodebuild -project TTStationDriver.xcodeproj -alltargets -configuration Debug \
    SYMROOT="$HERE/build" -allowProvisioningUpdates -quiet \
    CODE_SIGNING_ALLOWED=YES CODE_SIGN_STYLE=Automatic DEVELOPMENT_TEAM="$TEAM" \
    CODE_SIGN_IDENTITY="Apple Development" TTBH_ENTITLEMENTS_FLAVOR=Team \
    build
  codesign --verify --deep --strict "$APP" && echo "signature ok"
else
  echo "── build (unsigned)"
  xcodebuild -project TTStationDriver.xcodeproj -alltargets -configuration Debug \
    SYMROOT="$HERE/build" build -quiet
  echo "── ad-hoc sign (dev entitlements)"
  # Inside-out: the dext first, then the app that embeds it (signing the app seals the dext).
  codesign --sign - --force --entitlements Driver/TTBlackholeDriver.Dev.entitlements "$DEXT"
  codesign --sign - --force --entitlements Host/TTStationDriver.entitlements "$APP"
  codesign --sign - --force build/Debug/tt-station-bh-probe
fi

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
  0) echo "activated. Now run: /Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver probe" ;;
  4) echo "approve the extension in System Settings, then re-run: TTStationDriver status / probe" ;;
  *) echo "activation failed (exit $rc). See: log show --last 5m --predicate 'eventMessage CONTAINS \"ttbh:\" OR subsystem == \"com.apple.sx\"'" ;;
esac
exit $rc

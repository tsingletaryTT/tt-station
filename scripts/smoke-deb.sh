#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# smoke-deb.sh: install the built tt-station .deb in a clean container and
# check that the installed agent starts and serves.
#
#   scripts/smoke-deb.sh IMAGE DIR      # e.g. ubuntu:24.04 dist/
#
# DIR holds tt-station_*.deb. In a fresh IMAGE:
# installs tt-station with apt (so its Depends must resolve), checks the files
# the updater and systemd rely on, runs scripts/smoke-agent.sh against
# /usr/bin/tt-station-agentd with no flags (as the unit runs it), then purges the
# package. Needs docker.
set -euo pipefail

IMAGE="${1:?usage: $0 IMAGE DIR}"
DIR="$(cd "${2:?usage: $0 IMAGE DIR}" && pwd)"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

ls "$DIR"/tt-station_*.deb >/dev/null || { echo "no tt-station_*.deb in $DIR" >&2; exit 1; }

docker run --rm \
    -v "$DIR:/debs:ro" \
    -v "$ROOT/scripts/smoke-agent.sh:/smoke-agent.sh:ro" \
    -e DEBIAN_FRONTEND=noninteractive \
    "$IMAGE" bash -euo pipefail -c '
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends curl ca-certificates >/dev/null
        cp /debs/*.deb /tmp/
        deb=$(ls /tmp/tt-station_*.deb | head -1)
        echo "== installing $(basename "$deb")"
        apt-get install -y -qq --no-install-recommends "$deb"
        test -x /usr/bin/tt-station-agentd
        test -x /usr/bin/tt-station
        test -f /usr/lib/systemd/user/tt-station-agentd.service
        grep -q "^ExecStart=/usr/bin/tt-station-agentd" /usr/lib/systemd/user/tt-station-agentd.service
        test -f /usr/share/tt-station/app/index.html
        # In the package, not on disk: Ubuntu images skip /usr/share/doc, and debhelper gzips big docs.
        files=$(dpkg-deb -c "$deb")
        for f in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do
            grep -qE "/usr/share/doc/tt-station/$f(\.gz)?$" <<<"$files" || { echo "missing $f in the package"; exit 1; }
        done
        tt-station-agentd --help >/dev/null
        echo "== smoke-testing the installed agent"
        # No flags and no agentd.toml, exactly as the packaged unit starts it.
        SMOKE_APP=1 SMOKE_BARE=1 bash /smoke-agent.sh /usr/bin/tt-station-agentd
        echo "== purging"
        apt-get purge -y -qq tt-station >/dev/null
        test ! -e /usr/bin/tt-station-agentd
        echo "== package OK in '"$IMAGE"'"
    '

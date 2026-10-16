#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
#
# smoke-agent.sh: start a built tt-station-agentd and check that it serves.
#
#   scripts/smoke-agent.sh [AGENT_BINARY] [PORT]
#
# Defaults: target/release/tt-station-agentd, port 18765 (HTTPS on PORT+1).
# The agent runs with a throwaway HOME and config dir and with no-op stubs for
# the hardware and host tools it shells out to (tt-smi, docker, systemctl,
# systemd-run, tt), so nothing on the machine is touched. Passes when
# GET /healthz and GET /status both answer 200; fails at once, with the
# agent's output, if it exits first -- which is how a router panic at start
# (axum's "Overlapping method route") shows up. Used by CI's Rust job and by
# the packaging job against the installed .deb.
#
# SMOKE_BARE=1 starts the agent with no flags at all, the way the packaged
# systemd unit does (name from the hostname, port 8765), and ignores PORT.
# Only for a clean container, where 8765/8766 are free.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/release/tt-station-agentd}"
PORT="${2:-18765}"
ARGS=(--name smoke --ctrl-port "$PORT")
if [ "${SMOKE_BARE:-0}" = 1 ]; then
    PORT=8765
    ARGS=()
fi
WAIT_SECS="${SMOKE_WAIT_SECS:-60}"

[ -x "$BIN" ] || { echo "smoke: $BIN isn't an executable" >&2; exit 1; }
command -v curl >/dev/null || { echo "smoke: needs curl" >&2; exit 1; }

WORK="$(mktemp -d)"
PID=""
cleanup() {
    if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
        kill "$PID" 2>/dev/null || true
        for _ in $(seq 1 20); do kill -0 "$PID" 2>/dev/null || break; sleep 0.25; done
        kill -9 "$PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$WORK/stubs" "$WORK/home/.config/tt-station"
for cmd in tt-smi docker systemctl systemd-run tt; do
    printf '#!/bin/sh\nexit 0\n' > "$WORK/stubs/$cmd"
    chmod +x "$WORK/stubs/$cmd"
done

echo "smoke: starting $BIN ${ARGS[*]:-(no flags)} on port $PORT"
HOME="$WORK/home" TT_CONFIG_DIR="$WORK/home/.config/tt-station" \
    PATH="$WORK/stubs:/usr/local/bin:/usr/bin:/bin" \
    "$BIN" "${ARGS[@]}" > "$WORK/agent.log" 2>&1 &
PID=$!

fail() {
    echo "smoke: FAIL: $*" >&2
    echo "----- agent output -----" >&2
    cat "$WORK/agent.log" >&2
    exit 1
}

code() { curl -s -o /dev/null -m 3 -w '%{http_code}' "http://127.0.0.1:$PORT$1" || true; }

deadline=$((SECONDS + WAIT_SECS))
until [ "$(code /healthz)" = 200 ]; do
    kill -0 "$PID" 2>/dev/null || { wait "$PID" || rc=$?; fail "the agent exited (status ${rc:-0}) before answering"; }
    [ $SECONDS -lt $deadline ] || fail "GET /healthz didn't answer 200 within ${WAIT_SECS}s"
    sleep 0.5
done
echo "smoke: GET /healthz 200"
status="$(code /status)"
[ "$status" = 200 ] || fail "GET /status answered $status"
echo "smoke: GET /status 200"
# SMOKE_APP=1 (the .deb test): the packaged app must be served too.
if [ "${SMOKE_APP:-0}" = 1 ]; then
    app="$(code /app/)"
    [ "$app" = 200 ] || fail "GET /app/ answered $app (is the app in the package?)"
    echo "smoke: GET /app/ 200"
fi
kill -0 "$PID" 2>/dev/null || fail "the agent exited right after answering"
echo "smoke: OK"

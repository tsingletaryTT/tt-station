#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: © 2026 Tenstorrent AI ULC
#
# One config of the code-serving benchmark: reset both boards (leases kept),
# start the gpt-oss-20b package with a profile the way TT Station does,
# time the load (S7), run S1-S3 (bench.py) and S5 (agent_reliability),
# then stop the container and confirm it is gone.
#
#   run_config.sh <name> <profile>      e.g. run_config.sh A p150x2-throughput
set -u
NAME=$1
PROFILE=$2
DIR=$(cd "$(dirname "$0")" && pwd)
PKG=tt-hous/gpt-oss-20b-p150
LEASES="01d335 a4ecc4"
cd "$DIR"
log() { echo "[$(date -Is)] $*" | tee -a "run-$NAME.log"; }

log "=== config $NAME ($PROFILE)"
if [ "${FULL_RESET:-0}" = 1 ]; then
  # Whole-box reset (all 4 chips are under our leases); TT Station's launch() does the same,
  # because a start after per-board resets can fail in get_num_devices ("unordered_map::at").
  log "full reset: tt-smi -r"
  timeout 180 tt-smi -r >>"run-$NAME.log" 2>&1 || { log "tt-smi -r failed (exit $?)"; exit 2; }
  timeout 60 tt-smi -ls --local >>"run-$NAME.log" 2>&1
else
  for l in $LEASES; do
    log "gozer reset $l"
    gozer reset "$l" >>"run-$NAME.log" 2>&1 || { log "reset $l failed (exit $?)"; exit 2; }
  done
fi
gozer status >>"run-$NAME.log" 2>&1

T0=$(date +%s.%N)
log "start: tt-model serve $PKG --local-only --no-update-check --port 8003 --profile $PROFILE"
tt-model serve "$PKG" --local-only --no-update-check --port 8003 --profile "$PROFILE" >"serve-$NAME.log" 2>&1 &
SERVE_PID=$!
python3 "$DIR/bench.py" wait-ready --t0 "$T0" --timeout 5400 --out "load-$NAME.json" 2>&1 | tee -a "run-$NAME.log"
READY=${PIPESTATUS[0]}
if [ "$READY" = 0 ]; then
  timeout 300 tail --pid=$SERVE_PID -f /dev/null; log "tt-model serve watcher gone"
else
  kill $SERVE_PID 2>/dev/null; log "killed tt-model serve watcher"
fi
CNAME=$(docker ps --format '{{.Names}}' | grep "^tt-model-gpt-oss-20b-p150-" | head -1)
log "container: $CNAME"
if [ -n "$CNAME" ]; then
  docker inspect "$CNAME" | python3 -c '
import json,sys,re
d=json.load(sys.stdin)
for c in d:
  c["Config"]["Env"]=[(e.split("=",1)[0]+"=<redacted>") if re.search("TOKEN|KEY|SECRET",e.split("=",1)[0]) and not e.startswith("VLLM_HARMONY") else e for e in c["Config"]["Env"]]
json.dump(d,sys.stdout,indent=1)' >"inspect-$NAME.json"
fi

if [ "$READY" = 0 ]; then
  ACC0=$(docker logs "$CNAME" 2>&1 | grep -c '"POST /v1/')
  log "S1-S3"
  python3 "$DIR/bench.py" suite --config "$NAME" --out "results-$NAME.json" 2>&1 | tee -a "run-$NAME.log"
  ACC1=$(docker logs "$CNAME" 2>&1 | grep -c '"POST /v1/')
  log "server POSTs during suite: $((ACC1-ACC0))"
  log "S5"
  (cd /home/ttuser/code/tt-station && timeout 5400 cargo run --release --example agent_reliability -- \
      --url http://localhost:8003/v1 --histories 10 --tasks 5 --lengths 65536,98304 --seed 1 \
      --out "$DIR/s5-$NAME.json") >"s5-$NAME.log" 2>&1
  log "S5 exit $?"
  ACC2=$(docker logs "$CNAME" 2>&1 | grep -c '"POST /v1/')
  log "server POSTs during S5: $((ACC2-ACC1))"
else
  log "FAILED to become ready"
fi

[ -n "$CNAME" ] && docker logs "$CNAME" 2>&1 | grep -vE 'TOKEN=|KEY=|SECRET=' | tail -400 >"container-$NAME.log"
log "stop"
tt-model stop "$PKG" >>"run-$NAME.log" 2>&1
for i in $(seq 1 60); do
  [ -z "$(docker ps -q --filter label=org.tenstorrent.tt-model=gpt-oss-20b-p150)" ] && break
  sleep 3
done
docker ps --format '{{.Names}}' | grep tt-model- && log "WARNING: container still running"
curl -sS --max-time 3 http://127.0.0.1:8003/health >/dev/null 2>&1 && log "WARNING: port 8003 still answers"
gozer status >>"run-$NAME.log" 2>&1
log "=== done $NAME"

#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

# Upkeep for a QuietBox running TT Station. Nothing happens unless asked for:
#
#   sudo deploy/box-care.sh --docker-log-rotation
#       Cap Docker's container logs at 3 files of 50 MB each (json-file driver)
#       instead of letting a chatty model server fill the disk. Merges
#       "log-driver"/"log-opts" into /etc/docker/daemon.json, keeping every
#       other setting, with a backup beside it.
#
#       THIS RESTARTS DOCKER, AND SO EVERY RUNNING CONTAINER: the served model
#       stops (and has to load again), as do the voice, search and embedding
#       services. Run it when nobody is using the box. The cap applies to
#       containers created after the restart; one that already exists keeps
#       its old log settings until it's recreated (stopped and started again
#       from TT Station).
#
#   deploy/box-care.sh --docker-log-rotation --dry-run
#       Print the daemon.json it would write, change nothing.
#
# Safe to run again.
set -euo pipefail

DAEMON_JSON=${DAEMON_JSON:-/etc/docker/daemon.json}
rotate=0
dry=0
for arg in "$@"; do
  case "$arg" in
    --docker-log-rotation) rotate=1 ;;
    --dry-run) dry=1 ;;
    -h|--help) sed -n '5,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if [[ $rotate -eq 0 ]]; then
  echo "Nothing to do. Choose a step, e.g. --docker-log-rotation (see --help)." >&2
  exit 2
fi

# daemon.json with json-file rotation merged in; everything else kept as is.
merged() {
  python3 - "$DAEMON_JSON" <<'PY'
import json, os, sys
path = sys.argv[1]
cfg = {}
if os.path.exists(path) and os.path.getsize(path) > 0:
    with open(path) as f:
        cfg = json.load(f)
driver = cfg.get("log-driver", "json-file")
if driver not in ("json-file", "local"):
    sys.exit(f"{path} sets log-driver {driver!r}; not changing it.")
cfg["log-driver"] = driver
opts = cfg.setdefault("log-opts", {})
opts["max-size"] = "50m"
opts["max-file"] = "3"
print(json.dumps(cfg, indent=2))
PY
}

new=$(merged)
if [[ $dry -eq 1 ]]; then
  echo "$new"
  exit 0
fi

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo: sudo $0 $*" >&2
  exit 1
fi

if [[ -f "$DAEMON_JSON" ]] && [[ "$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])), indent=2))' "$DAEMON_JSON")" == "$new" ]]; then
  echo "Docker log rotation is already set (50 MB x 3). Nothing changed."
  exit 0
fi

echo "This restarts Docker: every running container stops, including the served model."
read -r -p "Continue? [y/N] " ok
[[ "$ok" == [yY]* ]] || { echo "Nothing changed."; exit 0; }

mkdir -p "$(dirname "$DAEMON_JSON")"
if [[ -f "$DAEMON_JSON" ]]; then
  cp -p "$DAEMON_JSON" "$DAEMON_JSON.bak-$(date +%s)"
fi
tmp=$(mktemp "$DAEMON_JSON.XXXXXX")
printf '%s\n' "$new" > "$tmp"
chmod 644 "$tmp"
mv "$tmp" "$DAEMON_JSON"
systemctl restart docker
echo "Done. New containers keep at most 3 x 50 MB of logs; existing ones change when recreated."

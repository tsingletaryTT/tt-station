#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

# Containment for bring-up runs that needs root (docs/reference/bringup-containment.md).
# TT Station already runs each bring-up stage in a Landlock sandbox with the
# systemd properties a user manager can apply; this script adds what only root
# can. Run with sudo. Safe to run again. Each part is opt-in:
#
#   sudo deploy/contain-bringup.sh --userns
#       Lets the systemd user manager set up mount namespaces (Ubuntu's AppArmor
#       blocks them by default), so ProtectSystem=/InaccessiblePaths=/
#       TemporaryFileSystem= etc. apply to bring-up units as a second layer
#       under Landlock. TT Station notices on its next start; nothing else
#       to do.
#
#   sudo deploy/contain-bringup.sh --account [--box-user NAME]
#       A separate account for runs, `tt-bringup`, with no docker group, no
#       sudo and no read access to the box user's home except what a run needs
#       (the tt-metal checkout and the Hugging Face cache, by ACL); a root-owned
#       launcher that starts a run as a *system* unit under that account (where
#       every sandbox property works, IPAddressDeny= and DeviceAllow= too);
#       and outbound traffic from that account only through an allowlisting
#       proxy. TT Station uses the launcher once it's installed and
#       TT_STATION_BRINGUP_AS=tt-bringup is set in its environment (see the
#       reference doc; the agent side of this is the follow-up). Sign the
#       engines in as that account once: `sudo -u tt-bringup -H codex login`,
#       `sudo -u tt-bringup -H claude auth login`.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo: sudo $0 --userns | --account" >&2
  exit 1
fi

USERNS=0 ACCOUNT=0
BOX_USER="${SUDO_USER:-}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --userns) USERNS=1 ;;
    --account) ACCOUNT=1 ;;
    --box-user) BOX_USER="$2"; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
if [[ $USERNS == 0 && $ACCOUNT == 0 ]]; then
  echo "Say what to set up: --userns, --account, or both." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# --userns: mount namespaces for systemd user units
# ---------------------------------------------------------------------------
# kernel.apparmor_restrict_unprivileged_userns=1 puts a process that creates a
# user namespace under the `unprivileged_userns` profile, which denies it
# CAP_SYS_ADMIN, so systemd-executor can't mount anything in it (and in user
# mode it skips the properties silently). This profile lets systemd-executor,
# and only it, keep its capabilities in the namespace it makes; once it execs
# the unit's program, that program is confined as before. The sysctl stays on.
if [[ $USERNS == 1 ]]; then
  cat > /etc/apparmor.d/tt-station-systemd-executor <<'EOF'
# TT Station: let systemd user units use mount namespaces (PrivateUsers= with
# ProtectSystem=, InaccessiblePaths=, TemporaryFileSystem=...), which Ubuntu's
# unprivileged-userns restriction otherwise makes systemd skip silently.
abi <abi/4.0>,
include <tunables/global>

profile tt-station-systemd-executor /usr/lib/systemd/systemd-executor flags=(unconfined) {
  userns,
  include if exists <local/tt-station-systemd-executor>
}
EOF
  apparmor_parser -r /etc/apparmor.d/tt-station-systemd-executor
  echo "Mount namespaces for user units: on. Restart TT Station (systemctl --user restart tt-station-agentd) to use them."
fi

# ---------------------------------------------------------------------------
# --account: a separate account, a launcher, and egress through a proxy
# ---------------------------------------------------------------------------
if [[ $ACCOUNT == 1 ]]; then
  if [[ -z "$BOX_USER" || "$BOX_USER" == root ]]; then
    echo "Say which account TT Station runs as: --box-user NAME" >&2
    exit 2
  fi
  BOX_HOME=$(getent passwd "$BOX_USER" | cut -d: -f6)
  RUNS=/var/lib/tt-bringup

  # The account: no shell login, no extra groups (no docker: the docker socket is root).
  if ! id tt-bringup >/dev/null 2>&1; then
    useradd --system --create-home --home-dir "$RUNS" --shell /usr/sbin/nologin tt-bringup
  fi
  install -d -o tt-bringup -g tt-bringup -m 0750 "$RUNS" "$RUNS/runs"
  # The box user reads a run's logs and writes its notes.
  setfacl -m "u:$BOX_USER:rwx" "$RUNS" "$RUNS/runs"
  setfacl -d -m "u:$BOX_USER:rwX" "$RUNS/runs"

  # What a run works on, shared by ACL: the tt-metal checkout and the Hugging
  # Face cache. Home is traversable (x), not listable; nothing else is shared.
  setfacl -m u:tt-bringup:x "$BOX_HOME" "$BOX_HOME/.cache"
  for d in "$BOX_HOME/tt-metal" "$BOX_HOME/.cache/huggingface"; do
    [[ -d "$d" ]] || continue
    setfacl -R -m u:tt-bringup:rwX "$d"
    setfacl -R -d -m u:tt-bringup:rwX "$d"
    setfacl -R -d -m "u:$BOX_USER:rwX" "$d"
  done

  # The chips: /dev/tenstorrent/* is 0666 from the driver's udev rule; nothing to add.

  # Where the launcher finds what to bind into a run (root-owned).
  install -d -m 0755 /etc/tt-station
  printf 'BOX_HOME=%q\n' "$BOX_HOME" > /etc/tt-station/bringup.conf

  # The launcher. TT Station (as the box user) may run it through sudo and
  # nothing else as root; it only ever starts a unit as tt-bringup.
  install -d -m 0755 /usr/local/libexec/tt-station
  cat > /usr/local/libexec/tt-station/bringup-run <<'EOF'
#!/usr/bin/env bash
# Start, stop or ask after a TT Station bring-up unit, as the tt-bringup account.
#   bringup-run start UNIT RUN_DIR -- COMMAND...
#   bringup-run stop UNIT | is-active UNIT
set -euo pipefail
unit="${2:-}"
[[ "$unit" =~ ^tt-(codex|claude)-bringup-[A-Za-z0-9]{1,32}$ ]] || { echo "bad unit name" >&2; exit 2; }
case "${1:-}" in
  stop) exec systemctl stop "$unit" ;;
  is-active) exec systemctl is-active --quiet "$unit" ;;
  start) ;;
  *) echo "usage: bringup-run start|stop|is-active UNIT ..." >&2; exit 2 ;;
esac
run_dir=$(realpath -m "${3:-}")
[[ "$run_dir" == /var/lib/tt-bringup/runs/* ]] || { echo "run folder must be under /var/lib/tt-bringup/runs" >&2; exit 2; }
[[ "${4:-}" == "--" ]] || { echo "usage: bringup-run start UNIT RUN_DIR -- COMMAND..." >&2; exit 2; }
shift 4
install -d -o tt-bringup -g tt-bringup -m 0770 "$run_dir"
# shellcheck source=/dev/null
. /etc/tt-station/bringup.conf
exec systemd-run --collect --quiet --unit="$unit" \
  --uid=tt-bringup --gid=tt-bringup \
  --property=WorkingDirectory="$run_dir" \
  --property=StandardOutput=append:"$run_dir/runner.log" \
  --property=StandardError=append:"$run_dir/runner.log" \
  --setenv=HOME=/var/lib/tt-bringup \
  --setenv=HTTPS_PROXY=http://127.0.0.1:8899 --setenv=HTTP_PROXY=http://127.0.0.1:8899 \
  --setenv=NO_PROXY=127.0.0.1,localhost \
  --property=NoNewPrivileges=yes --property=PrivateTmp=yes \
  --property=DevicePolicy=closed --property=DeviceAllow="char-tenstorrent rw" \
  --property=DeviceAllow="/dev/null rw" --property=DeviceAllow="/dev/zero rw" \
  --property=DeviceAllow="/dev/urandom r" --property=DeviceAllow="/dev/random r" \
  --property=DeviceAllow="/dev/tty rw" \
  --property=ProtectSystem=strict --property=ReadWritePaths=/var/lib/tt-bringup \
  --property=ProtectHome=tmpfs \
  --property=BindPaths=-"$BOX_HOME/tt-metal" --property=BindPaths=-"$BOX_HOME/.cache/huggingface" \
  --property=ProtectKernelTunables=yes --property=ProtectKernelModules=yes \
  --property=ProtectKernelLogs=yes --property=ProtectControlGroups=yes --property=ProtectClock=yes \
  --property=ProtectProc=invisible --property=RestrictSUIDSGID=yes --property=RestrictNamespaces=yes \
  --property=LockPersonality=yes --property=RestrictRealtime=yes --property=KeyringMode=private \
  --property="SystemCallFilter=~@mount @swap @reboot @module @raw-io @clock @cpu-emulation @obsolete" \
  --property="RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK" \
  --property=IPAddressDeny=any --property=IPAddressAllow=localhost \
  -- "$@"
EOF
  chmod 0755 /usr/local/libexec/tt-station/bringup-run
  # ProtectHome=tmpfs hides every home; only the checkout and the Hugging
  # Face cache come back in (BindPaths=, from /etc/tt-station/bringup.conf).
  cat > /etc/sudoers.d/tt-station-bringup <<EOF
# TT Station starts bring-up runs as tt-bringup through this launcher only.
$BOX_USER ALL=(root) NOPASSWD: /usr/local/libexec/tt-station/bringup-run
EOF
  chmod 0440 /etc/sudoers.d/tt-station-bringup
  visudo -cf /etc/sudoers.d/tt-station-bringup >/dev/null

  # Egress: the unit itself may only reach localhost (IPAddressAllow=); the
  # proxy, running as its own account, reaches the allowlisted hosts.
  DEBIAN_FRONTEND=noninteractive apt-get install -y -q tinyproxy >/dev/null
  cat > /etc/tinyproxy/tt-bringup-allow <<'EOF'
^huggingface\.co$
\.huggingface\.co$
\.hf\.co$
^github\.com$
^codeload\.github\.com$
^objects\.githubusercontent\.com$
^raw\.githubusercontent\.com$
^pypi\.org$
^files\.pythonhosted\.org$
^download\.pytorch\.org$
^docs\.tenstorrent\.com$
^docs\.vllm\.ai$
^arxiv\.org$
^api\.openai\.com$
^chatgpt\.com$
^auth\.openai\.com$
^api\.anthropic\.com$
^console\.anthropic\.com$
^claude\.ai$
EOF
  cat > /etc/tinyproxy/tinyproxy.conf <<'EOF'
# TT Station: the only way out for bring-up runs (tt-bringup).
User tinyproxy
Group tinyproxy
Port 8899
Listen 127.0.0.1
Timeout 600
MaxClients 64
Allow 127.0.0.1
ConnectPort 443
Filter "/etc/tinyproxy/tt-bringup-allow"
FilterDefaultDeny Yes
FilterExtended On
LogLevel Notice
EOF
  systemctl enable --now tinyproxy >/dev/null
  systemctl restart tinyproxy

  echo "tt-bringup is ready. Sign the engines in as it once:"
  echo "  sudo -u tt-bringup -H codex login"
  echo "  sudo -u tt-bringup -H claude auth login"
  echo "Then set TT_STATION_BRINGUP_AS=tt-bringup for TT Station (agentd.toml env) and restart it."
fi

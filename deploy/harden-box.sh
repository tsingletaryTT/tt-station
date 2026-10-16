#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

# Network hardening for a QuietBox running TT Station (docs/reference/box-hardening.md):
# SSH by key only, a firewall that lets in only what paired devices need, and a
# lockout for repeated failed sign-ins. Run with sudo. Safe to run again.
#
# Before running it, make sure every computer that signs in over SSH has a key
# on the box (pairing installs one), or sign in at the box itself: password
# sign-in over SSH stops working.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run with sudo: sudo $0" >&2
  exit 1
fi

# SSH: keys only, no root, fewer guesses. Earlier files in sshd_config.d win.
cat > /etc/ssh/sshd_config.d/01-tt-station.conf <<'EOF'
# TT Station hardening. Sign-in is by SSH key only (paired computers get one
# when they pair; new ones pair with a code shown on the box), so password
# guessing from the network has nothing to try. No direct root sign-in.
# Earlier files in this folder win, hence the 01- prefix.
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
MaxAuthTries 3
LoginGraceTime 30
EOF
sshd -t
systemctl reload ssh

# Firewall: SSH, TT Station's HTTPS port and mDNS discovery from the network;
# everything from the box's own Docker networks (containers reach the agent
# and model through the bridge). Docker-published ports bypass ufw, so
# services meant for this computer only are published on 127.0.0.1 instead.
ufw --force reset >/dev/null
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
ufw allow 22/tcp comment 'SSH (keys only)' >/dev/null
ufw allow 8766/tcp comment 'TT Station HTTPS for paired devices' >/dev/null
# Phones and computers find the box over mDNS, then connect to its advertised
# port (8765) to resolve it; the agent itself refuses plain HTTP from other
# machines, so this lets discovery work without exposing anything.
ufw allow 8765/tcp comment 'TT Station discovery (agent refuses plain HTTP from other machines)' >/dev/null
ufw allow 5353/udp comment 'mDNS: find the box on the network' >/dev/null
for net in $(docker network ls -q | xargs docker network inspect --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}' | tr ' ' '\n' | grep -E '^[0-9.]+/[0-9]+$' | sort -u); do
  ufw allow from "$net" comment 'Docker networks on this box' >/dev/null
done
ufw --force enable >/dev/null

# Lockout: an hour's ban after 5 failed sign-ins in 10 minutes. Ubuntu runs SSH
# as ssh.service, and "aggressive" counts key refusals, the only kind left.
DEBIAN_FRONTEND=noninteractive apt-get install -y -q fail2ban >/dev/null
cat > /etc/fail2ban/jail.d/tt-station.local <<'EOF'
# TT Station: ban an address for an hour after 5 failed SSH sign-ins in 10 minutes.
# Ubuntu runs SSH as ssh.service (the stock match expects sshd.service), and
# "aggressive" also counts key-only refusals, the only kind left with passwords off.
[sshd]
enabled = true
backend = systemd
journalmatch = _SYSTEMD_UNIT=ssh.service + _COMM=sshd
mode = aggressive
maxretry = 5
findtime = 10m
bantime = 1h
ignoreip = 127.0.0.1/8 ::1
EOF
systemctl enable --now fail2ban >/dev/null
systemctl restart fail2ban

echo "Done. SSH is key-only, the firewall is on, and repeated failed sign-ins are banned for an hour."
ufw status

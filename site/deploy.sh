#!/usr/bin/env bash
#
# deploy.sh — publish the tt-station landing page to the TT internal web space.
#
#   Live (behind TT Microsoft SSO):
#   https://users.local.tenstorrent.com/~tsingletary/tt-station/
#
# Requires working SSH *key* auth to users.local.tenstorrent.com (no password).
# Test with:  ssh -o BatchMode=yes tsingletary@users.local.tenstorrent.com true
# If that prompts for a password, fall back to ~/code/tt-home/tt_upload.py
# (interactive, one file at a time).
#
# What it does, in one SSH connection:
#   1. tars the whole site/ dir (minus .DS_Store and this script)
#   2. extracts it into ~/public_html/tt-station on the remote
#   3. sets Apache UserDir perms: home 711, dirs 755, files 644
#      (scp/tar would otherwise land files 600, which Apache can't read)
#
# Usage:  ./site/deploy.sh
#
set -euo pipefail

HOST="tsingletary@users.local.tenstorrent.com"
REMOTE="public_html/tt-station"
SITE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Keep macOS AppleDouble (._*) resource forks out of the tarball.
export COPYFILE_DISABLE=1

echo "Deploying $SITE_DIR → $HOST:~/$REMOTE"
tar czf - -C "$SITE_DIR" --exclude='.DS_Store' --exclude='deploy.sh' . \
  | ssh -o StrictHostKeyChecking=accept-new "$HOST" "
      set -e
      mkdir -p ~/$REMOTE
      tar xzf - -C ~/$REMOTE
      chmod 711 ~ && chmod 755 ~/public_html
      find ~/$REMOTE -type d -exec chmod 755 {} +
      find ~/$REMOTE -type f -exec chmod 644 {} +
    "

echo 'Done. Live (behind TT SSO):'
echo '  https://users.local.tenstorrent.com/~tsingletary/tt-station/'

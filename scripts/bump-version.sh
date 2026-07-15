#!/bin/bash
# bump-version.sh — set the tt-station version in lockstep across all files
# that hard-code it: the workspace Cargo.toml, debian/changelog, and the panel.
#
# Usage: scripts/bump-version.sh 0.10.0
# Edits only — does not git-commit. Run cargo build afterwards to refresh Cargo.lock.
set -euo pipefail
NEW="${1:?usage: bump-version.sh <version>}"
SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR"

# In-place edits use `perl -i` rather than `sed -i`: `sed -i` is NOT portable
# across GNU and BSD/macOS sed (BSD's `-i` requires a backup-suffix argument, so
# `sed -i -E …` silently eats `-E` as the suffix — dropping extended-regex mode
# and leaving stray `*-E` files), and the `0,/re/` "first match" address below is
# a GNU-only extension BSD sed rejects outright. `perl -i` behaves identically on
# both. NEW is passed via the environment so it can't break the perl program text.

# Workspace version: the [workspace.package] `version = "…"` line, which is the
# first start-of-line `version = "x.y.z"` in the file (dependency versions are
# indented/inline, never at column 0). The `$done` guard replaces only that first
# match, mirroring the previous `0,/…/` intent.
NEW="$NEW" perl -i -pe 'if (!$done && s/^version = "\d+\.\d+\.\d+"/version = "$ENV{NEW}"/) { $done = 1 }' Cargo.toml

# Panel __version__.
NEW="$NEW" perl -i -pe 's/^__version__ = "\d+\.\d+\.\d+"/__version__ = "$ENV{NEW}"/' box-panel/tt-station-panel.py

# debian/changelog: rewrite the first entry's version token in-place. We prepend
# a fresh stanza so the changelog keeps history.
DATE="$(date -R)"
TMP="$(mktemp)"
{
  echo "tt-station ($NEW) noble; urgency=medium"
  echo ""
  echo "  * Release $NEW."
  echo ""
  echo " -- Tenstorrent <software@tenstorrent.com>  $DATE"
  echo ""
  cat debian/changelog
} > "$TMP"
mv "$TMP" debian/changelog

echo "Bumped to $NEW. Run 'cargo build' to update Cargo.lock, then commit."

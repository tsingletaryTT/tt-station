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

# Workspace version (the [workspace.package] version = "…" line).
sed -i -E "0,/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/s//version = \"$NEW\"/" Cargo.toml

# Panel __version__.
sed -i -E "s/^__version__ = \"[0-9]+\.[0-9]+\.[0-9]+\"/__version__ = \"$NEW\"/" box-panel/tt-station-panel.py

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

#!/usr/bin/env bash
# ensure-official-tt.sh — make sure Tenstorrent's OFFICIAL `tt` CLI (tenstorrent/tt-cli, PyPI
# package `tenstorrent`) is installed and is the `tt` on PATH.
#
#   macos/scripts/ensure-official-tt.sh           # install / repair
#   macos/scripts/ensure-official-tt.sh --check   # report only, change nothing
#
# Why tt-station cares: `tt-station local` treats this Mac as its own host and delegates
# "which models are right-sized for the attached card?" to `tt model list --hw <config>`.
# That only works if `tt` IS the official CLI.
#
# Called from two places, so the logic lives once:
#   * macos/install.sh (developer install), and
#   * TTStation.app's first run (bundled at Contents/Resources/scripts/, see CLIInstaller.swift).
#
# Exit codes (stable; the app reads them):
#   0  official tt present (version on the last stdout line: "official tt <version>")
#   3  a FOREIGN `tt` shadows it; left untouched, see the message
#   4  uv missing and couldn't be installed (no Homebrew)
#   5  install ran but `tt` still isn't the official CLI (PATH order?)
#   6  --check: official tt not installed (nothing changed)
set -euo pipefail

CHECK=0
[[ "${1:-}" == "--check" ]] && CHECK=1

# GUI apps get a minimal PATH; add the places uv, Homebrew and uv's tool shims live.
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

# The official CLI answers `--version` with "tt <semver>". tt-station's own pre-rename CLI (which
# used to be installed as `tt`) has no --version at all.
official_version() {
  local out
  out="$("$1" --version 2>/dev/null | head -1)" || return 1
  [[ "$out" =~ ^tt\ [0-9] ]] || return 1
  echo "${out#tt }"
}

# Is `$1` a stale copy/link of tt-station's OWN old CLI? Only then may we remove it.
# Capture first, then match: under `set -o pipefail` a `--help | grep -q` pipeline is sunk by the
# binary's own exit status (or grep's early-close SIGPIPE), misfiling a stale copy as foreign.
is_stale_tt_station() {
  local out
  out="$("$1" --help 2>&1 || true)"
  [[ "$out" == *"Operator CLI for tt-station"* ]]
}

current="$(command -v tt || true)"
if [[ -n "$current" ]] && ver="$(official_version "$current")"; then
  echo "official tt $ver"
  exit 0
fi

if [[ -n "$current" ]]; then
  if is_stale_tt_station "$current"; then
    # The 2026-08 rename (tt -> tt-station) stopped CREATING this collision but never cleaned up
    # an existing one. It is our own binary, so removing it is safe and is the fix.
    if (( CHECK )); then
      echo "stale tt-station CLI shadows tt at $current (would remove)"
    else
      echo "removing stale pre-rename tt-station CLI at $current (it shadowed the official tt)"
      rm -f "$current"
    fi
  else
    echo "a foreign \`tt\` is on PATH at $current and is not the official Tenstorrent CLI."
    echo "Not touching it. Check \`which -a tt\`, remove or rename it, then re-run."
    exit 3
  fi
fi

if (( CHECK )); then
  echo "official tt not installed (install: uv tool install tenstorrent)"
  exit 6
fi

if ! command -v uv >/dev/null 2>&1; then
  if command -v brew >/dev/null 2>&1; then
    echo "installing uv via Homebrew"
    brew install uv
  else
    echo "uv is required: https://docs.astral.sh/uv/getting-started/installation/ — then re-run."
    exit 4
  fi
fi

# `uv tool install` is idempotent-ish: an existing install makes it a no-op with a notice.
echo "installing the official Tenstorrent CLI: uv tool install tenstorrent"
uv tool install tenstorrent

hash -r
current="$(command -v tt || true)"
if [[ -n "$current" ]] && ver="$(official_version "$current")"; then
  echo "official tt $ver"
  exit 0
fi
echo "installed, but \`tt\` on PATH (${current:-none}) is still not the official CLI."
echo "Is ~/.local/bin on your PATH ahead of other tt's? Try: uv tool update-shell"
exit 5

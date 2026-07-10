# Linux .deb Packaging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the tt-station Linux side as two Ubuntu `.deb` packages (`tt-station` = `tt` CLI + `tt-station-agentd` + systemd user unit; `tt-station-panel` = the GTK4 on-box GUI), built like tt-toplike.

**Architecture:** A new `debian/` tree drives `debhelper` (compat 13) via `dpkg-buildpackage`, orchestrated by a repo-local `build-deb.sh` that vendors crates for offline builds. Two binary packages are produced from one source. A `release.yml` builds both per Ubuntu suite (noble/jammy) on `v*` tags and publishes to GitHub Releases. Version is unified across the workspace `Cargo.toml`, `debian/changelog`, and the panel via `scripts/bump-version.sh`.

**Tech Stack:** Rust (cargo workspace), debhelper/dpkg-buildpackage, bash, GitHub Actions, Python (panel packaging bits).

## Global Constraints

- Maintainer string everywhere: `Tenstorrent <software@tenstorrent.com>`.
- License: `Apache-2.0`. Copyright: `2026 Tenstorrent Inc.`.
- `Architecture: amd64`; target suites noble (24.04) + jammy (22.04).
- `debhelper-compat (= 13)` declared in Build-Depends (no `debian/compat` file).
- `debian/source/format` = `3.0 (native)`.
- Offline builds: crates vendored under `vendor/`, `.cargo/config.toml` redirects to it, `cargo build --frozen`, `CARGO_NET_OFFLINE=true`. Neither `vendor/` nor `.cargo/config.toml` is committed.
- Package version starts at **0.9.0** (aligns with the shipped macOS app).
- Rust MSRV/build-dep: `rustc (>= 1.93), cargo`.
- Never hardcode tool names in more than one place where avoidable — `tt`, `tt-station-agentd`, `tt-station-agentd.service` already have canonical defaults on the Rust and panel sides; reuse them.
- `mock-box` and `libttstation` are NOT packaged.

---

### Task 1: Unify workspace version + bump-version.sh

**Files:**
- Modify: `Cargo.toml` (root workspace — add `[workspace.package]`)
- Modify: `crates/tt/Cargo.toml`, `crates/tt-station-agentd/Cargo.toml`, `crates/libttstation/Cargo.toml`, `crates/mock-box/Cargo.toml` (inherit version)
- Modify: `box-panel/tt-station-panel.py` (add `__version__`)
- Create: `scripts/bump-version.sh`

**Interfaces:**
- Produces: a single workspace version `0.9.0`, an importable-from-shell version bumper `scripts/bump-version.sh <version>` used by CI and Task 4's `version-consistency` job.

- [ ] **Step 1: Add `[workspace.package]` to the root `Cargo.toml`**

In `Cargo.toml`, immediately after the `[workspace]` block (before `[workspace.dependencies]`), add:

```toml
[workspace.package]
version = "0.9.0"
edition = "2021"
```

- [ ] **Step 2: Make each crate inherit the workspace version**

In each of `crates/tt/Cargo.toml`, `crates/tt-station-agentd/Cargo.toml`, `crates/libttstation/Cargo.toml`, `crates/mock-box/Cargo.toml`, replace the `[package]` `version = "0.0.1"` line (and `edition = "2021"` if present) with inheritance:

```toml
[package]
name = "tt"           # keep each crate's own name
version.workspace = true
edition.workspace = true
```

(Apply per-crate, keeping each `name`. If a crate lacks an `edition` line, only add the `version.workspace = true` line.)

- [ ] **Step 3: Add `__version__` to the panel**

In `box-panel/tt-station-panel.py`, just after the module docstring (before `import json`, around line 52), add:

```python
__version__ = "0.9.0"
```

- [ ] **Step 4: Write `scripts/bump-version.sh`**

```bash
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
```

Make it executable:

```bash
chmod +x scripts/bump-version.sh
```

- [ ] **Step 5: Verify the workspace still builds with the unified version**

Run: `cargo build --workspace`
Expected: builds successfully; `grep '^version' Cargo.toml` shows `version = "0.9.0"`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/*/Cargo.toml box-panel/tt-station-panel.py scripts/bump-version.sh
git commit -m "build: unify workspace version at 0.9.0 + add bump-version.sh"
```

(Note: `debian/changelog` does not exist yet — Task 2 creates it at 0.9.0. `bump-version.sh` prepends to it and is only exercised for later bumps.)

---

### Task 2: debian/ tree + build-deb.sh producing the `tt-station` core package

**Files:**
- Create: `debian/control`, `debian/rules`, `debian/changelog`, `debian/copyright`, `debian/source/format`
- Create: `deploy/tt-station-agentd.package.service` (the packaged, placeholder-free unit)
- Create: `build-deb.sh`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: the unified `0.9.0` version from Task 1.
- Produces: `../tt-station_0.9.0_amd64.deb` containing `/usr/bin/tt`, `/usr/bin/tt-station-agentd`, `/usr/lib/systemd/user/tt-station-agentd.service`, docs. The `tt-station-panel` stanza is added in Task 3.

- [ ] **Step 1: Create the packaged systemd unit**

The from-source unit (`deploy/tt-station-agentd.service`) has `{{AGENT_BIN}}`/`{{PATH_ENV}}` placeholders filled by `tt console --install-service`. The packaged install has a fixed binary path, so ship a resolved variant. Create `deploy/tt-station-agentd.package.service`:

```ini
[Unit]
Description=tt-station box agent (QuietBox control plane)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/bin/tt-station-agentd
# systemd --user services get a minimal PATH that omits ~/.local/bin and any
# virtualenv, so the agent couldn't find tt-smi (device-mesh detection) or the
# python3 that runs tt-inference-server's run.py. %h is the user's home; the
# tt-smi apt package installs to /usr/bin, and a venv/tt-smi symlink lives in
# ~/.local/bin — cover both.
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

- [ ] **Step 2: Create `debian/source/format`**

```
3.0 (native)
```

- [ ] **Step 3: Create `debian/changelog`**

```
tt-station (0.9.0) noble; urgency=medium

  * Initial Debian packaging: tt-station (tt CLI + tt-station-agentd + systemd
    user unit) and tt-station-panel (GTK4 on-box control panel).

 -- Tenstorrent <software@tenstorrent.com>  Fri, 10 Jul 2026 00:00:00 +0000
```

- [ ] **Step 4: Create `debian/copyright`**

```
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: tt-station
Upstream-Contact: Tenstorrent <software@tenstorrent.com>
Source: https://github.com/tsingletaryTT/tt-station

Files: *
Copyright: 2026 Tenstorrent Inc.
License: Apache-2.0

License: Apache-2.0
 Licensed under the Apache License, Version 2.0 (the "License");
 you may not use this file except in compliance with the License.
 You may obtain a copy of the License at
 .
     http://www.apache.org/licenses/LICENSE-2.0
 .
 Unless required by applicable law or agreed to in writing, software
 distributed under the License is distributed on an "AS IS" BASIS,
 WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 See the License for the specific language governing permissions and
 limitations under the License.
```

- [ ] **Step 5: Create `debian/control`** (both package stanzas — the panel's install rules come in Task 3, but declaring the stanza now is harmless since Task 3 adds its files)

```
Source: tt-station
Section: utils
Priority: optional
Maintainer: Tenstorrent <software@tenstorrent.com>
Build-Depends: debhelper-compat (= 13), rustc (>= 1.93), cargo
Standards-Version: 4.6.2
Homepage: https://github.com/tsingletaryTT/tt-station
Rules-Requires-Root: no

Package: tt-station
Architecture: amd64
Depends: ${shlibs:Depends}, ${misc:Depends}
Recommends: tt-smi, docker.io | docker-ce
Suggests: tt-station-panel
Description: Tenstorrent QuietBox control plane (CLI + box agent)
 tt-station makes a Tenstorrent QuietBox plug-and-play on the LAN: discover a
 box, pair once, run a model, and get one OpenAI-compatible /v1 endpoint served
 by tt-inference-server (vLLM).
 .
 This package ships the `tt` CLI and the box-side agent `tt-station-agentd`
 (installed as a systemd --user service, not auto-enabled). Enable it per-user
 with: systemctl --user enable --now tt-station-agentd

Package: tt-station-panel
Architecture: amd64
Depends: ${misc:Depends}, tt-station (= ${binary:Version}), python3, gir1.2-gtk-4.0, python3-gi
Recommends: docker.io | docker-ce, xdg-utils
Description: On-box GTK control panel for tt-station
 A small GTK4 control surface that runs on the QuietBox itself: shows the live
 6-digit pairing code, agent/serving status and endpoint, and Start/Stop/
 Restart/Reset the agent. Includes one-click Connect launchers (Open WebUI,
 opencode) for the model the box is serving.
```

- [ ] **Step 6: Create `debian/rules`** (installs the core package now; the panel install block is appended in Task 3)

```make
#!/usr/bin/make -f
# debian/rules — debhelper build rules for tt-station.
#
# Rust workspace: builds two release binaries (tt, tt-station-agentd) with safe
# default features. Crates are vendored under vendor/ and .cargo/config.toml
# redirects cargo there; --frozen enforces no network fetches. Run build-deb.sh
# to vendor first.

export DH_VERBOSE = 1
export DEB_BUILD_OPTIONS = nocheck

# Isolate cargo state from the developer's ~/.cargo during packaging.
export CARGO_HOME = $(CURDIR)/debian/.cargo
export CARGO_TARGET_DIR = $(CURDIR)/debian/target
export CARGO_NET_OFFLINE = true

%:
	dh $@

# ── Clean ──────────────────────────────────────────────────────────────────
override_dh_clean:
	# dh_clean's compat-13 project-wide find deletes *.orig files, which would
	# destroy the Cargo.toml.orig files under vendor/ that cargo's --frozen
	# checksum verification needs. Replicate dh_clean's safe actions manually
	# and skip the *.orig sweep.
	rm -f debian/debhelper-build-stamp
	rm -rf debian/.debhelper/
	rm -f debian/tt-station.substvars debian/tt-station-panel.substvars debian/files
	rm -rf debian/tt-station/ debian/tt-station-panel/ debian/tmp/

# ── Compile ────────────────────────────────────────────────────────────────
override_dh_auto_build:
	cargo build --release --frozen -p tt
	cargo build --release --frozen -p tt-station-agentd

# ── Install ────────────────────────────────────────────────────────────────
override_dh_auto_install:
	# ── tt-station (CLI + agent + systemd user unit) ──
	install -d debian/tt-station/usr/bin
	install -m 755 debian/target/release/tt \
	    debian/tt-station/usr/bin/tt
	install -m 755 debian/target/release/tt-station-agentd \
	    debian/tt-station/usr/bin/tt-station-agentd
	install -d debian/tt-station/usr/lib/systemd/user
	install -m 644 deploy/tt-station-agentd.package.service \
	    debian/tt-station/usr/lib/systemd/user/tt-station-agentd.service
	install -d debian/tt-station/usr/share/doc/tt-station
	install -m 644 README.md \
	    debian/tt-station/usr/share/doc/tt-station/README.md

# ── systemd: install the user unit but do NOT enable/start it on install ─────
override_dh_installsystemd:
	dh_installsystemd --name=tt-station-agentd --user --no-enable --no-start

# ── Skip tests (need hardware; CI runs them separately) ──────────────────────
override_dh_auto_test:
```

Make it executable:

```bash
chmod +x debian/rules
```

- [ ] **Step 7: Create `build-deb.sh`** (adapted from tt-toplike — binary/package names changed)

```bash
#!/bin/bash
# build-deb.sh — build Debian packages for tt-station.
#
#   1. Vendors crate deps into vendor/ (offline builds)
#   2. Writes .cargo/config.toml redirecting cargo to vendor/
#   3. Runs dpkg-buildpackage to produce the .deb files
#
# Usage:
#   ./build-deb.sh           # full (re-vendor)
#   ./build-deb.sh --quick   # skip re-vendoring (vendor/ already present)
#
# Output: ../tt-station_*.deb and ../tt-station-panel_*.deb
# Prereqs: sudo apt install devscripts debhelper rustc cargo
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

SKIP_VENDOR=false
for arg in "$@"; do
    case "$arg" in
        --quick) SKIP_VENDOR=true ;;
        --help|-h) sed -n '2,14p' "$0" | sed 's/^# //'; exit 0 ;;
    esac
done

for cmd in cargo dpkg-buildpackage dh; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "ERROR: '$cmd' not found. Install with:"
        echo "  sudo apt install devscripts debhelper rustc cargo"
        exit 1
    fi
done

echo "╔══════════════════════════════════════════"
echo "║  tt-station Debian package builder"
echo "╚══════════════════════════════════════════"

if [ "$SKIP_VENDOR" = true ] && [ -d vendor ]; then
    echo "⏭  Skipping vendor (--quick, vendor/ already exists)"
else
    echo "📦 Vendoring crate dependencies…"
    rm -rf vendor
    rm -f .cargo/config.toml
    cargo vendor vendor/ 2>&1 | grep -v "^$" || true
fi

mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
# Generated by build-deb.sh — do not edit. Redirects crate lookups to vendor/.
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF
echo "✍  Wrote .cargo/config.toml (vendor redirect)"

# dpkg-checkbuilddeps validates Build-Depends against APT's rustc, which on
# noble is too old for `rustc (>= 1.93)`. Skip that check whenever rustc isn't
# the apt one (rustup/CI); the top-of-script check already confirmed the tools.
SKIP_DEP_FLAG=""
if [ "${GITHUB_ACTIONS:-}" = "true" ] || [ "$(command -v rustc)" != "/usr/bin/rustc" ]; then
    SKIP_DEP_FLAG="-d"
    echo "ℹ  Skipping apt Build-Depends check (toolchain: $(command -v rustc))"
fi

echo "🔨 dpkg-buildpackage…"
dpkg-buildpackage -us -uc -b -jauto $SKIP_DEP_FLAG

echo "✅ Build complete. Packages:"
find .. -maxdepth 1 -name 'tt-station*.deb' -exec ls -lh {} \;
echo "Inspect: dpkg-deb --contents ../tt-station_*.deb"
```

Make it executable:

```bash
chmod +x build-deb.sh
```

- [ ] **Step 8: Add `.gitignore` entries** (create the file if absent, else append)

```
# Debian packaging build artifacts
/vendor/
.cargo/config.toml
debian/.cargo/
debian/target/
debian/files
debian/*.substvars
debian/debhelper-build-stamp
debian/.debhelper/
debian/tt-station/
debian/tt-station-panel/
```

- [ ] **Step 9: Build the package and verify the core layout**

Run: `./build-deb.sh`
Expected: succeeds; `../tt-station_0.9.0_amd64.deb` exists.

Then inspect:

Run: `dpkg-deb --contents ../tt-station_0.9.0_amd64.deb`
Expected: lists `./usr/bin/tt`, `./usr/bin/tt-station-agentd`, `./usr/lib/systemd/user/tt-station-agentd.service`, `./usr/share/doc/tt-station/README.md`.

Run: `dpkg-deb --info ../tt-station_0.9.0_amd64.deb`
Expected: `Package: tt-station`, `Version: 0.9.0`, the Recommends line.

- [ ] **Step 10: Commit**

```bash
git add debian/ deploy/tt-station-agentd.package.service build-deb.sh .gitignore
git commit -m "build: debian packaging tree + build-deb.sh (tt-station core package)"
```

---

### Task 3: Add the `tt-station-panel` package (wrapper, .desktop, icons)

**Files:**
- Create: `box-panel/tt-station-panel.wrapper` (installed as `/usr/bin/tt-station-panel`)
- Create: `box-panel/com.tenstorrent.ttstation.panel.desktop` (the packaged desktop entry)
- Modify: `debian/rules` (append the panel install block)

**Interfaces:**
- Consumes: the `tt-station-panel` stanza already declared in `debian/control` (Task 2, Step 5).
- Produces: `../tt-station-panel_0.9.0_amd64.deb` with the script at `/usr/share/tt-station-panel/tt-station-panel.py`, a `/usr/bin/tt-station-panel` wrapper, a system `.desktop`, and hicolor icons.

- [ ] **Step 1: Create the launcher wrapper**

`box-panel/tt-station-panel.wrapper`:

```bash
#!/bin/bash
# Launch the packaged tt-station GTK panel. Installed as /usr/bin/tt-station-panel.
exec python3 /usr/share/tt-station-panel/tt-station-panel.py "$@"
```

- [ ] **Step 2: Create the packaged `.desktop`**

`box-panel/com.tenstorrent.ttstation.panel.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=tt-station Panel
Comment=On-box control panel for Tenstorrent tt-station
Exec=/usr/bin/tt-station-panel
Icon=com.tenstorrent.ttstation.panel
Terminal=false
Categories=Utility;System;
StartupWMClass=com.tenstorrent.ttstation.panel
```

- [ ] **Step 3: Append the panel install block to `debian/rules`**

Add to the end of the `override_dh_auto_install:` recipe (after the tt-station lines from Task 2):

```make
	# ── tt-station-panel (GTK on-box GUI) ──
	install -d debian/tt-station-panel/usr/share/tt-station-panel
	install -m 755 box-panel/tt-station-panel.py \
	    debian/tt-station-panel/usr/share/tt-station-panel/tt-station-panel.py
	install -d debian/tt-station-panel/usr/bin
	install -m 755 box-panel/tt-station-panel.wrapper \
	    debian/tt-station-panel/usr/bin/tt-station-panel
	install -d debian/tt-station-panel/usr/share/applications
	install -m 644 box-panel/com.tenstorrent.ttstation.panel.desktop \
	    debian/tt-station-panel/usr/share/applications/com.tenstorrent.ttstation.panel.desktop
	# Icons (existing PNGs under box-panel/assets/icons/hicolor)
	for sz in 48x48 128x128 256x256; do \
	    install -d debian/tt-station-panel/usr/share/icons/hicolor/$$sz/apps; \
	    install -m 644 box-panel/assets/icons/hicolor/$$sz/apps/com.tenstorrent.ttstation.panel.png \
	        debian/tt-station-panel/usr/share/icons/hicolor/$$sz/apps/com.tenstorrent.ttstation.panel.png; \
	done
	install -d debian/tt-station-panel/usr/share/doc/tt-station-panel
	install -m 644 box-panel/README.md \
	    debian/tt-station-panel/usr/share/doc/tt-station-panel/README.md
```

- [ ] **Step 4: Rebuild and verify the panel package layout**

Run: `./build-deb.sh --quick`
Expected: succeeds; `../tt-station-panel_0.9.0_amd64.deb` exists.

Run: `dpkg-deb --contents ../tt-station-panel_0.9.0_amd64.deb`
Expected: lists `./usr/share/tt-station-panel/tt-station-panel.py`, `./usr/bin/tt-station-panel`, `./usr/share/applications/com.tenstorrent.ttstation.panel.desktop`, and the three hicolor icon PNGs.

- [ ] **Step 5: Commit**

```bash
git add box-panel/tt-station-panel.wrapper box-panel/com.tenstorrent.ttstation.panel.desktop debian/rules
git commit -m "build: add tt-station-panel debian package (wrapper, desktop, icons)"
```

---

### Task 4: Guard the panel's runtime desktop-file generator for packaged installs

**Files:**
- Modify: `box-panel/tt-station-panel.py` (the `install_desktop_icon()` function, ~line 686, and its call site in `main`, ~line 749)

**Interfaces:**
- Consumes: the packaged `.desktop` path from Task 3 (`/usr/share/applications/com.tenstorrent.ttstation.panel.desktop`) and the packaged script path (`/usr/share/tt-station-panel/`).
- Produces: no new symbols; makes `install_desktop_icon()` a no-op when running from the package so it doesn't double-register a per-user entry.

- [ ] **Step 1: Read the current `install_desktop_icon()` and its call site**

Run: `sed -n '686,748p' box-panel/tt-station-panel.py`
Expected: see the function body and the `main()` call (`install_desktop_icon()` near line 749).

- [ ] **Step 2: Add an early-return guard at the top of `install_desktop_icon()`**

Insert as the first statements inside `install_desktop_icon()` (before it copies icons / writes the per-user `.desktop`):

```python
    # Packaged installs ship the .desktop + icons in system dirs (see the
    # tt-station-panel .deb). When we're running from the packaged location, or
    # the system entry already exists, skip the per-user self-install so we
    # don't double-register. The from-source run (python3 box-panel/…) still
    # self-installs, since neither condition holds there.
    packaged_script = "/usr/share/tt-station-panel/"
    system_desktop = Path("/usr/share/applications/com.tenstorrent.ttstation.panel.desktop")
    if str(Path(__file__).resolve()).startswith(packaged_script) or system_desktop.exists():
        return
```

- [ ] **Step 3: Verify the guard doesn't break the from-source run**

Run: `python3 -c "import ast; ast.parse(open('box-panel/tt-station-panel.py').read()); print('parse ok')"`
Expected: `parse ok` (syntax valid; GTK need not be installed to parse).

- [ ] **Step 4: Verify the guard logic in isolation**

Run:
```bash
python3 - <<'PY'
from pathlib import Path
# Simulate: running from packaged path → guard should trip.
packaged_script = "/usr/share/tt-station-panel/"
p = "/usr/share/tt-station-panel/tt-station-panel.py"
assert str(Path(p)).startswith(packaged_script), "packaged path should trip guard"
# Simulate: from-source path → guard should NOT trip (assuming no system desktop).
src = "/home/ttuser/code/tt-station/box-panel/tt-station-panel.py"
assert not str(Path(src)).startswith(packaged_script), "source path must not trip on path alone"
print("guard logic ok")
PY
```
Expected: `guard logic ok`.

- [ ] **Step 5: Commit**

```bash
git add box-panel/tt-station-panel.py
git commit -m "fix(panel): skip runtime desktop self-install when packaged"
```

---

### Task 5: CI — release workflow + version-consistency check

**Files:**
- Create: `.github/workflows/release.yml`
- Modify: `.github/workflows/ci.yml` (add a `version-consistency` job; create the file if it does not exist)

**Interfaces:**
- Consumes: `build-deb.sh` (Task 2), the unified version (Task 1), both package stanzas (Tasks 2–3).
- Produces: on a `v*` tag, GitHub Release assets `tt-station_<ver>_amd64_<suite>.deb` and `tt-station-panel_<ver>_amd64_<suite>.deb` for noble + jammy.

- [ ] **Step 1: Check whether `.github/workflows/ci.yml` exists**

Run: `ls .github/workflows/`
Expected: note whether `ci.yml` and `macos-release.yml` exist (macos-release.yml should — leave it untouched).

- [ ] **Step 2: Create `.github/workflows/release.yml`**

```yaml
name: Release

on:
  push:
    tags:
      - 'v*'

permissions:
  contents: write   # upload release assets

jobs:
  build-deb:
    name: Build .deb (${{ matrix.suite }})
    runs-on: ${{ matrix.runner }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - runner: ubuntu-24.04
            suite:  noble
          - runner: ubuntu-22.04
            suite:  jammy
    steps:
      - uses: actions/checkout@v4

      - name: Install packaging prerequisites
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends devscripts debhelper rustc cargo

      # rustup toolchain (>= 1.93) needed: apt's rustc is too old for the build-dep.
      - name: Install Rust toolchain
        run: |
          curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.93.0
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"

      - name: Patch changelog suite (${{ matrix.suite }})
        if: matrix.suite != 'noble'
        run: sed -i "1s/) noble;/) ${{ matrix.suite }};/" debian/changelog

      - name: Build .deb packages
        run: ./build-deb.sh

      - name: Rename packages with suite suffix
        run: |
          SUITE="${{ matrix.suite }}"
          for deb in ../tt-station*.deb; do
            newname="${deb%.deb}_${SUITE}.deb"
            mv "$deb" "$newname"
            echo "Renamed: $(basename "$deb") → $(basename "$newname")"
          done

      - name: Ensure release exists, then upload packages
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          SUITE="${{ matrix.suite }}"
          if gh release view "$GITHUB_REF_NAME" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
            echo "Release $GITHUB_REF_NAME already exists — skipping create."
          else
            gh release create "$GITHUB_REF_NAME" \
              --repo "$GITHUB_REPOSITORY" \
              --title "$GITHUB_REF_NAME" \
              --generate-notes \
              --verify-tag
          fi
          gh release upload "$GITHUB_REF_NAME" \
            ../tt-station_*_amd64_${SUITE}.deb \
            ../tt-station-panel_*_amd64_${SUITE}.deb \
            --repo "$GITHUB_REPOSITORY" \
            --clobber
```

(Note: unlike tt-toplike, `build-deb.sh` runs full — not `--quick` — because `vendor/` is not committed. The rustup step supplies `rustc >= 1.93`; `build-deb.sh` auto-adds `-d` since rustc is not `/usr/bin/rustc`.)

- [ ] **Step 3: Add the `version-consistency` job to `.github/workflows/ci.yml`**

If `ci.yml` exists, add this job under `jobs:`. If it does not exist, create `.github/workflows/ci.yml` with a `name`, an `on: [push, pull_request]` trigger, and this single job:

```yaml
  version-consistency:
    name: Version consistency
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - name: Assert version matches across Cargo.toml, changelog, and the panel
        run: |
          set -euo pipefail
          CARGO=$(grep -m1 '^version = ' Cargo.toml | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
          DEB=$(head -1 debian/changelog | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
          PANEL=$(grep -m1 '^__version__ = ' box-panel/tt-station-panel.py | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
          echo "Cargo.toml:       $CARGO"
          echo "debian/changelog: $DEB"
          echo "panel __version__: $PANEL"
          fail=0
          [ "$DEB" = "$CARGO" ]   || { echo "::error::debian/changelog ($DEB) != Cargo.toml ($CARGO)"; fail=1; }
          [ "$PANEL" = "$CARGO" ] || { echo "::error::panel __version__ ($PANEL) != Cargo.toml ($CARGO)"; fail=1; }
          [ "$fail" -eq 0 ] || { echo "Run scripts/bump-version.sh <version> to fix."; exit 1; }
          echo "All version strings agree: $CARGO"
```

- [ ] **Step 4: Validate the workflow YAML**

Run:
```bash
python3 -c "import yaml,sys; [yaml.safe_load(open(f)) for f in ['.github/workflows/release.yml','.github/workflows/ci.yml']]; print('yaml ok')"
```
Expected: `yaml ok`. (If PyYAML is unavailable, `pip install pyyaml` first, or skip and rely on GitHub's parse on push.)

- [ ] **Step 5: Verify the version-consistency logic locally**

Run:
```bash
CARGO=$(grep -m1 '^version = ' Cargo.toml | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
DEB=$(head -1 debian/changelog | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
PANEL=$(grep -m1 '^__version__ = ' box-panel/tt-station-panel.py | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
echo "$CARGO $DEB $PANEL"; [ "$CARGO" = "$DEB" ] && [ "$CARGO" = "$PANEL" ] && echo AGREE
```
Expected: `0.9.0 0.9.0 0.9.0` then `AGREE`.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/release.yml .github/workflows/ci.yml
git commit -m "ci: release workflow (per-suite .debs) + version-consistency check"
```

---

## Self-Review Notes

- **Spec coverage:** Two-package split (Task 2/3), user unit not-auto-enabled (Task 2 `override_dh_installsystemd`), packaged .desktop + guard (Task 3/4), build-deb.sh + vendored offline (Task 2), release.yml per-suite + version-consistency (Task 5), versioning at 0.9.0 + bump-version.sh (Task 1). All covered.
- **Not packaged:** mock-box, libttstation — never installed by rules.
- **Panel README:** installed to both doc dirs; harmless duplication.

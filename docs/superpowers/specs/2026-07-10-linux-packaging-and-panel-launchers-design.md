# Linux packaging + GTK panel launchers — design

**Date:** 2026-07-10
**Status:** approved (brainstorming), pending implementation plan
**Builds on:** the Rust workspace (`crates/tt`, `crates/tt-station-agentd`), the GTK box
panel (`box-panel/tt-station-panel.py`), the systemd-user lifecycle model
(`deploy/tt-station-agentd.service`, `tt console --install-service`), and the macOS
Connect launchers (`macos/TTStation/…/OpenWebUILauncher.swift`,
`OpenCodeLauncher.swift`, `LaunchController.swift`).
**Reference for packaging:** `~/code/tt-toplike` (debhelper + `build-deb.sh` + vendored
crates + per-suite `release.yml`).

## Goal

Make the Linux side of tt-station **installable via Ubuntu `.deb` packages** (the way
tt-toplike is), and bring the macOS app's one-click **Connect** launchers (Open WebUI,
opencode) to the on-box GTK panel — so an operator sitting at the QuietBox can go from
"a model is serving" to "a chat/coding session against it" in one click, locally.

Two cohesive but independent deliverables, each with its own implementation plan:
- **Part A** — Debian packaging.
- **Part B** — GTK panel launchers.

---

## Part A — Debian packaging

### Package split (two packages)

Mirrors tt-toplike's TUI/app split so headless boxes install only the core.

**`tt-station`** — the headless control plane (`Architecture: amd64`):
- `/usr/bin/tt` — the CLI (Rust `crates/tt`).
- `/usr/bin/tt-station-agentd` — the box agent (Rust `crates/tt-station-agentd`).
- `/usr/lib/systemd/user/tt-station-agentd.service` — the **user** unit, installed but
  **not auto-enabled / not auto-started** (`dh_installsystemd --user --no-enable
  --no-start`). `ExecStart=/usr/bin/tt-station-agentd` and `Environment=PATH=…` are
  resolved at package-build time (the `{{AGENT_BIN}}`/`{{PATH_ENV}}` placeholders that
  `tt console --install-service` fills are dropped for the packaged unit, since the
  installed path is fixed). The operator enables it per-user:
  `systemctl --user enable --now tt-station-agentd` (and `loginctl enable-linger <user>`
  to survive reboot). `tt console --install-service` continues to work unchanged for the
  from-source / non-packaged flow.
- `/usr/share/doc/tt-station/` — README + selected `docs/reference/*.md`.
- `Depends: ${shlibs:Depends}, ${misc:Depends}`
- `Recommends: tt-smi, docker.io | docker-ce`
- `Suggests: tt-station-panel`

**`tt-station-panel`** — the on-box GTK4 GUI:
- `/usr/bin/tt-station-panel` — a wrapper that execs the installed script (the script
  itself lives at `/usr/share/tt-station-panel/tt-station-panel.py`).
- `/usr/share/applications/com.tenstorrent.ttstation.panel.desktop` — a **packaged**
  `.desktop` (`Exec=/usr/bin/tt-station-panel`, `Icon=com.tenstorrent.ttstation.panel`).
  Today `install_desktop_icon()` generates this at runtime; when the packaged file is
  present the runtime generator becomes a no-op (guard on existence), so the from-source
  run (`python3 box-panel/tt-station-panel.py`) still self-installs its desktop entry.
- `/usr/share/icons/hicolor/{48x48,128x128,256x256}/apps/com.tenstorrent.ttstation.panel.png`
  — the existing PNGs from `box-panel/assets/icons/`.
- `Depends: tt-station (= ${binary:Version}), python3, gir1.2-gtk-4.0, python3-gi`
- `Recommends: docker.io | docker-ce, xdg-utils`

`mock-box` (dev fixture) and `crates/libttstation` (library) are **not** packaged.

### Build mechanics (copied from tt-toplike)

New `debian/` tree at the repo root:
- `debian/control` — one `Source:` stanza + the two `Package:` stanzas above.
  `Build-Depends: debhelper-compat (= 13), rustc (>= 1.93), cargo`.
  `Rules-Requires-Root: no`. `Maintainer: Tenstorrent <software@tenstorrent.com>`.
- `debian/rules` — `dh $@` with:
  - `override_dh_auto_build`: `cargo build --release --frozen -p tt` and
    `-p tt-station-agentd` (safe default features).
  - `override_dh_auto_install`: `install` the two binaries into
    `debian/tt-station/usr/bin/`, the resolved systemd unit into
    `debian/tt-station/usr/lib/systemd/user/`, and the panel script + wrapper + desktop +
    icons into `debian/tt-station-panel/…`.
  - `override_dh_auto_test`: empty (tests need hardware; CI runs them separately).
  - `override_dh_clean`: replicate the safe dh_clean actions but **skip the `*.orig`
    sweep** so vendored `Cargo.toml.orig` files survive (`--frozen` checksum verification
    needs them) — verbatim from tt-toplike.
  - `dh_installsystemd --name=tt-station-agentd --user --no-enable --no-start` for the
    agent unit.
  - Build-time env: `CARGO_HOME=$(CURDIR)/debian/.cargo`,
    `CARGO_TARGET_DIR=$(CURDIR)/debian/target`, `CARGO_NET_OFFLINE=true`,
    `DEB_BUILD_OPTIONS=nocheck`.
- `debian/changelog` — version + suite (`noble`) source of truth.
- `debian/copyright`, `debian/source/format` = `3.0 (native)`. No `compat` file (declared
  via `debhelper-compat` build-dep). No `.install` files (imperative install in `rules`).

`build-deb.sh` at the repo root — copied from tt-toplike almost verbatim (vendor crates,
write `.cargo/config.toml` source-replacement, `dpkg-buildpackage -us -uc -b -jauto`,
add `-d` when `rustc` is a rustup toolchain, outputs land in `../`). Prereqs:
`sudo apt install devscripts debhelper rustc cargo`.

Optional secondary: `[package.metadata.deb]` in `Cargo.toml` for quick `cargo deb`
developer builds (single-package convenience only, like tt-toplike).

### CI

`.github/workflows/release.yml` (new) — triggered on `v*` tags:
- `build-deb` matrix over `{ubuntu-24.04→noble, ubuntu-22.04→jammy}`, `fail-fast: false`.
  Install prereqs, patch the changelog suite for non-noble, `./build-deb.sh --quick`,
  rename outputs with a `_<suite>` suffix, race-tolerant `gh release create … || true`
  then `gh release upload … --clobber`. Both `tt-station_*.deb` and
  `tt-station-panel_*.deb` per suite.
- Reuse the existing macOS release workflow untouched (`macos-release.yml` already
  handles the DMG on `v*` tags).

`.github/workflows/ci.yml` — add a `version-consistency` job asserting the version matches
across `Cargo.toml` (workspace), `debian/changelog`, and the panel's `__version__`.

### Versioning

Introduce a single source of truth:
- Set `version` in the root `Cargo.toml` `[workspace.package]`; each crate uses
  `version.workspace = true` (they are `0.0.1` today).
- Start the Debian packages at **0.9.0** to align with the shipped macOS app.
- Add `scripts/bump-version.sh` (modeled on tt-toplike's) that updates, in lockstep:
  the workspace `Cargo.toml` version, the first-line token in `debian/changelog`, and the
  panel's `__version__`. It edits only; it does not commit.

Release artifacts: `tt-station_<version>_amd64_<suite>.deb`,
`tt-station-panel_<version>_amd64_<suite>.deb`, published to GitHub Releases.

---

## Part B — GTK panel one-click launchers

A new **Connect row** in `tt-station-panel.py`, shown only when the box is serving
(the snapshot's `serving` list is non-empty). It ports the macOS Connect launchers,
simplified because the panel runs **on the box** (no SSH, no `osascript`, no IPv4
resolution dance).

### Endpoint resolution (from the snapshot the panel already polls)

The launchers need `base_url` + `model`. Both come from the snapshot's agent-source
`ServingEntry` (`crates/libttstation/src/model.rs`: `model`, `base_url`, `host_port`,
`source`). The panel already receives the `serving` list in every
`tt console --snapshot` poll, so:
- Pick the entry with `source == "agent"`; fall back to the first entry if none is tagged
  agent (an external run.py the operator wants to connect to is still connectable).
- `base_url` → the full `http://<host>:<port>/v1`. `servingPort` for the docker command is
  parsed from `base_url` (fall back to the panel's `TTS_SERVING_PORT`).
- `model` → the served model id (may contain a `/`, e.g. `meta-llama/Llama-3.3-70B`).

This avoids the unimplemented `/endpoint` route and keeps the panel's "snapshot is the
single source of truth" invariant.

### Launchers

**Open WebUI** (local `docker run`) — reuse the exact image + idempotent logic from
`OpenWebUILauncher`, run locally (not over SSH):
- Container `ttstation-openwebui`, image `ghcr.io/open-webui/open-webui:main`, published
  `-p 3000:8080`, `--add-host=host.docker.internal:host-gateway`,
  `-e OPENAI_API_BASE_URL=http://host.docker.internal:<servingPort>/v1`,
  `-e OPENAI_API_KEY=sk-none -e WEBUI_AUTH=false`,
  `-v ttstation-openwebui:/app/backend/data`.
- Reuse-if-running fast path (`docker inspect -f '{{.State.Running}}'`), else
  `docker rm -f` + retry-loop pull + `docker run -d`.
- Run in a worker thread (first pull is slow), poll `http://localhost:3000/health`
  (~180s ceiling), then `xdg-open http://localhost:3000`. Errors marshaled back via
  `GLib.idle_add` to an inline status label.

**opencode** (local terminal) — reuse the `OpenCodeLauncher` config shape:
- Write `opencode.json` (`$schema`, `provider.ttstation` = `@ai-sdk/openai-compatible` +
  `options.baseURL` + `models[<model>]`, top-level `model: ttstation/<model>`) to
  `~/.local/share/tt-station/opencode/<host_port-safe>/`. Rely on opencode's first-`/`
  split so a vendored `<vendor>/<model>` id resolves under the `ttstation` provider (no
  manual re-split, matching macOS).
- Precheck the `opencode` binary (probe `~/.local/bin`, `/usr/local/bin`, `/usr/bin`);
  if missing, show an actionable inline hint (install pointer) rather than opening a
  "command not found" terminal. No auto-install on Linux (unlike brew on macOS).
- Resolve a terminal emulator by trying, in order: `x-terminal-emulator` (Debian
  alternatives), `gnome-terminal`, `konsole`, `xterm`. Spawn it running
  `bash -lc "cd '<dir>' && opencode"` (login shell so PATH resolves opencode). If none
  found, inline error.

**Copy endpoint / Open in browser** — a "Copy `/v1`" button (writes `base_url` to the
Gdk clipboard) and an "Open endpoint" button (`xdg-open <base_url>`). Cheap QoL.

### Structure

Follow the panel's existing conventions:
- **Pure builders** (no GTK, unit-testable via stdlib `unittest`, alongside the existing
  `derive_view`): `build_openwebui_command(serving_port) -> str`,
  `build_opencode_config(base_url, model) -> str` (JSON),
  `opencode_terminal_command(config_dir) -> str`, `resolve_terminal_emulator() -> list |
  None`, and an `endpoint_from_snapshot(snap) -> (base_url, model) | None` helper.
- **Glue** mirrors `reset_fresh`'s worker-thread pattern (`threading.Thread` +
  `subprocess.run(..., check=False)` + `GLib.idle_add` to update labels). Each button has
  its own inline status/spinner label and disables while in flight.
- The Connect row is built like the profile row and shown/hidden on each render:
  `self.connect_row.set_visible(endpoint is not None)`.

### View wiring

New `Gtk.Box` "Connect" row added to `root`, below the serving/endpoint labels. Buttons:
**Open WebUI**, **opencode**, **Copy `/v1`**, **Open endpoint** — each
`Gtk.Button(...).connect("clicked", lambda _b: self.method())`, following the existing
button-wiring pattern. Sensitivity/visibility derived from the polled snapshot in
`_render_snapshot` / a new `_refresh_connect()` helper.

### Config

Reuse existing `TTS_*` env vars for defaults (`TTS_SERVING_HOST`/`TTS_SERVING_PORT` as
fallbacks). Add `TTS_OPENWEBUI_PORT` (default 3000) for the published host port, so an
operator whose 3000 is taken can move it.

---

## Data flow

Model serving → snapshot's `serving` list non-empty → Connect row visible.
- **Open WebUI:** click → (reuse or `docker run` locally) → poll `:3000/health` →
  `xdg-open` browser → chatting with the box's model.
- **opencode:** click → write per-endpoint `opencode.json` → terminal emulator runs
  `cd <dir> && opencode` → coding against the box.
- **Copy / Open:** click → clipboard / `xdg-open`.

## Error handling

- Missing tool (`docker`, `opencode`, no terminal emulator, no `xdg-open`) → explicit
  inline message, no silent failure, no terminal-of-shame.
- Open WebUI health-poll timeout → surfaced inline; the container keeps running so a retry
  reattaches.
- Nothing serving → the Connect row isn't shown at all.
- Snapshot unreadable (existing "unknown" state) → Connect row hidden.

## Testing

- **Rust:** `cargo test --workspace` / `clippy` unaffected by packaging. The `.deb` build
  is CI-verified per suite (build succeeds, both packages produced). A local
  `dpkg-deb -c`/`lintian` sanity check on the artifacts.
- **Python:** unit-test the pure builders (`build_openwebui_command`,
  `build_opencode_config` valid JSON containing base_url+model+`ttstation/<model>`,
  `opencode_terminal_command`, `resolve_terminal_emulator`, `endpoint_from_snapshot`).
- **Owner click-through (live box):** install both `.deb`s in a container/box, enable the
  user service, serve a model, then from the panel: Open WebUI opens a working chat,
  opencode opens a terminal talking to the box, Copy/Open behave.

## Deferred (not now)

- tt-toplike launcher button on the panel (not selected; trivial once tt-toplike is
  apt-installable) and the VS Code / workbench launchers.
- An apt PPA / hosted repo — GitHub Releases `.deb`s only, like tt-toplike.
- Auto-installing opencode on Linux (macOS uses brew; Linux surfaces a hint).
- A system-wide (non-user) agent service variant.
- Per-endpoint launcher when multiple models serve (uses the agent-source/first entry).

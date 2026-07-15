# tt-station — project CLAUDE.md

Plug-and-play Tenstorrent from a Mac: discover a QuietBox on the LAN, pair once,
`tt run <model>`, get one OpenAI-compatible `/v1`. **No llama.cpp** — usability rides
on the `/v1` that `tt-inference-server` (vLLM, via `run.py`) exposes.

Repo: github.com/tsingletaryTT/tt-station (private). Work happens on `main`.
This box (`tsingletaryTT-quietbox`) IS a real QuietBox: 4× Blackhole (`p300c`),
`tt-smi`, docker, `~/code/tt-inference-server`. Real serving has been proven here.

---

## ▶ PICK UP HERE

**Box power & hardware controls shipped (v0.10.0, merge `dd42c9a`, 2026-07-15):** the box
can now be power-managed end to end — agent `POST /power` (authed: `reset-chips` via `tt-smi -r`
keeps pairing → 200; `suspend`/`reboot`/`shutdown` machine ops → 202 then disconnect; polkit
denial → 403), the NIC MAC is advertised in `/status` + mDNS TXT for **Wake-on-LAN**, `tt power`
/ `tt wake` CLI subcommands, `libttstation` WoL magic-packet builder, GTK panel local power row
(with confirm), macOS PowerMenu in the header + popover, and a polkit rule packaged as a conffile
(`deploy/tt-station-power.rules`). Reference: `docs/reference/power-controls.md`.

**Box-side is built, installed, and verified live on this box (2026-07-15):** `cargo build/test/
clippy` all green (fixed a pre-existing clippy `doc_lazy_continuation` in `catalog.rs`, commit
`876ffd3`); fresh `tt` + `tt-station-agentd` copied into `~/.local/bin` and the systemd `--user`
agent restarted. Confirmed live: `/status` carries `mac`, `POST /power` → 401 unauthed (route
exists), `tt power`/`tt wake` in `--help`. **Still owner-gated:** macOS click-through of the new
PowerMenu on a real Mac, and a `.deb` install-test that exercises the polkit rule on a box.

### macOS build/click-through

**The macOS app (`macos/TTStation`, MARKETING_VERSION now 0.10.0) was authored on a Linux box
with NO Swift toolchain, but now BUILDS + SHIPS via CI:** `macos-release.yml` compiles it
on a `macos-14` runner and published `TTStation-0.9.0-arm64.dmg` to the **v0.9.0 GitHub
Release** (2026-07-10, alongside the four `.deb`s). CI gotcha found + fixed: brew's latest
`xcodegen` emits objectVersion 77 (Xcode 16 format) which the runner's default Xcode can't
read ("future Xcode project file format (77)") — the workflow now selects the newest
`/Applications/Xcode_*.app` before `xcodebuild`. Still worth a **local** click-through on a
real Mac against the live box (CI only proves it compiles, not that the UI/launchers behave).

```
cd macos/TTStation && swift test                      # TTStationKit unit tests (pure logic)
cd macos/TTStation/AppShell && xcodegen generate \
  && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Then click-through against the live box (see "Activate the live box" below):
discover → pair → (smart-default model pre-selected) → search/pick → **Run** (amber
"starting"→green "serving") → **Connect** buttons (Open WebUI / opencode) → the
"Serving" list.

**Written-blind spots to eyeball on the Mac** (all target macOS 14):
- opencode provider/model id split on `ttstation/<vendor>/<model>` (`OpenCodeLauncher`).
- `Label` button width in the MenuBarExtra popover; the plain-`TextField` search (I
  avoided `.searchable` in the popover for compatibility) look; `LazyVStack` pinned
  section headers (`ModelPickerView`).
- `LaunchController` (Process/osascript/NSWorkspace) is owner-verified, not unit-tested.

Per-feature reports (what each change did + assumptions) are in `.superpowers/sdd/*.md`
(gitignored, local to this box) — regenerate/ignore as needed; the source + tests are
the truth.

**tt-toplike is a SEPARATE repo, owner-managed.** The remote-QuietBox work
(`WsBackend`, `--remote HOST:PORT`, `--remote <name>` via `tt discover`, and the
`/remote` slash command that hot-swaps to a box's live telemetry) is on tt-toplike's
`inference-server-monitoring` branch, committed locally there — NOT pushed by this
session, NOT part of tt-station. Design: `~/code/tt-toplike/docs/REMOTE_QUIETBOX_DESIGN.md`.

---

## Current state (what's shipped on `main`)

**Agent (`crates/tt-station-agentd`)** — box-side daemon, default backend `runpy`:
- Serves via `tt-inference-server/run.py`, deferring device/image/impl/engine to it.
  On this board `run.py` can't auto-detect the device, so the agent does: **`--tt-device`
  auto-detected from `tt-smi`** (4× p300c → `p300x2`); **`--serving-image` must be pinned**
  (image↔run.py compat is a curated matrix; `--auto-image` picks newest-local but is
  unsafe/opt-in). Before each serve: **stop any stale container on the port** + **`tt-smi -r`
  board reset** (clears wedged mesh ethernet cores). Readiness is gated on **`/v1/models`
  actually listing the model** (not just `/health`) — no dead endpoints; served id comes
  from `/v1/models`. Health-poll ceiling ~40 min.
- Pairing: 6-digit code (TTL + `MAX_PAIR_ATTEMPTS` lockout), **tokens persisted**
  (`--token-store`, default `~/.config/tt-station/agentd-tokens.json`) so pairing survives
  restarts. Two-step `pair-init`/`pair-complete` for the app.
- Routes: `GET /status` (unauthed), `GET /models` (unauthed, **vLLM-servable only**),
  `POST /pair/init|complete`, `POST /run|stop`, `GET /endpoint`, `POST /reset` (authed),
  `GET /telemetry` (**WebSocket**, unauthed — streams `tt-smi -s` for remote tt-toplike;
  frames now carry an OPTIONAL ADDITIVE `tt_toplike` key alongside the verbatim tt-smi JSON —
  `{ schema: 1, processes: [{pid,name,cmd,uses_tt,cpu_pct,mem_bytes}] }`, `uses_tt` best-effort
  (only processes the agent's uid can inspect); `inference` is DEFERRED (its absence means
  tt-toplike falls back to local view for that panel) — see `TT_TOPLIKE_STREAM.md`),
  `GET /serving` (unauthed — every running `tt-inference-server` `/v1`, `source: agent|external`),
  `GET /config` (unauthed — redacted resolved config, no secrets),
  `GET /logs` (unauthed — `?source=container|run&tail=N` tail of a `workflow_logs/` file;
  `container`=serving-container stdout/stderr where failures actually live, `run`=run.py's
  own log; `409` on non-runpy backends, `200`/`lines: []` when nothing's logged yet),
  `GET /logs/stream` (unauthed **WebSocket** — replays then follows the same source,
  re-resolving the newest file each ~500ms so a fresh serve is picked up). Every emitted
  log line is redacted (masks `hf_…`/`sk-…`/`Bearer …` shapes) — see `docs/reference/logs.md`.
  **`POST /power` (authed)** — `reset-chips` (`tt-smi -r`, keeps pairing) → 200; machine ops
  `suspend`/`reboot`/`shutdown` → 202 (box disconnects shortly after); polkit denial → 403.
  The primary-NIC MAC is advertised in `/status` + the mDNS TXT record so a Mac can Wake-on-LAN
  a powered-down box. Command executor is configurable; see `docs/reference/power-controls.md`.
- mDNS `_tenstorrent._tcp` status re-published on run/stop; graceful shutdown unregisters.
- **Config file + named profiles:** optionally reads `agentd.toml` (default
  `$TT_CONFIG_DIR/agentd.toml` or `~/.config/tt-station/agentd.toml`, override with
  `--config`) with named `[profile.*]` serving configs (e.g. `stable`/`bleeding`), selected via
  `--profile` (else `default_profile`, else the sole profile, else today's flag-only behavior).
  `--print-config` resolves and prints the redacted summary without binding the port. No config
  file at all = unchanged pre-feature behavior. Full schema/precedence/errors:
  `docs/reference/agentd-config.md`; copy-paste starter: `box-panel/agentd.example.toml`.

**CLI (`crates/tt`):** `discover` (`--host`/`--no-mdns`), `pair`/`pair-init`/`pair-complete`,
`run`, `stop`, `status` (unauthed), `endpoint`, `models`, `serving`, `reset`,
**`power`** (authed — `reset-chips`/`suspend`/`reboot`/`shutdown`) and **`wake`** (broadcasts a
Wake-on-LAN magic packet from this machine using the MAC learned at discovery, or `--mac`; needs
WoL enabled in the box BIOS/NIC — see `docs/reference/power-controls.md`),
`config` (unauthed — active/available profiles + resolved backend + serving host/port, mirrors
`GET /config`; see `docs/reference/agentd-config.md`), **`logs`** (unauthed —
`tt logs [--source container|run] [--tail N] [--follow]`; one-shot tail respects global
`--json`, `--follow` streams plain lines from `GET /logs/stream` until Ctrl-C; see
`docs/reference/logs.md`), **`console`** (ratatui SSH operator TUI
for THIS box's agent — start/stop/restart/reset/pair-localhost/profile-cycle/install-service;
`--snapshot` prints one `BoxLifecycleSnapshot` JSON and exits, `--install-service` installs the
systemd unit and exits; now also has an auto-tailing serving-log pane sourced from
`GET /logs?source=container`; see `docs/reference/tt-console.md`). Global `--json`.
Tokens in macOS Keychain / file store. Respects `TT_CONFIG_DIR`.

**Agent as a `systemctl --user` service:** the agent can run under the user's systemd
instance instead of ad-hoc `Popen` supervision — unit template at
`deploy/tt-station-agentd.service` (`ExecStart={{AGENT_BIN}}`, `Restart=on-failure`),
installed via `tt console --install-service` into `~/.config/systemd/user/`. Survives SSH
disconnect; survives reboot too once `loginctl enable-linger` is run for the user. Start/
Stop/Restart under this model are just `systemctl --user start|stop|restart
tt-station-agentd.service`.

**Box panel (`box-panel/tt-station-panel.py`, GTK4):** the box's own screen — Start/Stop/
Restart/Reset the agent, a **local power row** (reset-chips/suspend/reboot/shutdown, each behind
a confirm dialog — pure builders in `panel_launchers.py`), **live 6-digit pairing code** (with TTL), status/endpoint,
**profile dropdown** (reads `agentd.toml`'s profile list, passes `--profile` on Start/Restart;
hidden when no config file exists). Config via `TTS_*` env (repo path, serving host/port,
`TTS_IMAGE`, `TTS_AUTOSTART`, `TTS_CONFIG` for the profile dropdown's TOML path).
**Shares `tt console`'s lifecycle state machine**: Start/Stop/Restart shell out to
`systemctl --user <verb>` (no more child-process supervision — closing the panel doesn't
kill the agent), and status/pairing/serving/profile all come from a single poll of `tt
console --snapshot` (the same `BoxLifecycleSnapshot` JSON the TUI renders) — one source of
truth the panel and the TUI can never disagree about. **Connect row** (shown only while
serving): one-click **Open WebUI** (local `docker run` of `ghcr.io/open-webui/open-webui:main`
wired to the box's own `/v1` via `host.docker.internal`, then `xdg-open`; `TTS_OPENWEBUI_PORT`,
default 3000), **opencode** (writes a per-endpoint `opencode.json` under
`~/.local/share/tt-station/opencode/`, opens a terminal emulator running `opencode` — resolves
`x-terminal-emulator`/`gnome-terminal`(`--`)/`konsole`/`xterm`(`-e`)), plus **Copy /v1** /
**Open endpoint**. Endpoint+model come from the snapshot's agent-source `serving` entry (not the
unimplemented `/endpoint` route). Missing docker/opencode/terminal/xdg-open surface an inline
message, never a crash. Pure builders live in `box-panel/panel_launchers.py`
(`+ test_panel_launchers.py`, stdlib unittest); the panel is thin glue (worker thread +
`GLib.idle_add`). Ported from the macOS Connect launchers, local (no SSH) since the panel runs
on the box.

**Linux packaging (`debian/`, `build-deb.sh`, v0.10.0):** two Ubuntu `.deb`s, modeled on
tt-toplike (debhelper compat 13 + `dpkg-buildpackage`, vendored offline crates via `cargo
vendor` + `--frozen`; `vendor/`/`.cargo/config.toml` not committed). **`tt-station`** ships
`/usr/bin/tt`, `/usr/bin/tt-station-agentd`, and the systemd **user** unit at
`/usr/lib/systemd/user/tt-station-agentd.service` **installed but not enabled/started**
(`dh_installsystemduser --no-enable` — this box's debhelper lacks `dh_installsystemd --user`;
operator runs `systemctl --user enable --now`). **`tt-station-panel`** ships the GTK panel to
`/usr/share/tt-station-panel/` (incl. `panel_launchers.py` + `assets/tt-logo.png`), a
`/usr/bin/tt-station-panel` wrapper, a packaged `.desktop`, and hicolor icons (`Depends:
tt-station, python3, gir1.2-gtk-4.0, python3-gi`). `install_desktop_icon()` no-ops when packaged.
`tt-station` also ships the **polkit rule** for box power (`deploy/tt-station-power.rules` →
`/etc/polkit-1/rules.d/49-tt-station-power.rules`) as a **conffile** — debhelper auto-registers
`/etc` files as conffiles, so no `debian/*.conffiles` list is needed (a manual list double-registers).
Workspace version unified at 0.10.0 (`[workspace.package]`, `scripts/bump-version.sh`); CI
`release.yml` builds per-suite (noble/jammy) `.deb`s — a `v*` tag publishes a GitHub Release,
while a manual **`workflow_dispatch`** run uploads the same `.deb`s as downloadable Actions
artifacts (`tt-station-debs-<suite>`, 90d) with no Release (a pre-release/test-build flow;
design: `docs/superpowers/specs/2026-07-10-deb-prerelease-ci-design.md`). `ci.yml` enforces
version-consistency. `mock-box`/`libttstation` not packaged. Design/plan:
`docs/superpowers/{specs,plans}/2026-07-10-*`. **Deb install verified in a fresh QB2 container**
(2026-07-15, `tt-developer-image`'s golden `tenstorrent/qb2-env:latest`, Ubuntu 24.04/noble): the
core `tt-station` deb installs cleanly, `tt`/`tt-station-agentd` run, `tt power`/`tt wake` present,
the systemd user unit is valid, and the polkit rule lands at `/etc/polkit-1/rules.d/` (root-readable,
correct). That test **caught + fixed a duplicate-conffile defect** — `debian/tt-station.conffiles`
manually listed the /etc rule that debhelper already auto-registers, so dpkg registered it twice;
removed the file (fix on `main`, NOT in the released v0.10.0 deb — needs a v0.10.1). The GTK panel
deb correctly refuses to configure without `gir1.2-gtk-4.0` (expected on a headless box). Still
owner-gated: install on a **real** box + GTK click-through, and exercise the destructive power ops.

**macOS app (`macos/TTStation`, v0.5.0 — native control room):** window-first veneer over
`tt --json` with a fast MenuBarExtra popover for glance + quick actions (the menu-bar icon
badges + rows highlight currently-serving models). The resizable window is a card-based
control room: **box header** with a detected **device-mesh badge** (`P300X2`) and a **power menu**
(`PowerMenuView` — reset-chips/suspend/reboot/shutdown with a confirm sheet + expected-disconnect
state; also mirrored in the MenuBarExtra popover, backed by `PowerControls`/`TTClient.power`); a **live
device strip** (per-device temp/power/aiclk streamed from the agent's `/telemetry` WebSocket
— the one read-only Swift I/O path); a **read-only Config card** (active/available profiles,
backend, serving endpoint — from `tt config`); a **3-tier hardware-aware model browser**
(Runs on this box / Experimental / Needs other hardware) built from `tt catalog` — which
merges the box's live `/models` with the public compatibility catalog (24h-cached in `tt`)
classified for the box mesh; the Experimental/other tiers carry "bring these up with the
tools" messaging that links to the workbench; **fast Connect** (Open WebUI / opencode that
`brew install` missing deps as needed); and an elevated **workbench** (Terminal / tt-toplike /
VS Code with the `Tenstorrent.tt-vscode-toolkit` extension). TT brand theme (teal `#4FD1C5`).
Mesh detection covers **P150 x1–x4** + P300/N300/T3K/GALAXY.
The device mesh is sourced from Rust: the agent detects it once at startup and reports it in
`/status` + the mDNS TXT record (so `tt --json discover`/`status` carry `device_mesh`). See
`macos/README.md`.

**Release installer (v0.9.0):** the app now bundles the `tt` CLI at `Contents/Resources/bin/tt`;
ships as an arm64 DMG built by `macos/make-release.sh` (local source of truth, also called by
`.github/workflows/macos-release.yml` on `v*` tags); the first-run prompt installs a
`~/.local/bin/tt` symlink with foreign-`tt` collision handling (leaves a foreign `tt` alone,
offers `tt-station`); ad-hoc signed (no notarization) so users run
`xattr -dr com.apple.quarantine /Applications/TTStation.app` once. See
`docs/superpowers/specs/2026-07-09-macos-release-installer-design.md`.

**Keyless SSH on pairing (v0.4.0):** the workbench launchers SSH as **`ttuser`** (QuietBox 2
default, override via the `tt.sshUser` UserDefault / agent `--ssh-user`). The pair flow has an
opt-in toggle (default on) that, on a successful pair, installs this Mac's SSH **public** key on
the box as `ttuser` — the PIN handshake is the trust anchor. Flow: `tt ssh-authorize` (reads or
generates `~/.ssh/id_ed25519`, never transmits the private key) → authed `POST /ssh/authorize`
on the agent → appended to the run-user's `~/.ssh/authorized_keys` (validated public-key-only,
idempotent, tagged `ttstation:<host>:<date>`; `DELETE`/`tt ssh-authorize --revoke` removes it).
The SSH step is non-fatal to pairing. The `authkeys` module hardened against label
newline-injection + unanchored-revoke; mock-box serves `/ssh/authorize` against a temp file.

**mock-box (`crates/mock-box`):** dev fixture — mDNS advertise + `serve` faking the control
API + `/v1` (used by the CLI e2e, no hardware).

**Docs:** `docs/reference/tt-inference-server-docker.md` (the real run.py launch),
`docs/reference/tt-console.md` (the `tt console` operator TUI: systemd unit model,
keybindings, `--snapshot` JSON contract, configurable tool names, reset/pair-localhost),
`docs/reference/logs.md` (the `/logs`/`/logs/stream` contract, `tt logs`, the container-log
visibility gap this closes), `docs/tt-studio-integration.md` (verdict: **no clean cache-share without modifying tt-studio**;
`/serving` makes tt-studio's models visible), `docs/superpowers/{specs,plans}` (PoC, macOS
menubar, connect launchers), `docs/superpowers/cleanup-analysis.md`.

---

## Activate the live box (:8765 agent, via the panel)

The panel launches `./target/release/tt-station-agentd`. To pick up the latest agent
routes/fixes: **hit Restart on the panel** (or relaunch it). Box-local config it uses:
`--backend runpy --tt-inference-repo ~/code/tt-inference-server --serving-host qb2-lab.local
--serving-port 8003 --serving-image ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0-80180b9-7678b70`
(device auto-detects; pass `TTS_IMAGE` to the panel). Telemetry/serving/status are unauthed;
`run`/`stop`/`endpoint`/`reset` need a pairing (code shows on the panel). Restarting the
agent is fine now — **persist-tokens** keeps the Mac paired across restarts.

## Run / test (Rust, on this box)
- `cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings`.
- CLI e2e (no hardware): `cargo test -p tt --test e2e_mock -- --ignored`.
- Live remote-telemetry smoke: start the agent, `python3` WebSocket read of `ws://…/telemetry`.

## How this project is built
Subagent-driven: fresh implementer + independent reviewer per change, TDD, frequent
commits, honest reports. Blend sources (glean + repos + docs). The git history is the
detailed log; this file is the current-state map.

## Known follow-ups (not blocking)
- **Box power controls: owner testing on real hardware.** Box-side is built/installed/verified
  on this box (route responds, MAC advertised, CLI present), but the destructive machine ops
  (`suspend`/`reboot`/`shutdown`) and the polkit rule that authorizes them have NOT been exercised
  end to end — do a `.deb` install that lands `/etc/polkit-1/rules.d/` and confirm each op
  actually runs (and `reset-chips` keeps pairing). Wake-on-LAN needs WoL enabled in the box
  BIOS/NIC and a Mac on the same L2 broadcast domain. macOS `PowerMenuView` needs a click-through.
- **Panel Connect launchers: finish the click-through WITH a serving model.** The GTK panel
  now launches cleanly on this box (2026-07-10, via `/usr/bin/python3` on `DISPLAY=:0`):
  `panel_launchers` imports, the Connect row is built, and `_refresh_connect` gating runs
  error-free across poll cycles — with nothing serving, the Connect row is correctly HIDDEN.
  Still unverified: the actual **button behavior** (Open WebUI `docker run` → browser, opencode
  → terminal, copy/open) which needs a model serving. Do that next time a model is up.
- **Wrapper needs `/usr/bin/python3` — fixed on main, ships v0.9.1 (NOT in the released v0.9.0
  `.deb`).** The tt-smi venv (`~/.tenstorrent-venv`) is first on this box's PATH and lacks
  `python3-gi`, so the v0.9.0 packaged wrapper's bare `python3` would `ModuleNotFoundError` from
  a venv-activated shell (fine from the desktop menu's clean PATH). Wrapper now pins
  `/usr/bin/python3`. To release: `scripts/bump-version.sh 0.9.1`, bump `MARKETING_VERSION` to
  match, `git tag v0.9.1 && git push origin v0.9.1`.
- **v0.9.0 is released** (2026-07-10): the GitHub Release carries all four `.deb`s
  (`tt-station`/`tt-station-panel` × noble/jammy) + `TTStation-0.9.0-arm64.dmg`. Collaborator
  first-run guide: `TESTING.md`. The manual `workflow_dispatch` `.deb` artifact flow is
  validated (a manual Release run succeeded). Still owner-gated: install the `.deb`s on a box +
  the DMG on a Mac and run the full discover → pair → run → connect loop.
- tt-studio: real cache-sharing needs tt-studio running + config changes (see the doc).
- Agent: wrap `advertise_status` mDNS send in `spawn_blocking` (async-hygiene nit).
- macOS: build-verify everything above; wire discovery-by-name into the app's `/remote`
  story if desired; App Intents / deep links (deferred in the connect spec).
- Log viewing: an external-container (`docker logs`) fallback for containers with no
  `workflow_logs/` file; a structured serve-phase field in `/status` (so "downloading
  weights" vs "container crashed" are distinguishable without reading logs); a macOS
  "View logs" button (`docs/reference/logs.md` has the pointer); `tt console`'s log pane
  is auto-tail-only — manual scroll is unimplemented; the console/snapshot log fetch's
  `tail=20` is hardcoded, not configurable.

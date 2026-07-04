# tt-station — project CLAUDE.md

Plug-and-play Tenstorrent from a Mac: discover a QuietBox on the LAN, pair once,
`tt run <model>`, get one OpenAI-compatible `/v1`. **No llama.cpp** — usability rides
on the `/v1` that `tt-inference-server` (vLLM, via `run.py`) exposes.

Repo: github.com/tsingletaryTT/tt-station (private). Work happens on `main`.
This box (`tsingletaryTT-quietbox`) IS a real QuietBox: 4× Blackhole (`p300c`),
`tt-smi`, docker, `~/code/tt-inference-server`. Real serving has been proven here.

---

## ▶ PICK UP HERE — especially on the Mac

**The entire macOS app (`macos/TTStation`, now v0.2.0) was authored on a Linux box
with NO Swift toolchain — it is committed but has NEVER been compiled.** First thing
on the Mac: build + verify it, fix any compile slips, then it's ready.

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
  `GET /telemetry` (**WebSocket**, unauthed — streams `tt-smi -s` for remote tt-toplike),
  `GET /serving` (unauthed — every running `tt-inference-server` `/v1`, `source: agent|external`).
- mDNS `_tenstorrent._tcp` status re-published on run/stop; graceful shutdown unregisters.

**CLI (`crates/tt`):** `discover` (`--host`/`--no-mdns`), `pair`/`pair-init`/`pair-complete`,
`run`, `stop`, `status` (unauthed), `endpoint`, `models`, `serving`, `reset`. Global `--json`.
Tokens in macOS Keychain / file store. Respects `TT_CONFIG_DIR`.

**Box panel (`box-panel/tt-station-panel.py`, GTK4):** the box's own screen — Start/Stop/
Restart/Reset the agent, **live 6-digit pairing code** (with TTL), status/endpoint. Config
via `TTS_*` env (repo path, serving host/port, `TTS_IMAGE`, `TTS_AUTOSTART`).

**macOS app (`macos/TTStation`, v0.2.0):** MenuBarExtra veneer over `tt --json` —
discover/pair, **smart-default model** (remembers last-run per box; else prefers chat-tuned
~7–9B), **searchable family-grouped model browser**, **HIG run/serving states**, endpoint
copy, **`/serving` list** (agent + external/tt-studio badge), **Connect launchers** (one-click
Open WebUI via `uvx` + opencode in Terminal). See `macos/README.md`.

**mock-box (`crates/mock-box`):** dev fixture — mDNS advertise + `serve` faking the control
API + `/v1` (used by the CLI e2e, no hardware).

**Docs:** `docs/reference/tt-inference-server-docker.md` (the real run.py launch),
`docs/tt-studio-integration.md` (verdict: **no clean cache-share without modifying tt-studio**;
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
- tt-studio: real cache-sharing needs tt-studio running + config changes (see the doc).
- Agent: wrap `advertise_status` mDNS send in `spawn_blocking` (async-hygiene nit).
- macOS: build-verify everything above; wire discovery-by-name into the app's `/remote`
  story if desired; App Intents / deep links (deferred in the connect spec).

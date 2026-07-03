# tt-station — project CLAUDE.md

Plug-and-play Tenstorrent from a Mac: discover a QuietBox on the LAN like an AirPlay
device, pair once, `tt run <model>`, get one OpenAI-compatible `/v1` endpoint. **No
llama.cpp** — usability rides on the `/v1` that `tt-inference-server` (vLLM) exposes.

Repo: github.com/tsingletaryTT/tt-station (private). Work happens on `main`.

## What happened (session log)

- **Origin:** blue-sky notes (`CONTEXT.md`) from a Claude Cowork session → asked to turn
  them into an achievable plan and try it out, without needing a llama.cpp integration.
- **Process:** brainstorming → spec (`docs/superpowers/specs/2026-07-02-tt-station-poc-design.md`)
  → plan (`docs/superpowers/plans/2026-07-02-tt-station-poc.md`) → subagent-driven
  execution (fresh implementer + independent reviewer per task, fix loops on findings).
- **Key decisions (with owner):** Rust core; SwiftUI menu-bar shell deferred (owner-gated,
  needs macOS); serving behind a `ServingBackend` trait so we can swap Docker↔dstack↔run.py
  without the Mac noticing; cloud-burst deferred; discovery is a 3-provider interface
  (mDNS / manual / Tailscale) because corp LANs often block mDNS.
- **Built (Tasks 0–12):** a 4-crate workspace, all TDD'd, then merged to `main`.
- **Notable moment — real launch path:** the guessed `docker run` was wrong. The operator's
  own scripts (`~/code/tt-local-generator/bin/start_*.sh`) revealed LLMs are launched via
  `tt-inference-server/run.py`, not raw docker. Added **`RunPyBackend` as the default**
  (raw `DockerBackend` kept as a fallback behind the trait). Ground truth captured in
  `docs/reference/tt-inference-server-docker.md`.
- **Notable bug caught in review:** `MODEL_SOURCE` was set via `std::env::set_var` — a data
  race under concurrent `/run` on the multithreaded tokio runtime (unsafe in Rust 2024).
  Fixed by passing env on the child `Command` (`CommandRunner::run_in_dir_with_env`).

## Layout

- `crates/libttstation` — model/TXT, discovery (trait + mDNS + manual + `aggregate`, shared
  `SERVICE_TYPE`), secrets (file + macOS Keychain), pairing client, `AgentClient`.
- `crates/tt-station-agentd` — box-side axum daemon: `/status`, 6-digit pairing→bearer token
  (expiry + `MAX_PAIR_ATTEMPTS` lockout), `ServingBackend` {`RunPyBackend` default,
  `DockerBackend`, `DstackBackend` stub}, bearer-authed `/run /stop /endpoint`
  (backend calls via `spawn_blocking`).
- `crates/mock-box` — dev fixture: mDNS advertiser + `serve` (fakes control API + canned `/v1`).
- `crates/tt` — CLI: `discover/pair/run/stop/status/endpoint`, `--json`, prints
  `export OPENAI_BASE_URL=…`. Respects `TT_CONFIG_DIR` for the token store.

## Verified facts about the real box (from operator scripts, 2026-07-03)

- LLM launch = `run.py --workflow server --engine vllm --docker-server --override-docker-image
  <ghcr vllm tag> --no-auth --service-port <p> --host-hf-cache <cache> --tt-device <dev>`.
- **This box = `--tt-device p300x2`** (4× p300c). `p150x4` = the *other* BH QuietBox.
- LLM image: `ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64`
  tags in use: `0.14.0-80180b9-7678b70` (pref), `0.11.1-bac8b34-7c6685a`. (`tt-media-inference-server`
  is a *different* server for images/video.)
- Models = spec names (`Qwen3-8B`, `Llama-3.1-8B-Instruct`, …), not raw HF ids, for vLLM.
- Readiness `GET /health`; chat `POST /v1/chat/completions`. `--no-auth` for local dev.
- run.py repo: prefer `<checkout>/vendor/tt-inference-server`, else `$HOME/code/tt-inference-server`.

## Run / test

- Full suite: `cargo test --workspace` (70 passing). Lint: `cargo clippy --workspace --all-targets -- -D warnings`.
- End-to-end (no hardware): `cargo test -p tt --test e2e_mock -- --ignored` (self-spawns mock-box;
  proves discover→pair→run→endpoint→completion).
- mDNS integration: run `mock-box advertise …` then
  `TT_MOCK_NAME=<n> cargo test -p libttstation --test mdns_integration -- --ignored`.
- SDD progress ledger + per-task reports live in `.superpowers/sdd/` (gitignored).

## Next steps (owner-gated)

1. **Real M2 on the QB2** — build `tt-station-agentd` on the box, run with
   `--backend runpy --tt-inference-repo <path> --tt-device p300x2 --serving-image <tag>
   --service-port <p>`; from the Mac: `tt discover`/`pair`/`run`/`endpoint`, then curl `/v1`.
2. **macOS SwiftUI `MenuBarExtra`** shell over `tt --json` (Task 14; needs a Mac).
3. **Real `DstackBackend`** + cloud-burst router (separate spec).

## Deferred tickets (post-PoC quality)

- `ServingStatus` serde derive produces a non-canonical wire form (latent footgun) — add
  custom Serialize/Deserialize or drop the derive (CLI works around it with `DiscoveredBox`).
- pair_complete collapses all non-2xx to one message; hex token vs base64url; per-IP (not
  global) attempt cap; discover always waits full mDNS timeout; e2e uses fixed port 18899.

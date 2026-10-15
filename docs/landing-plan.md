# Landing plan: `jberkowitz/mobile-voice` → `main`

`jberkowitz/mobile-voice` (at `8d1725c`) is **~94K changed lines over `origin/main` (`8daa46e`,
v0.11.0)**: 299 files, +93,795 / −642. Most of it arrived as three squash commits
(`6f39e94` agentd box services, `156b01c` desktop app, `86347d7` iOS app), followed by
merges of a few feature branches (`device-sessions`, `security-fixes`, `agentd-hardening`,
`code-page`, `reliability-eval`, ...). Nobody can review that as one PR. This plan splits it
into ~17 PRs of 2–8K lines each, in dependency order, by module area.

Sizes below are added + removed lines from `git diff --numstat 8daa46e..8d1725c`, grouped by
path. Each PR carries its own tests and its own `docs/reference/*.md` page.

## How to cut each PR

The squash commits can't be cherry-picked apart, so each PR is built by path:

1. `git switch -c <owner>/<area> origin/main` (or the previous PR's branch while it is in review).
2. `git checkout jberkowitz/mobile-voice -- <the PR's files>`.
3. Hand-split the two **hub files** (4.1K lines between them): `crates/tt-station-agentd/src/routes.rs`
   (each PR adds only its own `.route(...)` lines, handlers and `AppState` fields) and `src/main.rs`
   (its own flags and startup tasks), plus the `mod` lines in `src/lib.rs`. `git add -p` works; so
   does checking out the final file and deleting what belongs to later PRs.
4. `cargo build`/`cargo test` with the hardware stubs (see `CLAUDE.md`, "Run / test") must pass at
   every step, so a later PR's routes can't be left referenced.
5. When the last PR merges, `git diff origin/main jberkowitz/mobile-voice` should be empty
   (apart from anything deliberately dropped). That is the completeness check.

The hub-file split is the main cost; PR 1 should land the `routes.rs` scaffolding (`AppState`
additions, auth extractors, `ws_token`, `LocalOrPaired`) so later PRs only append routes.

## Gates for every PR

- CI green: version check, Rust build + tests (stubbed hardware), docs-route check, desktop
  typecheck/test/build, cargo-deny licenses/bans/sources.
- Clippy and `cargo fmt` are report-only today. **Don't** fold the repo-wide reformat into these
  PRs; it lands once, after the last one (PR 13).
- One reviewer who owns the area, plus a second for PRs marked *security review*.
- Docs: the PR's `docs/reference` page is included and `scripts/check-doc-routes.py` passes.
- No live-box step is required to merge, but PRs marked *box check* list a manual check on a
  QuietBox in the PR description (run by the owner, not CI).

## The PRs

| # | PR | Paths (agentd = `crates/tt-station-agentd`) | Size | Depends on | Extra gates | Owner |
|---|---|---|---|---|---|---|
| 0 | **Engineering hygiene** (this branch, `jberkowitz/eng-hygiene`) | CI jobs, `deny.toml`, `NOTICE`, `THIRD_PARTY_LICENSES.md`, `scripts/`, `.gitattributes`, doc fixes, crate `license` fields | ~1.5K (≈1K generated) | none (can go to `main` first) | legal review of the flagged licenses | _TBD_ |
| 1 | **Auth and pairing foundation**: per-device sessions, hashed tokens, TLS, LAN Host checks, QR pairing, WS token transport | agentd `sessions.rs`, `tls.rs`, `web.rs`, `pairqr.rs`, `pairing.rs`, `config.rs`, `lib.rs`; `routes.rs` scaffolding; `crates/libttstation`, `crates/tt-station`, `crates/mock-box`; `Cargo.toml`/`Cargo.lock`; `deploy/`; tests `ws_auth`, `devices`, `pair_offer`, `token_persistence`, `pairing`, `config`; `docs/security-faq.md`, `docs/reference/box-hardening.md` | ~3.1K + hub share | 0 | *security review*, *box check* (pair a phone over HTTPS) | _TBD_ |
| 2a | **Serving lifecycle and model management**: `/serve`, `tt` CLI models and pulls, downloads, native vLLM, discovery, metrics history, prefs/favorites | agentd `serving/`, `ttcli.rs`, `tttools.rs`, `download.rs`, `native_vllm.rs`, `discover.rs`, `history.rs`, `inference.rs`, `telemetry.rs`, `gozer.rs`, `prefs.rs`, `favorites.rs`; tests `serve`, `runpy`, `ttcli`, `metrics`, `serving`, `gozer`, `leases`, `logs*`, `reset`; `docs/reference/tt-cli.md`, `tt-inference-server-docker.md` | ~5.4K | 1 | *box check* (serve and stop a model) | _TBD_ |
| 2b | **Box operations**: co-serve, modes, idle sleep, doctor, updates, power | agentd `coserve.rs`, `modes.rs`, `idle.rs`, `doctor.rs`, `updates.rs`, `power.rs`; tests `coserve`, `modes`; `docs/reference/box-care.md`, `alerts-and-power.md` (power half) | ~3.9K | 2a | *security review* (runs privileged commands), *box check* | _TBD_ |
| 3 | **OpenAI/Anthropic gateway**: `/v1/*`, app keys, `/connect`, `/bench`, creations | agentd `gateway.rs`, `anthropic.rs`, `bench.rs`, `creations.rs`; tests `gateway`, `anthropic`; `docs/reference/gateway.md` | ~3.3K | 2a | *security review* (key handling) | _TBD_ |
| 4 | **Assistant core**: chat, tools, confirmations, memories, documents, embeddings, people | agentd `assistant.rs`, `memory.rs`, `docs.rs`, `embeddings.rs`, `people.rs`, `approvals.rs`, `skills.rs`; tests `assistant`, `people`; `docs/reference/assistant.md`, `household-schedules-remote.md` (people) | ~6.9K, the largest backend PR; split `assistant.rs` tools out if review stalls | 2a, 3 | *security review* (`read_webpage`, tool approvals) | _TBD_ |
| 5 | **Household and voice**: notifications/APNs, schedules, transcription jobs, recorder, voice, Tori | agentd `notify.rs`, `apns.rs`, `schedules.rs`, `jobs.rs`, `recorder.rs`, `voice.rs`, `tori.rs`; tests `notify`, `jobs`; `docs/reference/jobs.md`, `alerts-and-power.md`, `voice-and-desktop.md` (voice half) | ~5.1K | 4 | *box check* (voice round trip) | _TBD_ |
| 6 | **Apps and desktop backend**: app catalogue (Docker apps), desktop launcher, `/app` UI serving, Hermes, community models, library, tt-toplike terminal | agentd `apps.rs`, `desktop.rs`, `app_ui.rs`, `hermes.rs`, `community.rs`, `library.rs`, `toplike_term.rs` | ~4.3K | 2a, 4 | *security review* (Docker/apps, terminal); **legal sign-off on n8n** before the catalogue ships it | _TBD_ |
| 7 | **Research, routines, evals** | agentd `research.rs`, `routines.rs`, `evals.rs`, `reliability.rs`, `examples/agent_reliability.rs`; `docs/notes/agent-reliability-*` (the JSON is `linguist-generated`) | ~4.6K + 5.5K JSON | 4 | none | _TBD_ |
| 8a | **Orchard and Lab** | agentd `orchard.rs`, `orchard_progress.rs`, `orchard_lab.rs`, `orchard_bridge.py`, `lab.rs`, `lease_run.py`; tests `orchard` + `tests/fixtures/fake-orchard`; `docs/reference/orchard.md`, `lab.md`, `chip-leasing.md` | ~4.6K | 2b | *box check* | _TBD_ |
| 8b | **Model bring-up agents** (Claude/Codex runs, approvals) | agentd `bringup.rs`, `claude_bringup.py`, `codex_bringup.py`, `permission_mcp.py` | ~2.7K | 8a | *security review* (agent shell access, path containment) | _TBD_ |
| 8c | **Code workspace** (sessions, safe git, coding terminals, code speed) | agentd `workspace.rs`, `code_session.py`, `code_holder.py`, `safegit.rs`, `coding.rs`, `codespeed.rs` | ~3.6K | 8b | *security review* (terminal and git on the box) | _TBD_ |
| 9 | **Desktop app shell**: Electron main/preload, build, Vesper theme, API client, Home/Settings/Connect, tests | `desktop/electron`, `desktop/package*.json`, `desktop/src/{App,api,ui}.tsx`, `app.css`, shell pages, `desktop/test`, `desktop/mac`, `desktop/gnome-extension` | ~9K (3.9K is `package-lock.json`) | 1, 2a | desktop CI job; *security review* (CSP, bridge) | _TBD_ |
| 10 | **Desktop feature pages**, one PR per backend area or grouped: Chat/Tori/Voice (4–5), Models/Studio (2a, 3), Orchard/Bringup (8a–b), Code/Terminals (8c), Research/Routines/Evals (7) | `desktop/src/pages/*`, `desktop/src/*.tsx` | ~10.5K total; 1.5–3K each | 9 + the backend PR | desktop CI job | _TBD_ |
| 11 | **iOS app core**: project, API client, pairing, discovery, session, brand + fonts, dashboard | `ios/project.yml`, `ios/Sources/{AgentAPI,BoxSession,Discovery,PairSheet,Brand,Telemetry,Dashboard*,BoxesView}.swift`, `ios/Resources`, `ios/Assets.xcassets`, `ios/Info.plist` | ~6.5K | 1, 2a | iOS CI job green at least once (currently allowed to fail); fix `deploymentTarget` nesting in `project.yml` | _TBD_ |
| 12 | **iOS feature screens**: chat/voice, models, create, Orchard, jobs, alerts (APNs), Live Activity | remaining `ios/Sources`, `ios/ActivityExtension`, `ios/Shared` | ~9K; split by backend area like PR 10 | 11 + the backend PR | iOS CI job; *box check* on a device | _TBD_ |
| 13 | **Cleanup**: repo-wide `cargo fmt`, clippy to zero, then make both CI gates blocking (`-D warnings`, drop `continue-on-error`) | whole tree | mechanical | all of the above | none beyond CI | _TBD_ |

Remaining docs (`docs/presentations/`, `docs/tasks-framework.md`, `docs/notes/*.md`) go with the
PR they describe or in a final docs PR. `CLAUDE.md` is updated in each PR for its own area.

## Order and parallelism

```
0 ─┬─ 1 ─ 2a ─┬─ 2b ─ 8a ─ 8b ─ 8c
   │          ├─ 3 ─ 4 ─┬─ 5
   │          │         ├─ 6
   │          │         └─ 7
   │          └─ 9 ─ 10 (page PRs follow their backend PR)
   │                └─ 11 ─ 12
   └─────────────────────────────────── 13 (last)
```

After PR 2a, three tracks can run in parallel (ops/Orchard, assistant/gateway, clients). The
hub-file conflicts among them are append-only route lines and resolve mechanically.

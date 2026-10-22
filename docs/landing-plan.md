# Landing plan: reviewing `jberkowitz/mobile-voice`

`jberkowitz/mobile-voice` is about 152K changed lines over `origin/main` (`8daa46e`,
v0.11.0). It branches from `tsingletaryTT/tt-station` and is shared as a branch for review and
cherry-picking. Nobody can review that as one diff, so it is cut into **16 stacked review branches**,
`review/00-…` to `review/15-…`, one area each.

## How the stack works

- Each branch is the previous one plus one area's files, taken verbatim from
  `jberkowitz/mobile-voice`. The last branch's tree is identical to `jberkowitz/mobile-voice`,
  so approving every slice approves the whole branch, and nothing is left out.
- Review a slice as its diff against the previous slice:
  `git diff review/04-box-care review/05-assistant-gateway`. Opened as pull requests, each
  slice's base is the previous slice.
- `scripts/review-slices.py` holds the assignment of every file to a slice.
  - `--list` prints sizes and fails if any changed file has no slice.
  - `--build <worktree>` recreates the branches after `jberkowitz/mobile-voice` moves.
  - Change the plan there, not by hand.

## What builds where

The agent's modules depend on one another through shared state, routing and startup, so it
can't be built a piece at a time without a large restructuring. Each review track is
therefore checked where it becomes complete, and lands as a unit once all its slices are
approved.

| Track | Slices | Builds and passes tests at | Land as |
|---|---|---|---|
| Build and CI | 00 | read-only (it references the later code) | with the agent |
| CLI and client library | 01 | `01` (`cargo test -p tt-station -p libttstation`) | with the agent (the agent uses the library) |
| Box agent | 02–10 | `10`: full `cargo build`/`cargo test` with the hardware stubs, `clippy`, `fmt` | one merge of `review/10-code` |
| Desktop app | 11–12 | `12`: `npm run typecheck`, `npm test`, `npm run build` | one merge, after the agent |
| iPhone app | 13–14 | `14`: Xcode build plus the iOS tests (CI `ios` job, needs a Mac) | one merge, after the agent |
| Docs | 15 | nothing to build | last |

## The slices

Lines are added plus removed, tests and docs included.

| # | Branch | Lines | Look at | Extra review |
|---|---|---:|---|---|
| 00 | `review/00-hygiene` | 3.8K | CI jobs, `.deb` packaging, `deploy/` (firewall, SSH hardening, bring-up containment), release signing scripts, licenses | security (deploy scripts run as root) |
| 01 | `review/01-cli` | 2.5K | CLI over HTTPS with the box's certificate pinned; client library | security (certificate pinning) |
| 02 | `review/02-agent-core` | 16.7K | The wiring (`main.rs`, `state.rs`, `router.rs`) plus sign-in, pairing, TLS, sessions, owner and member roles, audit log, app keys, the mock box used by tests | **security**. The largest slice: about half is the wiring every later slice plugs into |
| 03 | `review/03-serving-chips` | 14.6K | Starting and stopping models, chip leasing through gozer, liveness, telemetry, model modes | box check: start and stop a model |
| 04 | `review/04-box-care` | 8.4K | Power, sleep and Wake-on-LAN, signed updates, export and import, doctor | security (privileged commands, update signatures) |
| 05 | `review/05-assistant-gateway` | 13.3K | OpenAI and Anthropic `/v1` gateway, assistant tools and approvals, memories, documents, library | security (web content steering tools, key handling) |
| 06 | `review/06-household` | 8.5K | Notifications and APNs, voice, meeting recorder, transcription jobs, schedules | privacy (recording indicator, retention) |
| 07 | `review/07-research-routines` | 13.6K | Deep research, routines, model check-ups and reliability evals | — |
| 08 | `review/08-apps` | 7.0K | Store apps, desktop launches, community models | security, and **legal sign-off on the app catalog** (all open source now; n8n, the one that wasn't, is out of the catalogue) |
| 09 | `review/09-bringup-lab` | 12.4K | Orchard, Lab, model bring-up agents and their Landlock sandbox | security (agents with shell access) |
| 10 | `review/10-code` | 10.3K | Code workspace: sandboxed sessions, safe git, model picker, speed | security (sandbox escape paths). **The agent builds from here** |
| 11 | `review/11-desktop-shell` | 15.1K | Electron main process and preload, token store, API client, shared UI (3.9K is `package-lock.json`) | security (window navigation, IPC) |
| 12 | `review/12-desktop-pages` | 9.3K | Every page: Chat, Code, Models, Studio, Research, Settings… | design |
| 13 | `review/13-ios-core` | 10.3K | API client, discovery, pairing, box control, power, settings, iOS tests | box check on a phone |
| 14 | `review/14-ios-features` | 8.6K | Chat and voice, studios, research, Lab, Orchard, notification extension, Live Activity | — |
| 15 | `review/15-docs` | 0.9K | Presentations, notes and the 2026-10-15 Code serving benchmark summary (its raw logs were dropped) | — |

## Gates for every slice

- One reviewer who owns the area, plus a second for slices marked security or privacy.
- Comments are fixed on `jberkowitz/mobile-voice`, then the stack is rebuilt with
  `scripts/review-slices.py --build`, so the slices never drift from the real branch.
- At each track's build point, CI must be green:
  - Rust build and tests with the hardware stubs, `clippy -D warnings`, `fmt`
  - the docs-route check and `cargo-deny`
  - the Python helper tests
  - desktop typecheck, tests and build
  - the iOS build
- Box checks are run by the owner on a QuietBox and noted in the review.

## Before any of it merges

- Move to an org-owned repository and release channel. Releases and updates point at the
  current personal repo.
- Name who holds the release signing key; no key is pinned yet.
- Get legal sign-off on the store app catalog.

## Cherry-picking onto `main`

The slices are for reading. To land code piece by piece instead, take whole areas from the
branch by path (`git checkout jberkowitz/mobile-voice -- <paths>`; each slice's paths are in
`scripts/review-slices.py`), not individual commits: much of the history is squashes and
merges that don't split cleanly. Two things keep that from breaking the build:

- The agent's areas (slices 02–10) share `state.rs`, `router.rs` and `main.rs`, which wire
  every area in. Taking only some areas means trimming those three to match; the agent builds
  and passes its tests only with all of them.
- The CLI and client library (01), the desktop app (11–12) and the iPhone app (13–14) stand
  on their own, but the apps need a box running this branch's agent: older agents don't serve
  HTTPS on 8766, QR pairing or the version check the apps read.

Versions: this branch is 0.12.0 and `origin/experiments/egpu` is 0.13.0. Whichever lands second
takes the next number (`scripts/bump-version.sh`) and adds its `debian/changelog` entry on top,
so the packaged version never goes backwards on a box that installed the other.

`origin/experiments/egpu` touches 12 of the same files, so expect conflicts there:
`Cargo.toml`, `Cargo.lock`, `CLAUDE.md`, `.gitignore`, `debian/changelog`,
`box-panel/tt-station-panel.py`, `crates/libttstation/{Cargo.toml,src/lib.rs,src/secrets.rs}`,
`crates/tt-station/{Cargo.toml,src/main.rs}` and `crates/tt-station-agentd/src/device.rs`.

## Handing more to tt-cli

The agent already uses the official CLI where it can, and works around it where it can't yet.
Where it works around a gap, the gap is a candidate for tt-cli:

| Today | Where | What tt-cli would need |
|---|---|---|
| Catalog and downloads go through `tt --json model list` and `tt --json model pull` | `ttcli.rs` | Download progress: `tt model pull` reports nothing until it exits, so the agent measures the HF cache folder instead (`ttcli.rs` header) |
| Serving runs `tt-inference-server/run.py` directly by default; `tt serve` is an option (`--serve-with tt`) | `serving/runpy.rs` | A way to pass chip ids or a gozer lease: `tt serve` can't be confined to leased chips (`serving/runpy.rs`, the `--serve-with tt` lease check) |
| The agent maps `tt-smi -s` board counts to a mesh (`p300x2`) itself, because `run.py`'s auto-detect fails on 4 × p300c | `device.rs` | A hardware query (boards, chips, mesh) the agent and `run.py` could share |
| Open WebUI and AnythingLLM start through `tt launch` | `tttools.rs`, `apps.rs` | Nothing; the other store apps are the agent's own Docker apps (`apps.rs`) |
| Community packages, native vLLM bundles and Orchard use their own tools (`tt-model`, `tt-kernel`, `tt-orchard`) | `community.rs`, `native_vllm.rs`, `orchard.rs` | Decide which of these tt-cli should front |

## Toward TT-Studio

`docs/tt-studio-integration.md` (July) studied how the two fit on one box. Where that stands
with this branch:

- **TT-Studio's models already show up.** `GET /serving` (`serving/discovery.rs`) finds every
  running tt-inference-server `/v1`, whoever started it, marked `source: external` when TT
  Station didn't, and the apps list them.
- **The chips are the conflict.** TT Station takes a gozer lease for every workload (model,
  Lab, bring-up, chip resets: `docs/reference/chip-leasing.md`); TT-Studio deploys with
  `run.py` and Docker and takes none, so either can start a model on chips the other holds.
  Fix before running both on one box: TT-Studio leases through gozer, or both go through
  tt-cli once it can serve on leased chips (see the table above).
- **TT-Studio can use a TT Station box as its model.** The gateway serves OpenAI's API over
  HTTPS on 8766 with app keys made in Connect (`docs/reference/gateway.md`), which is what
  TT-Studio's remote-endpoint mode (`CLOUD_*`, `VITE_ENABLE_DEPLOYED=true`) expects; the
  certificate is the box's own, so that client has to trust or pin it.
- **Shared weights, not shared compiles.** Both use `~/.cache/huggingface` when TT-Studio's
  `HOST_HF_HOME` points there; the tt-metal kernel caches stay separate.

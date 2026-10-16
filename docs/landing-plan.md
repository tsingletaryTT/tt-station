# Landing plan: reviewing `jberkowitz/mobile-voice`

`jberkowitz/mobile-voice` is about 167K changed lines over `origin/main` (`8daa46e`,
v0.11.0). It lives in `tsingletaryTT/tt-station`, a personal repository, and has not been
pushed. Nobody can review that as one diff, so it is cut into **17 stacked review branches**,
`review/00-…` to `review/16-…`, one area each.

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
| CLI and client library | 01 | `01` (`cargo build -p tt-station -p mock-box`) | with the agent (the agent uses the library) |
| Box agent | 02–10 | `10`: full `cargo build`/`cargo test` with the hardware stubs, `clippy`, `fmt` | one merge of `review/10-code` |
| Desktop app | 11–12 | `12`: `npm run typecheck`, `npm test`, `npm run build` | one merge, after the agent |
| iPhone app | 13–14 | `14`: Xcode build plus the iOS tests (CI `ios` job, needs a Mac) | one merge, after the agent |
| Docs and data | 15–16 | nothing to build | last |

## The slices

Lines are added plus removed, tests and docs included.

| # | Branch | Lines | Look at | Extra review |
|---|---|---:|---|---|
| 00 | `review/00-hygiene` | 3.2K | CI jobs, `.deb` packaging, `deploy/` (firewall, SSH hardening, bring-up containment), release signing scripts, licenses | security (deploy scripts run as root) |
| 01 | `review/01-cli` | 2.7K | CLI over HTTPS with the box's certificate pinned; client library; mock box | security (certificate pinning) |
| 02 | `review/02-agent-core` | 16.3K | The wiring (`main.rs`, `state.rs`, `router.rs`) plus sign-in, pairing, TLS, sessions, owner and member roles, audit log, app keys | **security**. The largest slice: about half is the wiring every later slice plugs into |
| 03 | `review/03-serving-chips` | 14.5K | Starting and stopping models, chip leasing through gozer, liveness, telemetry, model modes | box check: start and stop a model |
| 04 | `review/04-box-care` | 8.3K | Power, sleep and Wake-on-LAN, signed updates, export and import, doctor | security (privileged commands, update signatures) |
| 05 | `review/05-assistant-gateway` | 13.4K | OpenAI and Anthropic `/v1` gateway, assistant tools and approvals, memories, documents, library | security (web content steering tools, key handling) |
| 06 | `review/06-household` | 8.5K | Notifications and APNs, voice, meeting recorder, transcription jobs, schedules | privacy (recording indicator, retention) |
| 07 | `review/07-research-routines` | 13.6K | Deep research, routines, model check-ups and reliability evals | — |
| 08 | `review/08-apps` | 7.1K | Store apps, desktop launches, community models | security, and **legal sign-off on the app catalog** (n8n's license) |
| 09 | `review/09-bringup-lab` | 12.4K | Orchard, Lab, model bring-up agents and their Landlock sandbox | security (agents with shell access) |
| 10 | `review/10-code` | 11.1K | Code workspace: sandboxed sessions, safe git, model picker, speed | security (sandbox escape paths). **The agent builds from here** |
| 11 | `review/11-desktop-shell` | 14.6K | Electron main process and preload, token store, API client, shared UI (3.9K is `package-lock.json`) | security (window navigation, IPC) |
| 12 | `review/12-desktop-pages` | 9.9K | Every page: Chat, Code, Models, Studio, Research, Settings… | design |
| 13 | `review/13-ios-core` | 10.2K | API client, discovery, pairing, box control, power, settings, iOS tests | box check on a phone |
| 14 | `review/14-ios-features` | 8.6K | Chat and voice, studios, research, Lab, Orchard, notification extension, Live Activity | — |
| 15 | `review/15-docs` | 0.8K | Presentations and notes | — |
| 16 | `review/16-benchmark-data` | 15.1K | Raw logs and JSON from the 2026-10-15 Code serving benchmark | skim only, or drop it and keep the README |

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

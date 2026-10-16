# Orchard: model bring-ups with tt-orchard (`/orchard`) (reference)

[tt-orchard](https://github.com/tsingletaryTT/tt-orchard) brings up a new Hugging Face model on a
QuietBox 2 using only the QuietBox 2: it triages how the model differs from one that already runs
on Blackhole, writes and runs the tests, checks the chips against a CPU reference on 1, 2 and 4
chips, and stops at a review bundle (`RESULTS.md`, `RISKS.md`, a model card, publish commands as
text). Its agents are open models served on the box (the coder on one board, a CPU fallback through
ollama). It never publishes. tt-station surfaces it as **Orchard** (Box tab → Orchard).

The agent wraps tt-orchard's own entry points and never re-implements its rules:

- setup checks and the bringup config: `orchard.setup_machine.plan` and `orchard.cli.find_config`;
- preflight: `orchard.preflight.run_preflight`, the same checks `tt-orchard bringup` runs;
- run status: `tt-orchard status --json` (a read-only view of the run's ledger);
- pause, resume, abort: `tt-orchard pause|resume|abort --run-dir`.

The first two go through `src/orchard_bridge.py` (embedded with `include_str!`, run as
`python3.12 -c <bridge> <checkout> info|preflight MODEL`), which prints one JSON object.

## Setup on the box (once)

The agent looks for `~/code/tt-orchard` (`$TT_ORCHARD_DIR`) and runs it with `python3.12`
(`$TT_ORCHARD_PYTHON`). `POST /orchard/setup` clones it if missing and runs
`python3.12 -m orchard.setup_machine --yes`: gozer, the reference venv, the `tt-orchard` link, the
QuietBox 2 config (`config/bringup.toml`, `config/tiers.toml`, written only once gozer reports two
p300 boards), `ollama pull qwen3-coder:30b` and `tt-model pull` of the coder (~160 GB). Two steps
it can't do, and the app says so:

- **tt-model** — install [tt-model-manager](https://github.com/tenstorrent/tt-model-manager) by hand.
- **ollama** — install from ollama.com (needs sudo), then run setup again.

After gozer is installed, **restart the agent** (`systemctl --user restart tt-station-agentd`):
it probes gozer once at startup, and only then leases chips for its own serves. `GET /orchard`
adds that as a note. The coder serves on port 8000, which TT-Studio's backend also uses.

## Routes (authed)

| Route | |
|---|---|
| `GET /orchard[?fresh]` | `installed`, `version`, `ready` (every setup step ok/warn and a config), `setup: [{name, status, detail}]`, `setup_running`, `setup_log`, `config {path, runs_root, coder}` or `config_error`, `busy`, `runs`, `notes: [{text, command?}]`, `stages` (the orchard names). Setup/config cached 60 s |
| `POST /orchard/setup` | `202`; systemd unit `tt-orchard-setup`, log `<config>/orchard/logs/setup.log` |
| `POST /orchard/preflight {model, accept_credentials_visible}` | `{model, run_name, run_dir, resuming, local, download_gb, license, checks: [{name, status: ok/warn/block, detail, reason}], blocked}`. Up to 5 min (the reference check imports torch) |
| `POST /orchard/runs {model, stop_serving, accept_credentials_visible}` | `202 {run_name, model, restore}`. `409 {needs_stop: true}` while a model serves (or the box is split) unless `stop_serving`; `409` during a Lab run, a running transcription, or another live bring-up |
| `GET /orchard/runs/{name}` | a run (below) plus `console` (last 60 lines) and `reports` available |
| `POST /orchard/runs/{name}/{pause\|resume\|abort\|retry}` | `pause`/`resume`/`abort` go to the live supervisor; `abort` before the supervisor starts (preflight, download) stops the unit; `retry` starts the same model again (tt-orchard resumes from its ledger), body `{stop_serving}` |
| `GET /orchard/runs/{name}/report/{results\|risks\|card\|publish\|blocked\|console}` | `text/plain`: `stages/8/bundle/{RESULTS.md,RISKS.md,card.md,PUBLISH_COMMANDS.txt}`, `BLOCKED.md`, or the console log |

A run: `run_name` (`org--name`, tt-orchard's slug), `model`, `state`, `live`, `stage {current, name}`,
`stages: [{stage, name, status, wall_s, attempts}]`, `blocked {code, reason}`, `pause {reason}`,
`hint`, `counts`, `disk`, `last_events`, `started`, `restore`. `state` is tt-orchard's (`running`,
`paused`, `ready-for-operator-review`, `blocked`, `aborted`, `stopped-or-crashed`) plus two of the
agent's: `preparing` (the unit runs but no supervisor yet: preflight and download) and `refused`
(started, never wrote a ledger: tt-orchard's preflight refused).

## Progress (`progress` on every run)

`src/orchard_progress.rs` adds what tt-orchard's status doesn't say: how far through the current
step a run is, and whether the owner has to do anything.

| Field | |
|---|---|
| `title`, `detail` | the step now: `Downloading clef` / `22.0 of 55.0 GB · 110 MB/s`; `Starting the agents' model`; `🧪 Stage 1 of 8: soil test` / `An agent is working on it: turn 12 of up to 60. Usually 15 min; Orchard stops it after 4 h.`; `Testing on 4 chips: … Gives up after 45 min.`; `Waiting for chips to be free.` |
| `fraction`, `eta_s` | the step's bar and time left; `null` when it can't be measured (waiting for chips) |
| `overall`, `overall_eta_s` | the whole run, estimated: download (at 50 MB/s until measured) + the coder's first boot (+ its weights) + stages 0, 1, 2, 4, 8 at typical times, plus any other stage once it starts; skipped stages count as done and free |
| `needs_you` | paused, blocked (with tt-orchard's reason), stopped, refused, or ripe ("read the harvest basket") |
| `hint` | while it runs: nothing to do, notifications will come |
| `typical_s`, `budget_s`, `estimate_note` | per-stage typical times (from tt-orchard's first Clef run) and tt-orchard's `STAGE_BUDGET_S` |

Sources: bytes on disk under `<hf_home>/hub/models--org--name` (partial files included) against the
repo's size on the Hub (`/api/models/{repo}?blobs=true`, cached), speed averaged over 90 s; the
coder's weights repo from its tt-model manifest; the ledger's decisions (`coder starting`/`started`,
`agent step`, `hardware test started` with `deadline_s` and chips, `queued for lease`, `stopping the
coder`); turns from the newest `stages/<n>/log/*.jsonl`. When the ledger has no open stage, the
stage tt-orchard's status reports as running is used.

## How a run is held

- **Its own unit.** `systemd-run --user --unit=tt-orchard-<run name> --collect`, output appended to
  `<config>/orchard/logs/<run name>.log`, with the agent's `PATH` plus `~/.local/bin` and
  `~/.tenstorrent-venv/bin`. It outlives agent restarts and upgrades. Launches (model, unit, the
  model to restore) are kept in `<config>/orchard/launches.json`. Without `systemd-run` the run is
  a detached child of the agent.
- **The chips are tt-orchard's.** While any run is live (its unit is active or its supervisor is
  alive, so runs started from a terminal count too) `AppState::chips_holder` refuses model starts,
  chip resets, power actions, co-serving, Lab runs and doctor fixes, the jobs queue waits, tt-smi
  isn't run (gozer would see it as a chip user), and the doctor neither reads chips nor calls
  tt-orchard's containers strays.
- **The watcher** (`orchard::watch_forever`, every 15 s) re-reads a run's status only when its
  ledger changed or it's live, sets `orchard_busy`, and notifies: each stage that passes (`🌱 Clef:
  graft passed`, body "Stage 2 of 8 (…) passed after 41 min."), fails or escalates; paused; ripe
  (`🍎 Clef is ripe`); frost (blocked, with tt-orchard's reason); aborted; stopped on an error; and
  refused. It doesn't notify about what it finds on its first pass, so an agent restart is quiet.
  When no run is live, it starts the model that was serving before (once).

## Tests

`tests/orchard.rs` drives the routes against `tests/fixtures/fake-orchard`, a stand-in checkout
with the same entry points (bridge modules, `bin/tt-orchard` with `bringup`/`status`/control
words): setup and preflight, `needs_stop`, one run at a time, chips held, four stages to ripe,
notifications, reports and path checks, the old model restored, pause/resume/abort, and a
refusal. Unit tests cover id validation, notifications and the `preparing`/`refused` states.
Not yet run against a real bring-up: this box lacks tt-model, ollama and gozer.

## Stopping, resets and tt-smi (found on the first real run)

- **Stop escalates.** `abort` writes tt-orchard's control word, but the supervisor reads it only
  between steps: a coder boot polls `/health` for up to `COLD_BOOT_BUDGET_S` (45 min) without
  looking, even after the coder's container has exited. If the supervisor is still alive after
  30 s (`Orchard::abort_grace`), the agent sends it SIGTERM, tt-orchard's own clean abort (records
  `abort by signal SIGTERM`, stops the coder, releases the leases). Verified live.
- **Fresh chips.** A start resets the chips first (`run_power_command(ResetChips)`): the coder
  failed with "timed out while waiting for active ethernet core … try resetting the board" after a
  Wan2.2 serve.
- **The right tt-smi.** `~/.local/bin/tt-smi` (first on PATH) is the 3.x copy bundled with
  tt-inference-server; it rejects the PCI addresses gozer resets by, so gozer's and the agent's
  leased resets failed. `agentd.toml` sets `[global] tt_smi_bin = "/usr/local/bin/tt-smi"` (6.1);
  the agent's resets use it, and runs get it as `GOZER_RESET_CMD`.
- **Restore waits.** The model that was serving starts again 30 s after the last run stops
  (`settle_after_run`): a reset started while tt-orchard was still releasing failed.
- **`tt serve` under a lease.** With gozer installed every serve is leased; `serve_with = "tt"` is
  allowed when the lease covers every chip in `/dev/tenstorrent` (the whole box), refused otherwise.


## Bring-up engines (see also `bringup.rs`)

Bring-up starts from the model: `POST /bringup/survey` reads its Hugging Face
architecture and compares it with what the box's tt catalog runs (cached in
`bringup/architectures.json`). Same architecture → Orchard (weights-only
grafting, local agents, no account). New architecture → Codex, which runs
tt-metal's fast-models-fast pipeline (`~/tt-metal`, branch
`agentic-research/fast-models-fast`, `.agents/scripts/multigoal`, 11 stages)
under `gozer run --chips 4 --fresh`, as the systemd unit
`tt-codex-bringup-<id>`, logging to `~/.config/tt-station/bringup/codex/<id>/`.

The driver (`codex_bringup.py`) loads the real runner and replaces only its
approval handler: with policy `smart` (default) routine commands are approved
on the box and risky ones (chip resets, sudo, installs, deletes or writes
outside the repo, services/containers, and anything that talks to another
machine: curl/wget/nc/ssh/scp, git push or a fetch from a named URL, inline
`python -c`/`node -e` code that imports networking) wait for a person in the
app; `ask` sends every request; `auto` keeps the runner's unattended
behaviour. Claude's WebFetch reads documentation pages on a few hosts without
asking; any other address, a query string, or a path that isn't a page's shape
asks. This is a guard, not containment (a script the agent writes can still
reach the network). What contains a stage is its sandbox: a Landlock launcher
(`sandbox_exec.py`) that hides TT Station's folder and the account's keys and
leaves only the run's folder, the repo and caches writable, the systemd
properties a user manager applies, and no local trust for anything in the
run's unit; see [bringup-containment.md](bringup-containment.md), which also
covers what needs root (`deploy/contain-bringup.sh`). Approvals are answered
from an owner's paired device only, never by local trust. The
run's secret (`TT_STATION_RUN_SECRET`) is removed from the environment the
stage inherits, works only while the run is live, and is new at every launch; `HF_TOKEN` isn't set in it, but the stage reads
`~/.cache/huggingface/token` through `HOME`, which it needs to download gated
weights.
Stop and Resume (resume continues the stage's Codex thread via
`--resume-stage`). Claude Code is listed as the next engine.

### Claude Code engine

`claude_bringup.py` runs the same eleven stage prompts with `claude -p
--output-format stream-json` (skills from `~/.claude/skills`; `$skill` becomes
"the `skill` skill"), asks Claude to end each stage with `GOAL: complete` or
`GOAL: blocked — why`, runs the same `NN-*.check.sh` gates with one fix-up
turn, and writes the same `manifest.txt` / `STATUS.md` / exit codes as the
Codex runner. Permission questions go through `permission_mcp.py` (an MCP
server passed with `--permission-prompt-tool mcp__ttstation__approve`), which
applies the same smart/ask policy and reports to TT Station; `auto` uses
`--dangerously-skip-permissions`. Claude Code is installed user-locally
(`npm i -g @anthropic-ai/claude-code`, linked into `~/.local/bin`); signing in
is the owner's (`POST /bringup/claude/login` opens a terminal running
`claude auth login`).

### Finish line

Once stage 9 (vLLM) passes: **Package** runs one more stage (12) with the same
engine, using tt-model-manager's `tt-model-yaml` and `tt-model-package-test`
skills (linked into `~/.codex/skills` and `~/.claude/skills`), staging a
CONTAINER package under `<run>/package/out` and writing `PACKAGE.md`
(`repo_id:`, `staged_dir:`, what was verified). **Serve** runs `tt-model serve
<repo_id> --local-only --port <serving port>` as the harvest unit. **Push
(private)** runs `tt-model push <staged> --repo <id> --private`; **List
publicly** (`tt-model publish`) is a separate, confirmed step.

### Waiting for the chips, deleting runs, and following along

- Runs start through `lease_run.py`, not bare `gozer run`: when gozer queues
  (exit 10, e.g. the serving model is still letting go), it cancels the ticket,
  writes `chips-wait.json` (who holds the chips) and asks again every 20 s.
  The run shows "Waiting for the chips: … has them". A start guard stops a
  double click from launching two runs.
- `DELETE /bringup/codex/runs/{id}` (logs only; the port's code stays in
  tt-metal) and `DELETE /orchard/runs/{name}` (the run folder), both refused
  while the run is live.
- Tori: "how's the bring-up going?", "allow it" / "deny it" (and plain yes/no
  when only a bring-up is waiting). The phone's Happening-now card shows the
  live bring-up with Allow / Deny. Desktop notifications for bring-ups open
  Bring-up.

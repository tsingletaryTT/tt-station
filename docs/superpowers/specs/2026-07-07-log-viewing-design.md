# Log Viewing for tt-station — Design

**Status:** approved (conversation, 2026-07-07). **Author:** box-side session.

## Problem

When a model fails (or is slow) to start remotely, the failure is **invisible from the
Mac**. Investigation (2026-07-07) established:

- `tt-inference-server`'s own `run.py` phase **succeeds in ~1 second** every time
  (system-software validation, `tt-smi` parse, HF token, RAM/disk all ✅) and then
  hands off to `docker run`. run.py's file log **stops at that handoff** — e.g. every
  short 70B log ends at `run.py:627 Running inference server in Docker container … /
  Weights directory does not exist`.
- The real work — downloading ~140 GB of 70B weights and booting vLLM on the mesh
  (10–40 min) — and every real failure (OOM, mesh/ethernet-core `TT_THROW`,
  weight-download stall, vLLM crash) happens **inside the container**, whose output
  streams to a *different* place: `~/code/tt-inference-server/workflow_logs/docker_server/vllm_*.log`
  and `docker logs <id>`.
- The agent's only supervision is polling `GET /v1/models` up to a ~40-min ceiling. It
  logs the pre-serve steps (board reset, device detect) and **nothing** from run.py or
  the container. So "downloading 140 GB" and "container crashed" look identical from the
  Mac — both are just "not ready yet" until the ceiling trips.
- Some apparent "failures" are the agent superseding itself: a rapid retry / model-id
  correction (e.g. `Llama-3.1-70B` at 15:01 → `Llama-3.1-70B-Instruct` at 15:02, 77 s
  apart) kills the in-flight `run.py`, leaving a truncated log that *looks* like a crash.

**Conclusion:** these are mostly not tt-inference-server bugs. The fix is **visibility** —
surface the container log (where the failures live) to the operator and the Mac.

## Scope (this spec)

Three parts, shipped together:

- **A. `GET /logs` on the agent** — expose the serving logs over HTTP (bounded tail) and
  a WebSocket live-follow, unauthed-read (consistent with `/telemetry`, `/serving`,
  `/status`, `/models`, `/config`).
- **B. `tt logs` CLI + a log pane in `tt console`** — consume the route from the CLI, and
  give the operator TUI a live view of THIS box's serving log.
- **C. Journal surfacing in `runpy.rs`** — when the agent launches run.py / the container,
  emit the run.py log path, container log path, container ID, and the `docker logs -f`
  hint to the agent's stderr (→ systemd journal); on health-poll failure/timeout, tail the
  last lines of the container log into the journal so `journalctl --user -u
  tt-station-agentd` tells the story.

Explicitly **out of scope** (fast-follows, noted in Known follow-ups): structured
serve-phase parsing into `/status` (option D); a macOS "View logs" button (option E — a
brief for the other Claude); fixing the non-clean `/run` abort (tracked separately in
BOX_TELEMETRY_VALIDATION.md).

## Design

### A. `GET /logs` (agent)

**Both sources are files** under the tt-inference-server repo's `workflow_logs/` dir —
run.py streams the container's stdout/stderr to a file, so we never need to shell
`docker logs`: it's a plain file that persists after the container is removed and follows
cleanly by byte offset. Selectable by `?source=`:

- `source=container` (**default**) — the newest `docker_server/vllm_*.log` (the container's
  streamed stdout/stderr; where model-load failures — OOM, mesh `TT_THROW`, weight-download
  stall, vLLM crash — actually appear). Persists across container death.
- `source=run` — the newest `run_logs/*.log` (run.py's own log; surfaces the validation /
  supersede-truncation cases).

Both require knowing the repo dir (the runpy backend). With no repo dir configured (e.g.
dstack backend), the route returns a clear "not available for this backend" error. This is
runpy-specific by design — it's the backend that fails-to-start on this box. (External
containers with no `workflow_logs` file are a documented fast-follow, not v1.)

Two access shapes (mirror the existing split: plain reads vs the WS on `/telemetry`):

- `GET /logs?source=container|run&tail=N` → plain HTTP JSON:
  `{ source, origin, lines: [String; ≤N] }` where `origin` is the container id (or file
  path). `tail` defaults to 200, capped at a `MAX_TAIL` (e.g. 2000) to bound response size.
- `GET /logs/stream?source=container|run&tail=N` → WebSocket. On connect, replays the last
  `tail` lines, then follows by polling the file's byte offset on an interval (the
  `/telemetry` `tokio::select!` + `interval(Delay)` pattern), emitting only newly-appended
  lines. Re-resolves the newest file each tick so a fresh serve (new timestamped file)
  is picked up (offset resets, replays the new file). Text frames. Unauthed, like
  `/telemetry`.

**Redaction (defense-in-depth):** logs are unauthed-read. run.py already prints the HF
token only as `✅ HF_TOKEN is valid` and launches docker via `--env-file .env` (no secret
in argv), so no secret is expected in these logs. Regardless, pass every emitted line
through a redactor that masks anything matching an obvious secret pattern (e.g.
`hf_[A-Za-z0-9]{20,}`, `Bearer <hex>`, `sk-…`). Cheap insurance; keep the surface unauthed
and consistent.

**Errors:** no active container (`source=container`) → `200` with `lines: []` and an
`origin: null` (not an error — "nothing serving"); repo dir unknown / backend not runpy
(`source=run`) → `409`/clear JSON error. Never 500 on "no logs yet".

### B. `tt logs` CLI + `tt console` pane

- `tt logs [--source container|run] [--tail N] [--follow] [--host H]`, respects global
  `--json`.
  - no `--follow`: GET `/logs`, print the tail (plain lines, or the JSON object under
    `--json`).
  - `--follow`: connect `/logs/stream`, print lines as they arrive until Ctrl-C.
  - host/port resolution identical to existing subcommands (`--host`, `TT_CONFIG_DIR`).
- `tt console` log pane: the operator TUI (for THIS box) adds a scrollable, auto-tailing
  pane showing the newest container log file (box-local file tail — no HTTP needed, since
  console operates the local agent). Toggle/scroll via keybindings consistent with the
  existing UI. Non-invasive: if no serving log exists yet, the pane shows a friendly
  "no serving log yet" placeholder.

### C. Journal surfacing (runpy.rs)

At serve time, `run.py`'s captured stdout (already returned by
`runner.run_in_dir_with_env`) contains the exact breadcrumbs — `Created Docker container
ID: <id>`, `Docker logs are also streamed to log file: <path>`, and the run.py log path.
A pure helper parses these out; the agent then `eprintln!`s (→ journal), matching the
existing `tt-station-agentd:` log style:

- the run.py log path,
- the container log path (`workflow_logs/docker_server/vllm_*.log`),
- the container ID and `docker logs -f <id>` hint.

On health-poll **failure or ceiling timeout**, tail the last ~20 lines of the container
log into the journal (best-effort; never panics, never blocks shutdown), so a failed start
leaves an explanation in `journalctl` instead of silence.

## Testing strategy

- **A:** pure helpers unit-tested (tail-a-file returns ≤N last lines; the redactor masks
  known patterns and leaves clean lines intact; `source=run` newest-file selection picks
  the newest by mtime). Route-level tests spin the router with mock state (mirror existing
  route tests / mock-box) for: `source=container` with no container → `lines: []`,
  `origin: null`; `source=run` with a temp repo dir containing fixture log files → returns
  the newest file's tail; `source=run` when backend isn't runpy → clear error.
- **B:** CLI tested against mock-box (mirror `crates/tt/tests/e2e_mock.rs`): `tt logs`
  prints the tail; `--json` emits the object; unknown host errors cleanly. Console pane:
  unit-test the pure tail/scroll-state helper; the ratatui draw + event loop stays
  owner-verified (consistent with the rest of console).
- **C:** unit-test the pure "format the breadcrumb lines" + "tail last N lines of a path"
  helpers; the `eprintln!` wiring is owner-verified.

## Naming / consistency constraints

- CLI tool names must stay configurable — never hardcode `tt` / `tt-station-agentd` /
  service names in more than one place (see the configurable-cli-tool-names memory and
  `crates/tt/src/console/names.rs`).
- New unauthed routes join the same unauthed group as `/telemetry`/`/serving`; do not add
  auth to reads.
- Reuse the existing command-runner abstraction for `docker logs` (don't hand-roll a new
  `std::process::Command` path if the codebase already has one).
- Follow the existing WS structure from `/telemetry` for `/logs/stream`.

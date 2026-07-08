# `/logs`, `/logs/stream`, and `tt logs` (reference)

*Documents the shipped behavior of `crates/tt-station-agentd/src/logs.rs` and the
`GET /logs`/`GET /logs/stream` handlers in `crates/tt-station-agentd/src/routes.rs`, plus
the `tt logs` subcommand in `crates/tt/src/main.rs`. Design history:
`docs/superpowers/specs/2026-07-07-log-viewing-design.md`.*

## Why this exists

Before this feature, a remote model-start failure was **invisible from the Mac**.
`run.py`'s own file log succeeds in about a second (system-software checks, `tt-smi`
parse, HF token, RAM/disk) and then hands off to `docker run` — the log simply **stops at
that handoff**. The real work (downloading up to ~140 GB of weights, booting vLLM on the
mesh, 10–40 minutes) and every real failure — OOM, a mesh/ethernet-core `TT_THROW`,
a weight-download stall, a vLLM crash — happens **inside the serving container**, whose
stdout/stderr streams to a *different* file. The agent's own supervision is just polling
`GET /v1/models` up to a health-poll ceiling (~40 min); it logs its own pre-serve steps
(board reset, device detect) but nothing from `run.py` or the container. So, before this
feature, "downloading 140 GB" and "container crashed" looked identical from the Mac —
both were just "not ready yet" until the ceiling tripped, and finding out which one you
had meant SSHing in and reading `docker logs` by hand.

The fix is visibility: expose both log files — the point where failures actually surface,
and run.py's own launch log — over HTTP/WebSocket, unauthed-read like the rest of the
box's discovery surface (`/status`, `/models`, `/serving`, `/config`, `/telemetry`).

## The two sources

Both sources are **files** under the tt-inference-server checkout's `workflow_logs/` dir
— `run.py` streams the container's stdout/stderr to a file, so the agent never has to
shell out to `docker logs`; it just tails a plain file that persists after the container
is removed and follows cleanly by byte offset.

| `source=` | Directory | What it is | Why you'd read it |
|---|---|---|---|
| `container` (**default**) | `workflow_logs/docker_server/vllm_*.log` | The serving container's own stdout/stderr | **This is where failures live** — OOM, mesh `TT_THROW`, a weight-download stall, a vLLM crash. Persists after the container dies, so it's still readable after a failed start. |
| `run` | `workflow_logs/run_logs/*.log` | `run.py`'s own launch log | Validation steps and the handoff to `docker run`; also where a rapid retry / model-id correction shows up as a truncated log (the agent superseded itself, not a crash). |

Both routes require the `runpy` backend (the one that knows about a
`tt-inference-server` checkout and its `workflow_logs/` dir). On a non-`runpy` backend
(e.g. `dstack`) there is no such directory, so both routes answer `409` — see "Errors"
below.

Within each directory, the **newest `*.log` by mtime** is the one tailed/followed — a
fresh serve writes a new timestamped file, and both routes pick it up automatically (see
`GET /logs/stream` below for how a live connection detects the switch).

## `GET /logs`

```
GET /logs?source=<container|run>&tail=<N>
```

**Unauthed** — same group as `/status`, `/models`, `/serving`, `/config`, `/telemetry`.
Plain read-only file access; no bearer token needed, no pairing required.

- `source` — defaults to `container`.
- `tail` — number of trailing lines to return. Defaults to `200`, capped at `2000`
  (`crate::logs::DEFAULT_TAIL` / `MAX_TAIL`) regardless of what's requested, to bound
  response size. An unparseable `tail` value falls back to the default rather than
  erroring.

Response body (`LogsResponse` on the agent, `libttstation::model::LogsInfo` on the
client side — same JSON shape on the wire):

```jsonc
{
  "source": "container",
  "origin": "/home/operator/code/tt-inference-server/workflow_logs/docker_server/vllm_2026-07-07T14-32-01.log",
  "lines": ["...", "...", "..."]
}
```

`origin` is the absolute path of the file actually tailed, or `null` when there's nothing
to tail yet.

### Status codes

| Situation | Status | Body |
|---|---|---|
| Normal tail | `200` | `LogsResponse` as above |
| No log file written yet for this source | **`200`** | `{ "source": ..., "origin": null, "lines": [] }` — **not an error.** An idle box (or one that just booted and hasn't served anything) has no log file, and that's the normal state, not a failure. |
| Unknown `source` (anything other than `container`/`run`) | `400` | `{ "error": "unknown source '<value>'" }` — caller's mistake. |
| No `tt-inference-server` repo configured (non-`runpy` backend) | `409` | `{ "error": "logs unavailable: no tt-inference-server repo configured (non-runpy backend)" }` |
| Unexpected I/O failure while reading | `500` | `{ "error": "failed to read logs" }` |

## `GET /logs/stream`

```
GET /logs/stream?source=<container|run>&tail=<N>
```

**Unauthed**, WebSocket. Same query params/defaults as `GET /logs`. On connect:

1. Resolves the newest log file for `source` and replays its last `tail` lines as one
   text frame per line (through the same redactor `GET /logs` uses).
2. Then **follows**: every ~500ms it re-resolves the newest file in that source's
   directory and sends any lines appended since the last check as new text frames.
   - Re-resolving on every tick (rather than latching onto one path) is what makes a
     **fresh serve** — which writes a new timestamped log file — get picked up
     automatically: the follower detects the newest-file path changed and restarts the
     replay from byte 0 of the new file.
   - If the file disappears mid-follow (e.g. cleaned up outside the agent process),
     that's treated as "nothing to follow right now," not a fatal error — the next tick
     just re-resolves.

An unknown `source` or a non-`runpy` backend sends a single `{"error": "..."}` text frame
and then closes the connection — the same error conditions `GET /logs` reports as HTTP
status codes, just delivered as a frame since the WebSocket upgrade has already committed
to `101 Switching Protocols` by the time the source is validated.

The stream only ever sends `Message::Text` frames (no binary, no structured envelope per
line) — a client that wants to `tail -f`-style pipe the output through can just print each
frame verbatim.

## Redaction

Every line emitted by either route — the initial tail and the live follow — passes
through `crate::logs::redact_line`, which masks obvious secret shapes before the line
ever leaves the box:

- `hf_<20+ alphanumeric chars>` → `hf_***`
- `sk-<20+ alphanumeric chars>` → `sk-***`
- `Bearer <token>` → `Bearer ***`

This is **defense-in-depth, not the primary control**: `run.py` already avoids printing
the real HF token (it logs `✅ HF_TOKEN is valid` and passes secrets to `docker run` via
`--env-file`, not argv), so no secret is *expected* in these logs in the first place. The
redactor exists because `/logs`/`/logs/stream` are unauthed reads on a LAN-trust surface,
and cheap insurance costs nothing.

## `tt logs`

```
tt logs --host <host:port> [--source container|run] [--tail N] [--follow]
```

Unauthed on the agent side, so — like `tt status`/`tt models`/`tt serving`/`tt config` —
this works against a box you've never `tt pair`ed with.

- No `--follow` (default): one `GET /logs` call, printed as plain lines (`origin` header
  line, then one line per log line) — or the whole `LogsInfo` object as JSON under the
  global `--json` flag.
- `--follow`: connects to `GET /logs/stream` and prints each line as it arrives until the
  connection closes (agent restart/shutdown) or you hit Ctrl-C. **Always plain text, no
  `--json` mode** — a log stream is an unstructured sequence of lines rather than one
  decodable object, so wrapping each line in a JSON envelope would just be noise for a
  caller that almost certainly wants to pipe this straight through.

Examples:

```bash
# Last 200 lines of the serving container's log (the default) — where failures live
tt logs --host qb2-lab.local:8765

# The last 50 lines of run.py's own launch log
tt logs --host qb2-lab.local:8765 --source run --tail 50

# Live-follow the container log while a model is starting
tt logs --host qb2-lab.local:8765 --follow

# JSON, for scripting (one-shot only, not with --follow)
tt --json logs --host qb2-lab.local:8765 --tail 20
```

(`tt`/`tt-station-agentd` are this project's default binary/service names — every name is
independently configurable; see the "Configurable tool names" section of
`docs/reference/tt-console.md`.)

## The `tt console` log pane

`tt console` — the SSH operator TUI for the box's own agent — has an auto-tailing pane
that shows the newest container log (`GET /logs?source=container&tail=20`) below the
serving panel, refreshed on the same ~1s tick as the rest of the snapshot. This is
**already documented in `docs/reference/tt-console.md`** ("Log pane" section) — see that
doc for the pane's layout, the `(no serving log yet)` placeholder, and the current
auto-tail-only (no manual scroll) limitation. Not re-documented here.

## Fast-follow list (not shipped)

Called out explicitly so they aren't mistaken for gaps in this doc rather than known,
deliberate scope cuts:

- **External-container fallback.** `/logs` only knows about `workflow_logs/` files
  written by this box's own `run.py`-launched containers. A container started some other
  way (e.g. a manual `docker run`, or a tool other than `tt-inference-server` entirely)
  has no `workflow_logs` file to tail — a `docker logs <id>` fallback for containers
  visible in `GET /serving` with `source: external` is a documented follow-up, not v1.
- **Structured serve-phase field in `/status`.** Right now, "downloading weights" vs
  "container crashed" both look like "not ready yet" to anything polling `/status`;
  parsing the log for a phase and surfacing it as a structured field is deferred (this is
  "option D" from the design spec).
- **A macOS "View logs" button.** The Mac app has no UI for this yet — see
  `macos/README.md` for the pointer; wiring it up is future work for that codebase.
- **Console manual scroll.** The `tt console` pane is auto-tail-only (see above); adding
  scrollback/history is a documented follow-up there.
- **Configurable `tail=20` default for the console/snapshot fetch.** The
  `BoxLifecycleSnapshot.logs` fetch is hardcoded to `tail=20`; making that configurable
  wasn't needed for v1.

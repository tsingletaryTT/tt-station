# agentd `/telemetry` — `tt_toplike.processes` enrichment (scaffold) — design

**Date:** 2026-07-05
**Status:** Approved (self-approved per owner delegation)
**Author:** Claude (from the tt-toplike brief `TT_TOPLIKE_STREAM.md`)
**Schema owner:** tt-toplike (reference producer + consumer). This is the tt-station side.

## Goal

Enrich `agentd`'s `GET /telemetry` WebSocket frame with one **optional, additive** top-level
`tt_toplike` key carrying a **process list**, so a tt-toplike `--remote` session shows the *box's*
processes instead of the viewer's laptop. The frame stays **byte-for-byte valid `tt-smi -s` JSON**
for the telemetry portion (existing consumers unaffected). Per the owner's decision, this ships the
**`processes` scaffold now** and **defers the `inference` block** (which needs a vLLM `/metrics`
scrape and must match tt-toplike's not-yet-final canonical frame).

## Scope

**In:** the `tt_toplike` key with `{ schema, processes }`; a process scan (sysinfo + a `/proc/<pid>/fd`
scan for `uses_tt`); graceful/optional emission; a contract test that the enriched frame still parses
as tt-smi JSON.

**Out (deferred):** the `inference` array + vLLM `/metrics` scrape + rate-delta math (a later spec,
built to tt-toplike's published canonical example so shapes match exactly).

## Coordination note (confirm with the tt-toplike side)

We emit `tt_toplike` with **`processes` only and no `inference` key**. This must be read as "inference
not streamed → tt-toplike falls back to its local inference view," NOT "box reports zero inference."
The brief says an *absent whole `tt_toplike`* means local fallback; confirm tt-toplike treats an
absent `inference` **sub-key** the same way (local fallback for that panel) rather than "none." If
tt-toplike would misread a missing `inference` as "none running," we instead emit `"inference": null`
(explicit "not provided"). Default plan: **omit the key**; flip to `null` only if tt-toplike needs it.

## Frame shape (this scaffold)

```json
{
  "device_info": [ /* tt-smi -s, byte-for-byte as today */ ],
  "tt_toplike": {
    "schema": 1,
    "processes": [
      { "pid": 12345, "name": "python3",
        "cmd": "python -m vllm.entrypoints… --model …",
        "uses_tt": true, "cpu_pct": 31.4, "mem_bytes": 8123456789 }
    ]
  }
}
```
Field names/types match the brief exactly: `pid` (u32), `name` (String), `cmd` (String),
`uses_tt` (bool), `cpu_pct` (f32, plain number), `mem_bytes` (u64). `schema` is an integer (1).

## Architecture / components

### New: `crates/tt-station-agentd/src/procscan.rs`

```rust
pub const TT_TOPLIKE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtToplike {
    pub schema: u32,
    pub processes: Vec<ProcInfo>,
    // NOTE: no `inference` field yet — deferred (see Coordination note).
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    pub uses_tt: bool,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
}

/// Stateful — owns a `sysinfo::System` so cpu% is computed across refreshes
/// (sysinfo needs two samples for a meaningful cpu percentage; the telemetry
/// loop's interval provides the ticks).
pub struct ProcessSampler { sys: sysinfo::System }

impl ProcessSampler {
    pub fn new() -> Self;
    /// One scan: refresh processes, flag `uses_tt` via a /proc/<pid>/fd scan,
    /// select (tt-holders first, then busiest by cpu), cap at MAX_PROCESSES.
    pub fn sample(&mut self) -> TtToplike;
}

pub const MAX_PROCESSES: usize = 12;
```

**Pure, unit-tested helpers** (the logic; keep the filesystem/sysinfo I/O thin around them):
- `fn select_processes(mut procs: Vec<ProcInfo>, cap: usize) -> Vec<ProcInfo>` — stable-partition
  `uses_tt` holders to the front, sort the remainder by `cpu_pct` desc, truncate to `cap`. (Holders
  always kept even if idle; the rest are the busiest.)
- `fn target_holds_tt_device(fd_link_targets: &[String]) -> bool` — true if any target starts with
  `/dev/tenstorrent`. (Given the readlink results; pure so it's testable without real `/proc`.)

**`uses_tt` gathering** (`ProcessSampler::sample`): for each candidate pid, read `/proc/<pid>/fd/`,
`readlink` each entry, pass targets to `target_holds_tt_device`. Best-effort: a pid whose `fd` dir is
unreadable (owned by another user — e.g. a docker/root-run vLLM container process) yields
`uses_tt = false`. **Documented limitation:** `uses_tt` reliably reflects only processes the agent
(its uid) can inspect. Errors reading any single pid are swallowed (skip → false), never propagated.

### Modified: `crates/tt-station-agentd/src/telemetry.rs`

Keep `snapshot()` unchanged. Add a pure enricher:

```rust
/// Insert the optional `tt_toplike` object into a `tt-smi -s` JSON frame,
/// returning the re-serialized frame. If `toplike` is `None`, or `frame` does
/// not parse as a JSON object, returns `frame` unchanged (verbatim) — the
/// telemetry contract is preserved and a scan hiccup never corrupts the frame.
pub fn enrich_frame(frame: &str, toplike: Option<&procscan::TtToplike>) -> String
```
Implementation: `serde_json::from_str::<serde_json::Value>(frame)`; if it's `Value::Object` and
`toplike` is `Some`, `map.insert("tt_toplike", serde_json::to_value(t)?)`, re-serialize; on any
failure (parse error, non-object, serialize error) return `frame.to_string()`.

### Modified: `crates/tt-station-agentd/src/routes.rs` (telemetry WS loop)

Construct a `ProcessSampler` once per stream. Each tick: `let frame = telemetry::snapshot(bin, run)?;`
(unchanged error handling — a tt-smi failure still skips/errors the tick as today), then
`let toplike = sampler.sample();` and push `telemetry::enrich_frame(&frame, Some(&toplike))`. The scan
is additive; it must not change the existing tt-smi error/skip behavior. (`sample()` is infallible —
sysinfo refresh + best-effort fd scan — so processes are ~always present; if a future change makes it
fallible, pass `None` on failure to omit the key.)

### Modified: `crates/tt-station-agentd/src/lib.rs`, `Cargo.toml`

`pub mod procscan;`. Add `sysinfo` to the workspace + agentd `Cargo.toml`.

## Error handling / degradation

| Situation | Behavior |
|---|---|
| `tt-smi -s` fails | Unchanged from today (tick skipped / error surfaced by the WS loop). `enrich_frame` not reached. |
| `tt-smi` stdout not valid JSON object | `enrich_frame` returns it verbatim — no `tt_toplike` key added. |
| A single pid's `/proc/<pid>/fd` unreadable | That pid's `uses_tt=false`; scan continues. Never propagates. |
| Process scan yields empty list | Emit `tt_toplike { schema:1, processes: [] }` (we *can* gather — an empty box is a valid answer, distinct from "not gathered"). |

## Testing

- **`enrich_frame`:** Some → frame parses as a JSON object, `tt_toplike.schema==1`, `device_info`
  still present (telemetry portion intact); None → byte-identical to input; non-JSON / JSON-array
  input → returned verbatim (no key).
- **`select_processes`:** tt-holders float to front and survive the cap even when idle; non-holders
  ordered by `cpu_pct` desc; result length ≤ cap.
- **`target_holds_tt_device`:** `["/dev/tenstorrent0"]`→true, `["/dev/null","socket:[123]"]`→false,
  `[]`→false, `["/dev/tenstorrentX/y"]`→true (prefix match).
- **serde:** `TtToplike`/`ProcInfo` serialize with exactly the brief's field names (`pid`/`name`/
  `cmd`/`uses_tt`/`cpu_pct`/`mem_bytes`/`schema`), asserted against a JSON string.
- **Contract:** build an enriched frame from a canned tt-smi JSON + a sample `TtToplike`; assert it
  parses back and both the tt-smi telemetry and the `tt_toplike.processes` array are readable.
- The live sysinfo scan + real `/proc/<pid>/fd` walk are owner-verifiable over the running box
  (`websocat`/python WS read of `ws://…/telemetry`); the correctness-sensitive logic is the pure
  helpers above, which are fully unit-tested.

## Rollout

Additive and safe: existing tt-toplike/JSONBackend consumers ignore the unknown `tt_toplike` key and
keep parsing telemetry. Once tt-toplike's `--serve` canonical frame + `--remote` richer consumer land,
verify our `processes` shape renders identically, then a follow-up spec adds the `inference` block to
the same `schema: 1` (additive).

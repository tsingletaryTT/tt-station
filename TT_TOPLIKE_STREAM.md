# tt-station brief: enrich the `/telemetry` frame with the `tt_toplike` extension

**Audience:** whoever updates `tt-station-agentd` (coordinated by Taylor).
**Owner of the schema:** `tt-toplike` is the reference producer *and* consumer — match its
serialization exactly. This brief describes *what* agentd should add; the canonical field shapes
live in tt-toplike (`docs/superpowers/specs/2026-07-05-serve-broadcast-design.md` + the code once
it lands).

## Why

Today `agentd`'s `GET /telemetry` WebSocket pushes the **verbatim stdout of `tt-smi -s`** — chip
telemetry only (`crates/tt-station-agentd/src/telemetry.rs`). When tt-toplike connects with
`--remote`, it therefore only reflects the box's *chips*; the process panel shows the **viewer's
local** processes and the `[i]` inference tab probes the **viewer's local** docker, because the
stream carries nothing else. That's confusing (looks like you're watching the box, but half the
screen is your laptop).

tt-toplike is gaining its own `--serve`/`/serve` publisher that emits a **richer** frame so a
tt-toplike↔tt-toplike remote shows the full box (chips + processes + inference). For agentd's
`--remote` sessions to be equally complete, agentd should emit the **same enrichment**.

## What to change — one additive, optional key

Keep the frame **valid `tt-smi -s` JSON** (do not reshape it — tt-toplike still parses telemetry
from it unchanged, and older consumers ignore unknown keys). Add **one optional top-level key**,
`tt_toplike`:

```json
{
  "time": "…", "device_info": [ /* tt-smi -s, byte-for-byte as today */ ], /* …tt-smi… */,

  "tt_toplike": {
    "schema": 1,
    "processes": [
      { "pid": 12345, "name": "python3",
        "cmd": "python -m vllm.entrypoints… --model …",
        "uses_tt": true,          // holds /dev/tenstorrent
        "cpu_pct": 31.4,
        "mem_bytes": 8123456789 }
    ],
    "inference": [
      { "key": "tt-inference-server-2269d4f6",   // stable id (container name)
        "label": "Qwen3-32B",                      // model basename
        "phase": "ready",                          // down|compiling|loading|ready|alarm
        "progress": null,                          // 0.0–1.0 or null
        "serving": {                               // null unless a vLLM /metrics scrape succeeded
          "generation_tps": 842.0, "prompt_tps": 120.0,
          "requests_running": 6, "requests_waiting": 2,
          "kv_cache_usage": 0.42,
          "ttft_avg_s": 0.11, "queue_avg_s": 0.02,
          "prefill_avg_s": 0.05, "decode_avg_s": 0.03, "tpot_avg_s": 0.01,
          "completed_delta": 4, "errored_delta": 0,
          "prefix_hit_rate": 0.0, "preemptions_delta": 0
        }
      }
    ]
  }
}
```

Rules:
- `tt_toplike` is **optional**. If agentd can't gather it this tick, omit the key — tt-toplike falls
  back to local views + an honest "LOCAL / not streamed" label. Never send a half-populated object
  in place of omitting it.
- `schema` is an integer; bump it on any breaking field change. tt-toplike ignores a `tt_toplike`
  whose `schema` it doesn't understand (→ fallback), so new fields are safe to add at the same
  schema as long as they're additive/optional.
- All `serving.*` numeric fields are plain JSON numbers (not the quoted strings tt-smi uses for its
  telemetry). `phase` is a lowercase string enum. `serving` and `progress` may be `null`.

## Where the data comes from on the box

- **`processes`**: a process scan for holders of `/dev/tenstorrent` plus the busiest processes
  (agentd already runs on the box; a `/proc` walk or `sysinfo` gives pid/name/cmd/cpu/mem). Cap the
  list (tt-toplike shows ~12).
- **`inference`**: agentd already **manages** the inference server (it launches `run.py` /
  tracks the serving port), so it knows the container id, model, and lifecycle phase; the
  `serving` block is a scrape of the vLLM `/metrics` endpoint it's already serving on (rates from
  counter deltas over the push interval — same math tt-toplike does). One entry per detected
  inference container.

## Integration point

`crates/tt-station-agentd/src/telemetry.rs` currently returns `tt-smi -s` stdout verbatim and the
`/telemetry` route pushes it. To enrich: parse that stdout as `serde_json::Value`, insert the
`tt_toplike` object (built from the process scan + inference state), re-serialize, and push. Keep
the "run `tt-smi -s` on an interval" loop and the WebSocket route otherwise unchanged. Preserve the
verbatim-tt-smi contract for the telemetry portion — only *add* the key.

## Contract test

Add a test that a frame with the `tt_toplike` key still round-trips as valid tt-smi JSON (the
telemetry portion parses), and that omitting the key is valid. tt-toplike will publish a canonical
example frame from its `--serve` implementation; agentd's output should match that shape so a
tt-toplike `--remote` renders processes + inference identically whether the publisher is another
tt-toplike or agentd.

## Status

- tt-toplike side: `--serve`/`/serve` publisher + `--remote` richer-frame consumer is being built
  now (spec: tt-toplike `docs/superpowers/specs/2026-07-05-serve-broadcast-design.md`). The
  telemetry-only path (today's agentd) keeps working with an honest LOCAL fallback label.
- tt-station side: this brief. No rush — agentd can adopt the extension whenever; tt-toplike
  degrades gracefully until then.

# Code-serving benchmark, QuietBox 2 (p300x2), 2026-10-15

**Status: cancelled partway by the owner. Measured: A (`p150x2-throughput`), A′ (`p150x2`), B (`p150x4`).
B′ (`p150x4-throughput`) is incomplete: only its warm load time (182 s) was measured. No decision is drawn here.**

Package `tt-hous/gpt-oss-20b-p150`, image `tt-model/gpt-oss-20b-p150:e0046b6a0e80-hub1` (2026-10-09-e0046b6a0e80-hub1),
vLLM 0.26.0.dev4. Each config was started with `tt-model serve tt-hous/gpt-oss-20b-p150 --local-only --no-update-check --port 8003 --profile <p>`
under gozer leases on both boards (owner pid: a keeper process), after resets of both boards. Protocol: warm-up, then 3 reps; median (IQR).
S4/S6 not run. Scripts: `bench.py` (S1–S3, S7), `run_config.sh`, `summarize.py`. Raw results: `results-*.json`, `load-*.json`, `s5-*.json`/`s5-*.log`.
Baseline as found: `baseline-state.txt`, `baseline-docker-inspect.json` (secrets redacted).

## Results (measured configs only)

| Config | Load (s) | TTFT short (s) | Decode 1×256 (tok/s) | Batch 8×128 (tok/s) |
|---|---|---|---|---|
| A `p150x2-throughput` | 323.7 | 0.111 (0.007) | 69.1 (0.1) | 274 (9) |
| A′ `p150x2` | 236.9 | 0.106 (0.001) | 68.9 (0.1) | 275 (9) |
| B `p150x4` | 561.6 | 0.072 (0.002) | 86.0 (0.5) | 268 (59) |
| B′ `p150x4-throughput` | 181.7 | – | – | – |

S2: TTFT seconds, median (IQR) of 3; cold tok/s; hit overhead = hit2 − short TTFT

| Config | Size | Tokens | Cold (s) | Cold tok/s | Hit 1 (s) | Hit 2 (s) | Hit overhead (s) |
|---|---|---|---|---|---|---|---|
| A | 5K | 5055 | 1.78 (0.31) | 2838 (581) | 1.27 (0.37) | 0.87 (0.07) | 0.76 |
| A | 16K | 16064 | 2.98 (0.47) | 5394 (869) | 1.28 (0.25) | 0.85 (0.00) | 0.74 |
| A | 32K | 32034 | 6.05 (0.19) | 5301 (181) | 1.45 (0.02) | 1.00 (0.02) | 0.89 |
| A | 64K | 64134 | 13.37 (0.65) | 4798 (214) | 2.25 (1.32) | 1.33 (0.73) | 1.22 |
| A | 96K | 96126 | 21.97 (2.03) | 4375 (362) | 2.04 (0.25) | 1.63 (0.04) | 1.52 |
| A′ | 5K | 5056 | 1.89 (0.34) | 2674 (593) | 1.28 (0.35) | 0.82 (0.07) | 0.71 |
| A′ | 16K | 16064 | 8.04 (3.51) | 1999 (2147) | 1.35 (1.27) | 0.92 (0.48) | 0.81 |
| A′ | 32K | 32031 | 6.37 (0.54) | 5039 (432) | 1.37 (0.28) | 1.02 (0.04) | 0.91 |
| A′ | 64K | 64133 | 13.82 (0.38) | 4640 (127) | 1.72 (0.15) | 1.27 (0.10) | 1.16 |
| A′ | 96K | 96125 | 23.70 (0.77) | 4055 (135) | 2.06 (0.15) | 1.67 (0.04) | 1.57 |
| B | 5K | 5054 | 1.99 (0.56) | 2549 (1491) | 1.55 (2.57) | 0.57 (0.04) | 0.50 |
| B | 16K | 16062 | 1.95 (0.71) | 8236 (1760) | 0.72 (1.27) | 0.66 (0.06) | 0.59 |
| B | 32K | 32033 | 4.96 (0.29) | 6463 (342) | 1.75 (0.77) | 0.79 (0.02) | 0.72 |
| B | 64K | 64136 | 10.98 (5.18) | 5841 (1616) | 2.39 (0.30) | 1.11 (0.12) | 1.04 |
| B | 96K | 96123 | 16.82 (0.48) | 5715 (170) | 1.87 (0.84) | 1.33 (0.44) | 1.26 |

| Config | S3 chat longest gap (s) | S3 short-request p95 TTFT (s) | pooled p95 | cold 64K TTFT under load (s) |
|---|---|---|---|---|
| A | 8.77 (0.21) | 13.95 (0.11) | 14.04 | 14.14 (0.11) |
| A′ | 2.15 (0.83) | 15.83 (5.43) | 24.94 | 16.02 (5.43) |
| B | 3.63 (1.32) | 13.89 (6.29) | 22.55 | 14.09 (6.28) |

S5 (`agent_reliability --histories 10 --tasks 5 --lengths 65536,98304 --seed 1`, seed 1 only, 50 trials per size), valid within one retry:

| Config | 64K | 96K |
|---|---|---|
| A | 96% (48/50) | 92% (46/50) |
| A′ | 100% (50/50) | 92% (46/50) |
| B | 100% (50/50) | 94% (47/50) |

Load (S7) is from the serve command to the first 1-token completion. B's 562 s includes the first p150x4 weight conversion (cold). A, A′ and B′ were warm.

Rough §2 step model (227 output tokens, 64K hit-2 TTFT, 2K tail at cold rate): A ≈ 1.33 + 0.42 + 3.29 ≈ 5.0 s; B ≈ 1.11 + 0.34 + 2.64 ≈ 4.1 s,
about 18% better. That is under the 25% bar, and it is one seed with B′ missing, so treat it as an indication only.

## Caveats and incidents

- S3's short-request series is closed-loop: the first short request waits for the whole cold 64K read on every config. So n=1 per rep,
  and its "p95" is just that one TTFT. The chat stream's longest gap is the reliable S3 number: A 8.8 s, A′ 2.2 s, B 3.6 s.
- B′ attempt 1 failed at boot in `ttnn.get_num_devices()` (`IndexError: unordered_map::at`) after per-board `gozer reset`s
  (`container-B2-failed.log`). This is the failure `community.rs` works around with a full `tt-smi -r`.
  Attempt 2, after a full reset, loaded in 182 s. Then at 20:14:10 a paired client switched TT Station to Qwen3-32B with `--force`
  (audit `model_switch`), which stopped the B′ container during S1 (`container-B2-attempt2-crash.log`). That client stopped the
  switch at 20:16. The run was then cancelled.
- TT Station's agent polls `tt-smi -s` all the time, including during resets and serving (as in normal operation).
- Restore: full `tt-smi -r`, then gpt-oss-20b started again on `p150x2-throughput` with the recorded invocation. Image, Cmd and Labels
  match the baseline, on devices 0 and 1 with port 8003. The one difference is that `HF_TOKEN` is no longer in the container env: the
  current agent doesn't forward it, and the serve is offline. Chips were re-adopted as `tt-station:8003:gpt-oss-20b`: lease 839241
  replaces 74a91e. The agent was restarted so it adopts the server.

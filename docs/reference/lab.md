# Lab mode: tt-lab from the phone (`/lab`) (reference)

[tt-lab](https://github.com/tenstorrent/tt-lab) runs gpt-oss-20b/120b directly on Blackhole
with its own BRISC firmware -- no TT-Metal, no vLLM -- and can check every output logit
bit-for-bit against its host-side device proxy. tt-station surfaces it as Lab mode.

## Setup on the box (once)

```sh
git clone https://github.com/tenstorrent/tt-lab ~/code/tt-lab
cd ~/code/tt-lab && python3.12 ./make.py --env SFPI_PATH=/opt/tenstorrent/sfpi :build
```

`make.py` is hermetic: SFPI is passed with `--env`, not the environment. tt-lab's tested SFPI
is 7.76.0; on this QuietBox the system's 7.83.0 builds, passes `:test`, and runs bit-exact.
The agent looks for `~/code/tt-lab/_out/tt-lab` (`$TT_LAB_BIN`) and models in `~/models`
(`$TT_LAB_MODELS`); `POST /lab/prepare` downloads the GGUF and builds the `.ttq` sidecar.

## Routes (authed)

| Route | |
|---|---|
| `GET /lab` | `installed`, `models: [{id, title, download_gb, tiles, downloaded, ready, preparing}]`, `job`, `running`, `history` (last 20, newest first), `serving` |
| `POST /lab/prepare {model}` | `202`; background `curl -C -` of the GGUF, then `tt-lab requant` (20b: 12.1 GB download, 23.9 GB sidecar, ~4.5 min requant) |
| `POST /lab/run {model, prompt, tokens, tiles, check, profile, stop_serving}` | Server-sent events: `{type:"text", stream, text}` as tt-lab prints, then `{type:"done", ok, exit_code, seconds, stats, previous}` |

A run needs every chip: if a model serves (or the box is split), it's refused with `409`
unless `stop_serving: true` (the user confirmed). It then **resets the chips** -- tt-lab's
firmware never starts on chips TT-Metal used last ("BRISC command did not finish", every
shard's waypoint untouched; seen after a Wan2.2 serve) -- and holds `lab_busy` for the run:
the agent's tt-smi telemetry pauses (tt-lab opens the devices `O_EXCL`; management tools must
not touch them meanwhile), model starts and chip resets are refused. `stats` parses tt-lab's
output: `decode_tps`, `prefill_tps`, `ms_per_token` (check runs: `avg … s/token`; profiled
runs: host command time), `preload_s`, `dram_gib`, `tokens`, `check` (`exact`/`mismatch`),
and `profile` (per-stage share of BRISC cycles).

## Measured on this QuietBox (2026-10-07, gpt-oss-20b)

| Mapping | Check | Device time | End-to-end decode |
|---|---|---|---|
| 8 tiles, one chip | exact, 2 tokens × 201088 logits | 16.5 ms/token | -- |
| 32 tiles, four chips (PCIe P2P) | exact | 5.8 ms/token | 152-158 tok/s, prefill ~198 tok/s |

Profile (32 tiles): experts 42%, attention 14%, logits 12%, q/k/v 10%, RMSNorm 9%. Chips 0 and
2 report their PCIe-aligned Tensix column harvested, so tt-lab uses column 1 there ("may reduce
PCIe performance"), one likely reason these exceed the README's 3.83 ms/token.

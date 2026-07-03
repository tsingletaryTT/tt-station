# tt-inference-server — real launch invocation (reference)

*Researched 2026-07-03, blending the official repo, docs.tenstorrent.com, internal Glean, AND the operator's own validated launch scripts in `~/code/tt-local-generator/bin/start_*.sh`. Drives `crates/tt-station-agentd/src/serving/`.*

## ⭐ Ground truth: launch LLMs via `run.py`, not raw `docker run`

The operator's proven LLM launcher (`start_artgen.sh`) does **not** call `docker run` directly — it invokes `tt-inference-server/run.py`, which builds the container (resolving the model against `model_spec.json`, wiring the device mesh, hugepages, cache binds, and auth). The canonical, validated LLM serve command:

```bash
cd <tt-inference-server repo>
MODEL_SOURCE=huggingface python3 run.py \
  --model <SPEC_NAME> \            # e.g. Qwen3-8B, Llama-3.1-8B-Instruct, Qwen3-32B (model_spec.json names, NOT raw HF ids)
  --workflow server \
  --tt-device <DEVICE> \           # p300x2 (this box, 4x p300c) | p300 (single card) | n300 | p150x4 (BH QuietBox)
  --impl tt-transformers \
  --engine vllm \
  --docker-server \
  --override-docker-image <IMAGE> \
  --no-auth \                      # local dev; omit to require JWT (needs JWT_SECRET in .env)
  --service-port <PORT> \          # host port the OpenAI /v1 server is published on
  --host-hf-cache <HF_CACHE> \     # e.g. $HOME/.cache/huggingface (bind-mounted)
  [--device-id 0,1]                # optional: pin to specific chips
```
Stop is by publish port: `docker ps --filter publish=<PORT> -q | xargs -r docker stop` (mirrors `start_artgen.sh --stop`).

**Repo location** (operator convention): prefer `<checkout>/vendor/tt-inference-server`, else `$HOME/code/tt-inference-server`.

### Device string is box- AND model-specific
- **This box = `p300x2`** (P300X2 machine, 4× p300c). Single-card models (e.g. Qwen3-8B) use `p300`.
- **`p150x4`** is the *other* Blackhole "BH QuietBox" variant — not this box.
- Some models only have a `p300x2` spec; consult `model_spec.json`.

### Real LLM image tags in use (vLLM path)
`ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:`
- `0.14.0-80180b9-7678b70` (preferred, compatible with run.py v0.15.0)
- `0.11.1-bac8b34-7c6685a` (fallback)
- `qb2_launch-555f240-22be241` (v0.10.0 — rejected by v0.15.0 run.py)
- `0.12.0-5b5db8a-e771fff` (for Qwen2.5-7B on n300)

`tt-media-inference-server:<tag>` is a **separate** server (images/video: FLUX, SDXL, Wan, Mochi, Motif) — NOT the LLM /v1 path.

### Auth / health confirmed
- `--no-auth` is what the operator uses for local dev (LLM). JWT_SECRET only needed for the media server / when auth is on.
- Readiness: `GET /health`. Chat: `POST /v1/chat/completions`, models: `GET /v1/models`.

---

## Raw `docker run` (fallback backend only)

*The `run.py` path above is preferred and default. The raw invocation below is a best-effort fallback that does NOT replicate run.py's full mesh/host setup — use only when run.py is unavailable.*

## Canonical `docker run` (direct, no `run.py`)

```bash
docker run -d --rm --name <sanitized-name> \
  --ipc host \
  --device /dev/tenstorrent \
  --mount type=bind,src=/dev/hugepages-1G,dst=/dev/hugepages-1G \
  --volume <cache-volume>:/home/container_app_user/cache_root \
  --env "HF_TOKEN=$HF_TOKEN" \
  --publish <HOST_PORT>:8000 \
  ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:<TAG> \
  --model <org>/<model> --tt-device <device> [--no-auth]
```

Source of truth: `vllm-tt-metal/README.md` "Minimal Example" in the repo, corroborated by the auto-generated command in merged PR #2242.

## Required flags / env / mounts

| Item | Why | Notes |
|---|---|---|
| `--device /dev/tenstorrent` | pass TT accelerator into container | no `--privileged` needed |
| `--mount .../dev/hugepages-1G` | tt-metal needs 1G hugepages for DMA | provisioned on host by tt-installer |
| `--ipc host` | shared memory | used instead of `--shm-size` |
| `--publish H:8000` | OpenAI server listens on **container 8000** | overridable via `$SERVICE_PORT` |
| `--volume <v>:/home/container_app_user/cache_root` | persist weights/HF cache | container user UID 1000 — chown host binds |
| `--env HF_TOKEN=...` | gated HF repos (Llama etc.) | from huggingface.co access tokens |
| `--model <org>/<model>` | **CLI arg after image**, not an env var | resolved vs. bundled model-spec JSON |
| `--tt-device <dev>` | hardware/mesh target | e.g. `n300`, `p150`, and Blackhole combos `p150x4` / `p300x2` |
| `--no-auth` | disable JWT bearer auth (simplest) | else `JWT_SECRET` + client mints a JWT |

## Image

- One **generic** image (model chosen at start via `--model`/`--tt-device`), **not** per-model.
- Path: `ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64`
- **No `latest` tag.** Tags are `<semver>-<tt-metal-commit>-<vllm-commit>` (e.g. `0.9.0-84b4c53-222ee06`). Pin per release.

## Ports / health / readiness

- Container port **8000**; base URL `http://host:8000/v1`.
- Readiness: `GET /health` → 200 when ready (what `run.py` polls). `GET /v1/models` also works as a smoke test.
- Inference: `/v1/completions`, `/v1/chat/completions` (standard vLLM OpenAI routes).
- Bring-up is slow: ~10 min (8B), ~40 min (70B) first run incl. weight download + compile. → health-poll timeout must be generous.

## Host prerequisites / gotchas

- Host needs tt-kmd driver, firmware, `tt-smi`, hugepages (via tt-installer). Verify `tt-smi` before starting.
- ≥360 GB free disk (weights + build artifacts).
- Multi-chip boxes (QuietBox) need `tt-topology` mesh setup once beforehand.
- If a device is wedged from a prior run: `tt-smi -r` reset between `docker stop` and next `docker run`.
- Extra vLLM flags pass straight through after the standard args (`--max-model-len 8192`, `--enable-auto-tool-choice --tool-call-parser llama3_json`, …).

## Decisions taken for the PoC DockerBackend

- **`--no-auth`** by default (no JWT-minting dependency in Rust). `Endpoint.requires_key = !no_auth`.
- **QB2 = 2× p300 = 4 Blackhole chips.** The exact `--tt-device` string for QB2 is not 100% pinned in sources (`p150x4` / `p300x2` seen for Blackhole) — expose it as a **configurable flag**, don't hardcode. Operator confirms on hardware.
- **No `latest`** — image tag is a required-to-set flag; default points at the ghcr release repo with an example tag that MUST be reviewed per release.
- Health poll hits **`/health`** (not `/v1/models`).

## Uncertainties (confirm on hardware)

1. Exact `--tt-device` string for QuietBox 2 (Blackhole).
2. Correct pinned image tag for the model/release being served.
3. Whether the specific model id needs extra vLLM flags (context length, tool-call parser).

## Sources
- https://github.com/tenstorrent/tt-inference-server/blob/main/vllm-tt-metal/README.md
- https://github.com/tenstorrent/tt-inference-server/blob/main/docs/workflows_user_guide.md
- https://github.com/tenstorrent/tt-inference-server/blob/main/docs/prerequisites.md
- https://github.com/tenstorrent/tt-inference-server/pull/2242
- https://docs.tenstorrent.com/getting-started/vLLM-servers.html
- Glean: tt-vscode-toolkit lessons (tt-inference-server, qb2-local-agents)

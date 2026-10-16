# tt-inference-server — real launch invocation (reference)

*Researched 2026-07-03 from the official repo, docs.tenstorrent.com and launch scripts validated on a QuietBox. Drives `crates/tt-station-agentd/src/serving/`.*

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

## Cached-token reporting (`--enable-prompt-tokens-details`)

vLLM puts `usage.prompt_tokens_details.cached_tokens` (prompt tokens served from the prefix
cache) in its replies only when started with `--enable-prompt-tokens-details`. The translator
already reads it into each Code step's speed record (`anthropic::Stream`, `codespeed`).
Since 2026-10-15 the agent adds the flag where the vLLM it launches is known to take it:

| Launch path | Adds the flag? | Why |
|---|---|---|
| Community packages, `tt-model serve` (`community::launch`) | yes, for `kind: vllm-plugin` | `tt-model serve` passes flags it doesn't know to `vllm serve` (checked with `--print`). vLLM added the flag in 0.6.4 (#10174, 2024-11), before the platform-plugin API (#11602, 2024-12) that `vllm-plugin` needs. The gpt-oss-20b package runs vLLM 0.26.0.dev4, and the flag is in its `cli_args.py`. Other packages on the box pin vLLM 0.24 or 0.26. |
| Native vLLM bundles (`native_vllm::serve_argv`) | yes, when the command is a vLLM OpenAI server | It runs the tt-metal vLLM checkout (`~/dispatch/vllm`, merged from upstream 0.17), which has the flag. |
| `run.py` backend (`RunPyBackend`, run.py called directly, incl. a split box) | yes, for chat models with a known tool-call parser | Sent as `"enable_prompt_tokens_details": true` in the one `--vllm-override-args` JSON (with the tool-calling keys). Every tt-inference-server release image on the box has the flag in its vLLM's `cli_args.py` (0.10.0, 0.14.0, 0.17.0, 0.18.0, 0.19.0 and the 0.20.0 studio image; the fork's vLLM reports `0.1.dev14179+g9bd099cbd`). Base checkpoints and media models get nothing. |
| `--serve-with tt` (`tt serve <catalog model>`) | no | `tt serve` takes no vLLM passthrough for catalog models, so the agent can't add it. |
| Docker backend (`DockerBackend`) | no | It passes no vLLM flags at all. |
| Bring-up `serve` step (`bringup.rs`) | no | Not changed. |
| `tt-dit-server` and `vllm-fork` packages | no | `tt-dit-server` isn't vLLM. `vllm-fork` doesn't pass loose flags through to vLLM. |

The flag takes effect at a model's next start. A server that is already running keeps the
flags it started with.

### `/metrics` and `--disable-log-stats`

vLLM exports its prefix-cache counters (`vllm:prefix_cache_queries`/`_hits`) on `/metrics`
only with stats on, which is vLLM's default. Neither run.py nor the images'
`run_vllm_api_server.py` pass `--disable-log-stats`, so the run.py path already exports
them. Native vLLM bundles have it removed from their launch command if they carry it (the
cost is per-step stats bookkeeping on the API process; vLLM's own serving benchmarks run with
stats on). Community packages are out of reach: the gpt-oss-20b package's
`/opt/gpt-oss-20b/entrypoint.py` hard-codes `--disable-log-stats`, the flag is a plain
`store_true` with no `--no-` form in its vLLM (0.26.0.dev4), and `tt-model serve` only
appends, so the package itself has to drop it.

## The Hugging Face token reaches servers as a file

A container's environment is shown by `docker inspect`, and a `systemd-run --setenv`
unit's by `systemctl --user show`. Since 2026-10-15 the agent never puts the token's value
in either. It writes the token to `$XDG_RUNTIME_DIR/tt-station/hf_token` (tmpfs; directory
0700, file 0400, else `~/.config/tt-station/secrets/`) at each launch and points the server
at it with `HF_TOKEN_PATH`, which huggingface_hub reads instead of `$HF_HOME/token`
(verified with hub 1.16.1 in the release images and tt-metal's venv and 2.1.1 in tt-model's
venv, including as uid 1000 inside a release image with the file mounted read-only).

| Launch path | Now |
|---|---|
| Docker backend | `--volume <file>:/run/secrets/hf_token:ro --env HF_TOKEN_PATH=/run/secrets/hf_token` |
| Community `tt-model pull` / `tt-model serve` | `--setenv=HF_TOKEN_PATH=<file>`. tt-model forwards only `HF_TOKEN` into a container, so its containers get no token in their environment; a `--local-only` serve downloads nothing, and a container that needs it still finds `$HF_HOME/token` in the cache tt-model mounts into the container (as /hf). |
| Native vLLM | `--setenv=HF_TOKEN_PATH=<file>` |
| run.py backend and `tt serve` | **Gap, not fixed here.** run.py requires `HF_TOKEN` in its environment for `--docker-server`, writes it to its checkout's `.env`, and starts the container with `--env-file .env`, so the token shows in `docker inspect`. Needs an upstream run.py change (mount the token file, set `HF_TOKEN_PATH`). |

A token change takes effect at the next model start.

## Docker health status of community packages

`tt-model/gpt-oss-20b-p150:e0046b6a0e80-hub1` carries `HEALTHCHECK ...
urlopen('http://127.0.0.1:8000/health')` (and `EXPOSE 8000`), baked in by the package's
own `container/` Dockerfile (the image's source label is
`https://huggingface.co/tt-hous/gpt-oss-20b-p150`; its `entrypoint.py` also hard-codes
`--port 8000`, which `tt-model serve --port 8003` overrides by appending a later `--port`).
Served on 8003, that check always failed, so Docker reported `unhealthy` while the model
served fine. Neither tt-inference-server's images (no `HEALTHCHECK` in any of them) nor
`tt-model serve` had a passthrough for docker flags, so the fix went into tt-model's
`compose_run` (in tt-model-manager): every `tt-model serve` now passes `--health-cmd` checking the port it
publishes, which replaces the image's check. A container started before that change keeps the
old check until the model is restarted. The agent's own readiness checks hit the
serving port directly and don't read Docker's health status. The Docker backend's images
have no healthcheck, and they listen on 8000 inside the container, so nothing needs
overriding there.

## Decisions taken for the PoC DockerBackend

- **`--no-auth`** by default (no JWT-minting dependency in Rust). `Endpoint.requires_key = !no_auth`.
- **The keyless model port is published on loopback only** (2026-10): with the default
  serving host (`127.0.0.1`), the Docker backend publishes `127.0.0.1:<port>:8000` and the
  run.py backend passes `--bind-host 127.0.0.1` (run.py has had it since 2026-03; an older
  checkout without it keeps publishing on every interface). Nothing needs the port from the
  network: the gateway, Chat, Code's gate and the agent's own probes use `127.0.0.1`, and
  containers (apps, the Code sandbox) and other computers go through the gateway
  (`:8765`/`:8766` `/v1`), which checks app keys. A `--serving-host` set to a network address
  keeps the network publish that address needs. `--serve-with tt` launches are built by the
  `tt` CLI and bind however it does.
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

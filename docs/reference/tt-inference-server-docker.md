# tt-inference-server — real Docker invocation (reference)

*Researched 2026-07-03, blending the official repo, docs.tenstorrent.com, and internal Glean sources. Drives `crates/tt-station-agentd/src/serving/docker.rs`.*

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

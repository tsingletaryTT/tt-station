# The official tt CLI as model manager: `/tt/*` and `--serve-with tt` (reference)

*Documents `crates/tt-station-agentd/src/ttcli.rs`, the `/tt/*` handlers in
`crates/tt-station-agentd/src/handlers/ttcli.rs`, and the `tt_cli_serve` launch path in
`crates/tt-station-agentd/src/serving/runpy.rs`. On branch `jberkowitz/mobile-voice`.*

## Why

Tenstorrent's official CLI (`tt`, from `tenstorrent/tt-cli`) already knows which models a
mesh can run, which are downloaded, how to download them, and how to serve them with the
right image and tool-call parser. Per this repo's naming rule ("prefer delegating to it"),
the agent uses it instead of rolling its own — so a phone can browse, download and start
models with nothing but the agent's HTTP API.

## Finding `tt`

At startup the agent looks for `--tt-cli <path>`, else the first `tt` on `$PATH`, else
`~/.tenstorrent-venv/bin/tt`. A candidate only counts if `tt --version` answers
`tt <version>`. Startup logs what it found:

```text
tt-station-agentd: model manager: tt 1.0.1 (/home/ttuser/.tenstorrent-venv/bin/tt); serving with tt serve
```

Without `tt`, the `/tt/*` routes answer `409` and everything else is unchanged.

## Routes

| Route | Auth | What |
|---|---|---|
| `GET /tt/models` | — | `tt --json model list --hw <mesh>`, flattened for this mesh: `{device, disk_free_bytes, models:[{name, hf_repo, model_type, status, supported, engines, tool_call_parser, cached, cache_size_bytes, min_disk_gb, param_count}], pulls:[…]}`. Cached for 10 s (~0.3 s per call). |
| `GET /tt/pulls` | — | `{pulls:[{model, state, error, started_at, finished_at, bytes_on_disk}], disk_free_bytes}` — cheap enough to poll while a download runs. |
| `POST /tt/pull` `{model}` | bearer | Start `tt --json model pull <name>` in the background. `202 started`; `200 already_downloaded`; `404` not in the catalog; `400` not supported on this mesh; `409` already pulling, or less free disk than tt's `min_disk_gb`. `model` may be the tt name (`Qwen3-32B`) or the HF repo (`Qwen/Qwen3-32B`). |
| `POST /tt/pull/cancel` `{model}` | bearer | Kill a running pull. `404` if none. |

A pull's `state` is `running`, `done`, `failed` (with `error` = the last lines `tt` printed,
e.g. a gated-repo refusal) or `cancelled`. `bytes_on_disk` is the size of the model's
HF cache dir (`~/.cache/huggingface/hub/models--Org--Name`), counting blobs once; `tt`
reports no total, so clients show bytes downloaded rather than a percentage. `HF_TOKEN`
(flag/env/profile, as for serving) is passed to `tt model pull` for gated repos. Pull
logs go to `$TMPDIR/tt-station-pulls/pull-<model>.log`.

### `weights`: is it really all here?

`tt`'s `cached` only means "something for this repo is in the HF cache" — a 385 MB stub of
a 62 GB model counts. So every catalog entry also carries the agent's own measurement,
`weights` + `downloaded_bytes`:

| `weights` | Meaning |
|---|---|
| `complete` | No `*.incomplete` blobs, every file named by every `*.index.json` in the snapshot present (sharded safetensors, including diffusers' per-component indexes), and at least one weight file. Ready to serve. |
| `partial` | Some of it: an interrupted or running download, or a config-only stub. |
| `missing` | Nothing in the cache. |
| `container` | Speech-to-text/TTS (tt's container-fetched types) and models without a hub repo: the serving container brings its own weights, so there's nothing to pull (`POST /tt/pull` answers `200 not_needed`). |

`POST /run`, `POST /serve` and the assistant's `run_model` all refuse (`409`) a model
whose weights aren't `complete`/`container`: run.py would otherwise download them *inside*
the launch, silently, past the readiness ceiling.

## `--serve-with tt`

`tt-station-agentd --backend runpy --serve-with tt` launches models with
`tt serve <name> --port <serving-port> [--device <mesh>]` instead of `python3 run.py`.
`tt serve` runs the same `run.py --docker-server` from **its own managed checkout**
(`~/.local/share/tenstorrent/tools/tt-inference-server-v*/src`) with the image and
tool-call/reasoning parsers from its catalog, then streams run.py's output and returns.
Everything around the launch is the runpy backend's as before: stale-container sweep,
`tt-smi -r` board reset, container/log discovery from the streamed output, and the
`/v1/models` readiness gate. `--serving-image` and the agent's own tool-call parser
injection don't apply.

With `--serve-with tt` and no `--tt-inference-repo`, the log routes point at tt's managed
checkout, which is where `workflow_logs/` now land. `/run` takes the tt catalog name
(`Qwen3-32B`); an org-qualified id is stripped to it, since `tt serve` would read
`Org/Name` as a tt-model bundle id.

### Launches never wait on the box's console

run.py's first-run setup for a model asks for `JWT_SECRET` (asserted non-empty even with
`--no-auth`) and `HF_TOKEN` via `getpass`, which opens `/dev/tty` directly. Under an agent
started from a shell, that prompt appeared on the QuietBox's console and the launch hung —
which a phone user can't answer. Every launch now gets `AUTOMATIC_HOST_SETUP=1`, a
`JWT_SECRET` (the operator's `$JWT_SECRET`, else a random per-process value — the server runs
without auth anyway) and `HF_TOKEN` when known; and every agent subprocess runs with stdin
from `/dev/null` in its own session (`setsid`), so anything that still prompts fails fast
instead of hanging. Through `tt serve`, `MODEL_SOURCE` is left to tt (`noaction` for
container-fetched types).

A chip lease (gozer) can't be honoured — `tt serve` has no device selector — so a leased
serve with `--serve-with tt` fails closed.

Verified on a P300x2 QuietBox: Qwen3-32B served through `tt serve` in ~2 min (warm), the
assistant, telemetry and `/logs` working unchanged against it.

## The serving lifecycle: `GET /serve`, `POST /serve`

`/run` holds its request open for the minutes a bring-up takes; a phone backgrounding the
app killed that request, and the agent used to lose the outcome with it (the model came up
while `/status` said `idle`). Now every start runs to completion on its own task and is
tracked:

| Route | Auth | What |
|---|---|---|
| `GET /serve` | — | `{phase: idle\|starting\|serving\|stopping\|failed, model, served_id, since, error, kind}`. `model` is the name it was started as (media servers report a runner id or weights path instead); `kind` is `chat`, `image`, `video`, `speech`, `tts` or `other`. |
| `POST /serve` `{model}` | bearer | Validate (catalog, weights), then start in the background: `202 starting`. `409` while another start or a stop is in flight. |

A failed start keeps its reason in `error` until the next start. At startup the agent
**adopts** a model already serving on its port (e.g. it was restarted under a loaded
model) instead of reading `idle`; `/serving` discovery now also sees media-server
containers (`tt-media-inference-server` images).

## Image and video generation: `/generate/*`

Proxied to the served tt-media-server (which runs `--no-auth`), only while a model of the
right kind serves (`409` otherwise, naming what *is* serving):

| Route | Upstream |
|---|---|
| `POST /generate/image` | `POST /v1/images/generations` — `{prompt, negative_prompt, num_inference_steps, seed, number_of_images, guidance_scale, image_return_format}` → `{images: [base64], generation_time}` |
| `POST /generate/video` | `POST /v1/videos/generations` → a job `{id, status, ...}` (or the MP4 itself if the server runs synchronously) |
| `GET /generate/video/{id}` | job status: `queued`, `in_progress`, `completed`, `failed`, `cancelled` |
| `GET /generate/video/{id}/download` | the MP4 |
| `POST /generate/video/{id}/cancel`, `GET /generate/videos` | cancel; list jobs |

FastAPI `{"detail": ...}` errors come back as this API's `{"error": ...}`. The assistant
answers `409` with a plain explanation while a non-chat model serves.

## Co-serving: chat and images at once (`/coserve`)

A P300x2 box is two P300 cards: chips 0,1 and 2,3 (`tt-smi -ls` shows the board
numbers). Split mode serves a chat model on the first card and an image model
on the second, so the assistant keeps working while the Create tab makes images.

| Route | |
|---|---|
| `GET /coserve` | `{available, reason, split, layout: {chat: "0,1", image: "2,3"}, chat: <lifecycle>, image: <companion lifecycle>, choices: {chat: [{name, ready}], image: [...]}}`. Choices come from `tt --json model list --hw p300`: models tt supports on one card (chat ones must have a tool-call parser, for the assistant). |
| `POST /coserve` `{chat, image}` (authed) | `202`; stops whatever serves, then starts both side by side. `400` for a model that doesn't fit one card (names the options), `409` when one isn't fully downloaded, the box isn't P300x2, or a start/stop is in flight. |

How it works:

- **Placement.** `ServingBackend::set_placement` pins a backend to a slice of
  the box: `--tt-device p300 --device-id 0,1` for run.py (which then mounts
  only `/dev/tenstorrent/0` and `/1` into the container), and a pre-serve reset
  of only those chips (`tt-smi -r 0,1`). Resets are serialized across slots.
  `tt serve` has no chip selector, so a pinned launch runs tt's managed
  checkout's `run.py` directly with tt's venv Python, adding the `--no-auth` and
  `--host-hf-cache` that `tt serve` would.
- **Two slots.** The chat model runs on the agent's normal backend (pinned to
  0,1), so `/status`, `/assistant` and metrics follow it unchanged. The image
  model runs on a *companion* runpy backend on serving port + 1 (8004), with
  its own lifecycle, shown as `companion` in `GET /serve`. `/generate/image`
  goes to the companion when it serves.
- **Leaving split mode is automatic.** Any ordinary start (`/run`, `/serve`, the
  assistant's `run_model`) or `/stop` stops the companion and un-pins the main
  backend first. Each slot's container sweep only touches its own port, so one
  half never kills the other.
- **Restarts.** At startup the agent adopts an image server on the companion
  port and re-pins the main backend to 0,1.
- **Not with gozer**: a split box can't also lease chips, so the companion is
  only built when gozer is absent.

Fits one card on this box (tt catalog): Llama-3.1-8B-Instruct for chat; FLUX.1-schnell
and FLUX.1-dev for images. Qwen3-32B and Wan2.2 need all four chips.

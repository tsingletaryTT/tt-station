# tt-studio ↔ tt-station integration — design study

*Analysis + design only. No code changes are proposed as merged; this doc specs
what to build. Researched 2026-07-03 against the local checkouts of `tt-studio`
(`/home/ttuser/code/tt-studio`, with `/home/ttuser/code/tt-studio-fresh` cross-checked),
`tt-station-agentd`, the `tt` CLI, and the macOS `TTStation` app. All citations are
`path:line` into those repos.*

---

## 0. TL;DR

| Question | Verdict |
|---|---|
| **Q1 — agent starts tt-studio AND makes tt-studio reuse the agent's serving instance / cache** | **Cache-sharing only, and only after a config nudge. True instance-sharing: not possible today.** Both sides launch their *own* `run.py` serving container that binds the whole device mesh; neither can adopt the other's running endpoint. They *can* share the HF **weights** cache (`~/.cache/huggingface`) — both default to it — but tt-studio's FastAPI runs under `sudo` so its `run.py` resolves `/root/.cache/...` unless `HF_HOME`/`HOST_HF_HOME` is pinned. The tt-metal **compile** cache (`persistent_volume/`) is repo-relative and is *not* shared → recompile on each side. |
| **Q2a — agent can START tt-studio on the box** | **Yes, straightforward.** tt-studio is a `run.py`/`startup.sh` + docker-compose stack; the agent can shell it out. Additive. |
| **Q2b — tt-studio's served models show up in the toolbar** | **Yes — this is the recommended first slice.** Add a `GET /serving` route to the agent that scans `docker ps` for *any* `tt-inference-server` `/v1` container (whoever launched it) and surfaces it. Independent of, and far cheaper than, instance-sharing. |

**Recommended first slice: the `GET /serving` discovery/adoption route (§3).** It is
the smallest change that puts tt-studio's models in the toolbar, and it is decoupled
from the hard instance-sharing problem.

---

## 1. tt-studio anatomy (feasibility of Q1 hinges on this)

### 1.1 How tt-studio runs

tt-studio is a multi-container app orchestrated by `run.py`/`startup.sh` +
docker-compose, **plus** two host-side helper processes:

- **docker-compose app stack** — `app/docker-compose.yml`:
  - `tt_studio_backend` — Django/uvicorn, host port **8000**
    (`app/docker-compose.yml:21-24`). This is the brain that deploys models.
  - `tt_studio_frontend` — the React UI (port defined in mode override files).
  - `tt_studio_agent` — a chat agent, host port **8080** (`:100-101`).
  - `tt_studio_chroma` — vector DB, host port **8111** (`:137-138`).
  - Network is an **external** docker network `tt_studio_network`
    (`:150-156`) — the backend manages it itself.
- **`tt-inference-server` FastAPI job service** — a host process, **not** a
  compose service. `startup.sh` clones/updates `tt-inference-server` at branch
  `atupe/studio-fastapi-main` into `${TT_STUDIO_ROOT}/tt-inference-server`
  (`startup.sh:392-409`) and launches it under **sudo** as
  `uvicorn api:app --host 0.0.0.0 --port 8001` (`startup.sh:490-505`), with
  `HF_TOKEN`/`JWT_SECRET` in the env. PID/log go to `fastapi.pid`/`fastapi.log`.
- **`docker-control-service`** — a second host FastAPI (see
  `docker-control-service/`) the backend calls instead of mounting the docker
  socket directly (`app/docker-compose.yml:55-57,76-78`).

### 1.2 How tt-studio deploys a model — it *is* `run.py`, wrapped in HTTP

The tt-studio backend does **not** shell `run.py` itself. It POSTs a JSON job to
the port-8001 FastAPI:

- `FASTAPI_BASE_URL = "http://172.18.0.1:8001"`
  (`app/backend/docker_control/docker_utils.py:30`).
- `run_container(...)` builds a payload `{model, workflow:"server", device,
  docker_server:true, service_port, device_id, ...}` and POSTs it to
  `{FASTAPI_BASE_URL}/run` (`docker_utils.py:333-377`); it expects a `job_id`
  back and then polls `/run/progress/<job_id>` (`docker_utils.py:33-69`,
  `tt_inference_client.py:24-100`).
- The FastAPI turns that into a **`run.py` invocation**. Captured verbatim in
  `tt-studio-fresh/fastapi.log:24`:
  ```
  run.py --model Llama-3.3-70B-Instruct --workflow server --device p150x4 \
         --docker-server --dev-mode --service-port 7000
  ```

**This is the same serving path the agent uses** (`serving/runpy.rs:582-644`):
`run.py --model … --workflow server --docker-server --service-port …`. Both end
up as a `tt-inference-server` vLLM container publishing an OpenAI `/v1` server.
The only differences are (a) tt-studio wraps `run.py` in an async job API and
(b) its default host port is `7000 + device_slot` (`docker_utils.py:330-345`)
vs. the agent's `--service-port` (default **8000**, `main.rs:89`).

> Consequence for Q1: because both sides speak the *same* `run.py`→container
> mechanism, they are interchangeable at the *container* level — which is exactly
> why serving-discovery (Q2b) is easy — but each `run.py` call spins up its **own**
> container that grabs the device mesh, which is why *instance*-sharing is hard.

### 1.3 Where tt-studio's cache / weights live

`tt-inference-server` (the shared dependency) resolves two distinct caches:

- **HF weights cache** (`--host-hf-cache`, read-only weights):
  `workflows/utils.py:410 get_default_hf_home_path()` →
  `HOST_HF_HOME` → `HF_HOME` → **`~/.cache/huggingface`**.
  Note tt-studio's observed `run.py` command (§1.2) passes **no**
  `--host-hf-cache`, so it takes this default.
- **tt-metal compile / model cache** (`CACHE_ROOT`, the container's
  `cache_root`): `workflows/utils.py:421 get_default_persistent_volume_root()` →
  **`<repo_root>/persistent_volume`** — i.e. *repo-relative*
  (`setup_host.py:42-58`, bind `src=host_model_volume_root dst=cache_root`,
  `workflows/run_docker_server.py:290`).

tt-studio also has its own app-level `tt_studio_persistent_volume/` (Django DB,
chroma, deployment records — `app/docker-compose.yml:59,129`), which is unrelated
to model weights.

### 1.4 Ports / containers exposed

| Thing | Host port | Notes |
|---|---|---|
| tt-studio backend (Django) | 8000 | `app/docker-compose.yml:21` — **collides with the agent's default `--service-port 8000`** (`main.rs:89`). See §5. |
| tt-studio agent (chat) | 8080 | `:101` |
| chroma | 8111 | `:138` |
| tt-inference FastAPI job API | 8001 | host process, `startup.sh:504` |
| served LLM `/v1` container | 7000 + slot | `docker_utils.py:330-345` |

---

## 2. Shared-instance / shared-cache feasibility (Question 1)

### 2.1 True instance-sharing — **not possible today**

For tt-studio to "reuse the agent's running `tt-inference-server` instance," the
tt-studio **backend** would need a code path that *adopts an existing `/v1`
endpoint* instead of POSTing `/run` to its own FastAPI. There is none:
`run_container` unconditionally calls `{FASTAPI_BASE_URL}/run`
(`docker_utils.py:371`) and records a `ModelDeployment` keyed to the returned
`job_id` (`docker_utils.py:391-405`). Nothing accepts "here is a base_url that's
already serving model X; treat it as deployed." Symmetrically, the agent has no
API to hand out its endpoint as something tt-studio understands.

Even if the plumbing existed, **the device is the blocker**: a `run.py` serve
grabs the whole mesh (`--tt-device p300x2`, all 4 chips). Two live serving
containers cannot share the same Blackhole mesh; whoever launches second fails on
wedged ethernet cores (documented in `serving/runpy.rs:180-190`,
`start`'s pre-serve reset). So "share the *instance*" and "avoid device
contention" are the same requirement, and it can only be met by **one** serving
container existing at a time — with the *other* front-end adopting it, not
launching its own.

Verdict: **neither side can adopt the other's instance without new code on the
tt-studio side** (a "use external OpenAI endpoint" deployment type). That's a
tt-studio feature request, out of scope for an additive tt-station change.

### 2.2 Cache-sharing — **possible now, but not automatic**

- **HF weights cache: shareable.** Agent default is
  `$HOME/.cache/huggingface` (`main.rs:334-337`); tt-studio's `run.py` default
  is the same (`utils.py:410-418`). So the multi-hundred-GB raw weight downloads
  *can* be shared — **if the paths actually resolve to the same dir.** They do
  **not** by default: tt-studio's FastAPI is launched with **`sudo`**
  (`startup.sh:503`), so its `run.py`'s `Path.home()` is `/root`, giving
  `/root/.cache/huggingface`, while the agent (running as `ttuser`) uses
  `/home/ttuser/.cache/huggingface`. **Fix (actionable): pin
  `HOST_HF_HOME=/home/ttuser/.cache/huggingface`** (highest-priority key,
  `utils.py:414-416`) in the tt-studio FastAPI env, and pass the agent the same
  via `--host-hf-cache` (already its default). Then both bind the identical host
  weights dir → no double-download.
- **tt-metal compile cache: not shared.** `persistent_volume` is repo-relative
  (`utils.py:421-423`), and the two checkouts differ (agent:
  `~/code/tt-inference-server` or `vendor/…`, `main.rs:192`; tt-studio:
  `~/code/tt-studio/tt-inference-server`). Result: each recompiles per model.
  Shareable in principle by pointing both `--host-volume`/`host_model_volume_root`
  at one dir, but the two checkouts are on different branches/versions
  (`atupe/studio-fastapi-main` vs. the agent's) and a shared compile cache across
  mismatched tt-metal builds is risky — **do not share the compile cache** unless
  both are pinned to the same release.

**What the agent would pass to tt-studio to share the weights cache:** nothing on
the agent side — the agent already binds `~/.cache/huggingface`. The lever is on
the tt-studio launch (`HOST_HF_HOME` env for the port-8001 FastAPI). If the agent
*starts* tt-studio (§4), it sets that env at launch.

**Honest verdict for Q1: cache-only (weights), after pinning `HOST_HF_HOME`.**
True instance-sharing is a tt-studio code change (adopt-external-endpoint) and is
not achievable from tt-station alone.

---

## 3. Serving discovery / adoption — the concrete fallback (Question 2b) ⭐

**Goal:** the macOS toolbar lists tt-studio's served model(s) even though the
agent didn't launch them. Achieve it by having the agent detect **any** running
`tt-inference-server` `/v1` container on the box and surface it.

### 3.1 Why this is the right seam

- The toolbar is a veneer over `tt --json` (`macos/README.md:63-88`); `tt`
  fetches box state over HTTP from the agent. So "make it appear in the toolbar"
  = "make the agent report it" = a new agent route + a new `tt` subcommand.
- Today the agent only knows models **it** launched: `AppState` holds a single
  `status: Mutex<ServingStatus>` and one `endpoint` (`routes.rs:105-111`), flipped
  by `set_serving`/`set_idle` on `/run`/`/stop` (`routes.rs:341-364`).
  `ServingStatus` is `Idle | Serving(String)` — **one** model
  (`libttstation/src/model.rs:6-9`). It has no notion of externally-started
  containers. This is the gap to close, **additively**.
- tt-studio already proves the detection heuristic works: it identifies
  `tt-inference-server` containers by the **`CACHE_ROOT` / `TT_CACHE_PATH` env
  vars** (`docker_utils.py:637-644`, again at `:776-779`). We reuse the same
  signal.

### 3.2 Container-detection heuristic

A container is a candidate serving endpoint if **all** hold:

1. **Image match** — repository contains `tt-inference-server` (covers
   `…/vllm-tt-metal-src-release-…`, the tag this repo already keys on,
   `serving/runpy.rs:95`). *Or* env contains `CACHE_ROOT`/`TT_CACHE_PATH` (the
   signal tt-studio itself uses, `docker_utils.py:643`) — belt-and-suspenders for
   odd image names. Use env as the authoritative signal, image as a fast filter.
2. **Published host port** — the container publishes a port to the host (parse
   `docker ps --format '{{.ID}}\t{{.Image}}\t{{.Ports}}'`, or
   `docker inspect … NetworkSettings.Ports`). Skip containers with no published
   port (nothing to probe).
3. **Live `/v1` probe** — `GET http://127.0.0.1:<hostPort>/v1/models` returns
   JSON with a **non-empty `data[]`**. This is the *same* readiness gate the
   agent's own `start` uses (`serving/runpy.rs:680-713`): a non-empty `data[]`
   means vLLM has weights loaded and is answering. The `id` from `data[0]` is the
   authoritative served model id (`runpy.rs:695-705`).

**One-line summary:** *list `docker ps` containers whose image is
`*tt-inference-server*` (or whose env has `CACHE_ROOT`/`TT_CACHE_PATH`) and that
publish a host port, then `GET /v1/models` on each published port and keep the
ones returning a non-empty `data[]`.*

Everything shells through the existing `CommandRunner` seam (`serving/docker.rs`,
reused by `runpy.rs:69`) so it stays unit-testable with a fake.

### 3.3 New route: `GET /serving`

Unauthed, like `/status` and `/models` (read-only discovery,
`routes.rs:769-793,1020-1038`). Lists **all** live serving endpoints on the box,
regardless of launcher, each tagged with a `source`.

```jsonc
// GET /serving  — 200 OK
{
  "box": "quietbox-01",              // AppState.name (routes.rs:276)
  "chips": "4xBH",                   // AppState.chips
  "serving": [
    {
      "model": "meta-llama/Llama-3.3-70B-Instruct", // /v1/models data[0].id
      "base_url": "http://127.0.0.1:7000/v1",       // host:publishedPort + /v1
      "requires_key": false,
      "source": "agent",             // this container matches the agent's own
                                     //   in-memory endpoint (see §3.4)
      "container_id": "9f3c…",       // short docker id
      "image": "ghcr.io/…/vllm-tt-metal-src-release-…:0.14.0-…"
    },
    {
      "model": "Qwen/Qwen3-32B",
      "base_url": "http://127.0.0.1:8000/v1",
      "requires_key": false,
      "source": "external",          // e.g. launched by tt-studio's FastAPI
      "container_id": "1abd…",
      "image": "ghcr.io/…/vllm-tt-metal-src-release-…:0.12.0-…"
    }
  ]
}
```

- `serving` is an **array** (the box *can* physically hold only one live mesh
  container at a time, but the array shape is honest about "0, 1, or — briefly,
  during handoff — more" and avoids reshaping if multi-tenant ever lands). Empty
  array when idle.
- Reuses `Endpoint` fields (`model`, `base_url`, `requires_key`,
  `libttstation/src/model.rs:53-56`) so the Mac/`tt` decode path barely changes.
- `source` ∈ `{"agent","external"}` — see reconciliation below.

### 3.4 Reconciling with the agent's own in-memory state

The agent still tracks its own `status`/`endpoint` (`routes.rs:105-111`). `/serving`
does **not** replace that — it *augments* it:

1. Run the §3.2 scan → set of live `(container_id, base_url, model)`.
2. For each, compare `base_url` against `AppState::endpoint()`
   (`routes.rs:312-318`): a match ⇒ `source:"agent"`; otherwise
   `source:"external"`.
3. If the agent believes it is `Serving(model)` (`routes.rs:303-309`) but no live
   container matches, report it as **not** in `serving[]` (the container died) —
   `/serving` reflects docker reality, while `/status` keeps reporting the
   agent's last intent. Keeping the two independent means `/serving` never has to
   take a write-lock on `status`, and a crashed external container simply drops
   off the list on the next scan.

This is purely additive: `/status`, `/models`, `/run`, `/stop`, `/endpoint` are
untouched; `/serving` is a new read-only view. Wire it in `app()` alongside the
others (`routes.rs:1122-1134`).

### 3.5 Toolbar / CLI surface

- New `tt --json serving --host <h:p>` subcommand (mirrors `models`/`status`,
  `crates/tt/src/main.rs:7-13`), decoding the §3.3 JSON into a `Vec<Endpoint>` +
  `source`.
- The macOS app adds these to the model list under the box, each with the same
  status dot + **Copy endpoint** it already offers (`macos/README.md:89-96`).
  Because it's just more `Endpoint`s over `tt --json`, the "veneer, not a brain"
  rule holds (`macos/README.md:63-67`).

---

## 4. Agent starts tt-studio (Question 2a)

Feasible and additive. tt-studio's own entry point is `run.py`/`startup.sh`
(`startup.sh:5-6`), which brings up the compose stack + the port-8001 FastAPI +
docker-control-service.

**Options (increasing coupling):**

1. **Shell `startup.sh` / `python3 run.py`** in the tt-studio checkout — simplest;
   matches how a human starts it. The agent sets env at launch:
   `HOST_HF_HOME=/home/ttuser/.cache/huggingface` (§2.2 weights sharing) and
   `HF_TOKEN` (it already resolves one, `main.rs:151,358`).
2. **`docker compose -f app/docker-compose.yml … up -d`** for just the app stack,
   plus launching the FastAPI — more control, but re-implements `startup.sh`'s
   sequencing (network creation, sudo FastAPI, port-8001 wait). Prefer (1).

**Control surface:** a new bearer-gated route pair
`POST /tt-studio/start` + `POST /tt-studio/stop` (guarded by `BearerAuth` like
`/run`, `routes.rs:867-884,928-944`), shelling through `CommandRunner`. Keep it a
**separate** route from `/run` — starting tt-studio is a box-level app launch, not
a model serve, and conflating them would muddy `status`.

**Config to pass tt-studio:** checkout dir (default `~/code/tt-studio`),
`HOST_HF_HOME` (weights share), `HF_TOKEN`, and — to dodge the port clash (§5) —
override the agent's own `--service-port` off 8000 *or* run tt-studio and the
agent's `run.py` serving at non-overlapping ports.

**Caveat:** tt-studio's FastAPI wants **sudo** (`startup.sh:491,503`). An agent
running as a normal user can't grant that non-interactively without a sudoers
rule. Document this as a prerequisite; don't try to launch the sudo FastAPI from
the agent unless a passwordless sudoers entry exists for that exact command.

---

## 5. Additivity + risks

- **Device contention (the big one).** The 4-chip Blackhole mesh hosts **one**
  serving container at a time. If both the agent and tt-studio launch `run.py`
  serves, the second wedges the ethernet cores (`serving/runpy.rs:180-190`).
  Mitigations: (a) treat `/serving` (§3) as the *coordination* point — the agent
  can refuse `/run` when `/serving` already shows a live external container; (b)
  never run two serves; adopt, don't relaunch.
- **Port conflicts.** tt-studio backend is on host **8000**
  (`app/docker-compose.yml:21`); the agent's serving default `--service-port` is
  **8000** (`main.rs:89`). If both run, one must move. The agent's `run.py` serves
  and tt-studio's `7000+slot` serves (`docker_utils.py:330-345`) don't overlap by
  default, but the agent's *default* 8000 does collide with tt-studio's backend —
  set the agent's `--service-port` away from 8000 when co-hosting.
- **HF cache races.** Two processes downloading the same weights concurrently
  into one `~/.cache/huggingface` can race. HF's own file-locking mostly handles
  it, but the safe pattern is "warm the cache once, then both read." Sharing the
  weights dir (§2.2) makes the *second* serve a cache hit, which is the point.
- **Compile-cache mismatch.** Do **not** point both at one `persistent_volume`
  unless both `tt-inference-server` checkouts are the same release (§2.2).
- **Sudo.** tt-studio's FastAPI needs root (§4 caveat).
- **Everything above §3 is additive:** `/serving` is a new read-only route; the
  tt-studio start/stop routes are new; no existing route, `AppState` field, or the
  `ServingBackend` trait signature changes. (A `ServingBackend::list_serving`
  default method returning `vec![]` — mirroring `list_models`,
  `serving/mod.rs:91-96` — is a clean place to hang §3.2 so `RunPyBackend`
  overrides it and other backends stay untouched.)

---

## 6. Recommended first slice

**Ship `GET /serving` (§3) + `tt --json serving` + the toolbar list entry.**

Why this first:
- It delivers the owner's concrete ask (Q2b: tt-studio's models in the toolbar)
  **without** depending on instance-sharing (Q1, blocked) or on the agent being
  able to launch tt-studio's sudo FastAPI (Q2a, prerequisite-gated).
- It reuses a **proven** heuristic (tt-studio's own `CACHE_ROOT`/`TT_CACHE_PATH`
  detection, `docker_utils.py:643`) and the agent's **existing** `/v1/models`
  readiness gate (`serving/runpy.rs:680-713`) and `CommandRunner` test seam.
- It is the natural coordination point for the device-contention problem (§5):
  once the agent can *see* every serving container, it can make `/run` refuse to
  double-book the mesh.

Sequence after that: (2) `HOST_HF_HOME` weights-cache pinning (§2.2, config only);
(3) `POST /tt-studio/start|stop` (§4); (4) — only if tt-studio adds an
adopt-external-endpoint deployment type — true instance-sharing (§2.1).

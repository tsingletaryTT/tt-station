# Draft upstream issue: tt-media-server image workers deadlock at startup (fork while the import lock is held)

**Repo:** tenstorrent/tt-inference-server (`tt-media-server`)
**Image:** `ghcr.io/tenstorrent/tt-media-inference-server:0.10.0-555f240` (via tt-inference-server v0.18.0, `tt serve FLUX.1-schnell`)
**Hardware:** QuietBox 2 (P300x2), FLUX.1-schnell on `p300` (`--device-id 0,1` and `2,3`)

## Symptom

`/health` answers `405: Model is not ready` forever. The device worker (`model_runner='tt-flux.1-schnell'`)
logs `get_device_runner: worker_id='0,1'` and nothing after; it has used ~1 s of CPU and ~460 MB RSS
after many minutes (a healthy start burns minutes of CPU and reaches ~23 GB while loading).
Seen in **2 of 3** consecutive starts on one box, and again on a third day.

`py-spy dump` of the device worker:

```
Thread (active): "MainThread"
    acquire (<frozen importlib._bootstrap>:120)
    _lock_unlock_module (<frozen importlib._bootstrap>:224)
    ...
    <module> (PIL/PngImagePlugin.py:45)
    ...
    <module> (matplotlib/colors.py:53)
    ...
    <module> (seaborn/rcmod.py:3)
```

## Cause

`ImageService.__init__` builds `CpuWorkloadHandler("ImagePostprocessing", ...)`, which starts its
worker processes and then `_warmup_workers()` puts a warm-up `PIL.Image` on a `multiprocessing.Queue`.
`Queue.put` hands the object to the queue's **feeder thread**, which pickles it in the background;
pickling a PIL image lazily imports a PIL plugin, holding that module's import lock.

Milliseconds later (`main.py` → `service_resolver().start_workers()`), the scheduler **forks** the device
worker (`multiprocessing` default `fork`). Log timestamps from a hung start: warm-up submitted at
`21:27:28,944`, device worker forked at `21:27:28,951`. The child inherits the import lock in its held
state, but not the thread that holds it, so its own PIL import (via matplotlib/seaborn) blocks forever.

## Suggested fixes (any one)

- Wait for the post-processing warm-up to complete before starting device workers, or warm up
  with a plain bytes payload rather than a PIL image.
- Import `PIL.Image`/plugins (`Image.init()`) eagerly in the parent before any `Queue.put` and fork.
- Start device workers with the `spawn` (or `forkserver`) context instead of `fork`: forking a
  process that already runs threads is unsafe in general.

## Workaround in tt-station

The agent's runpy backend watches media-server starts: after 6 minutes, if every worker of the
`uvicorn … main:app` server has used under 20 s of CPU and stayed under 2 GB, it `docker restart`s the container (at most
5 times). See `media_workers_stalled` in `crates/tt-station-agentd/src/serving/runpy.rs`.

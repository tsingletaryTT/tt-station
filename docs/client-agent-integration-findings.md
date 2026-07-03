# Client ↔ agent integration findings (from real-hardware e2e)

**Date:** 2026-07-03
**Author:** macOS client session (TTStation / `tt` CLI veneer)
**Audience:** the agent-side session working on `crates/tt-station-agentd` and `crates/tt`
**Context:** built the macOS `TTStation` menu-bar app (a veneer over `tt --json`) and ran the
full loop against the live agent at `qb2-lab.local:8765` (p300x2, `4xBH`). The contract holds
end-to-end — discover → models → pair → status → endpoint → `/v1` inference all work, and every
client decoder matches the agent's JSON exactly. Two agent/CLI changes would materially improve
the client; one item is just a confirmation.

## 1. mDNS TXT `status` key is stale — not updated on run/stop (agentd)

**Observed:** while the box was serving `meta-llama/Llama-3.3-70B-Instruct`, `tt discover` over
**mDNS** reported `status:"idle"`, whereas the manual-probe path (`tt discover --host …`) reported
the true `serving:<model>`. The agent advertises `status` in the TXT record but does not
re-publish it when `/run` / `/stop` change the serving state.

**Fix:** re-publish the advertised `status` TXT key whenever serving state changes
(`serving:<model>` on run success, `idle` on stop).

**Why it matters to the client:** the app seeds its per-box status dot from the discovery
record, so a stale TXT shows the wrong dot for a discovered-but-unpaired box until it is paired
and refreshed.

## 2. `tt status` requires a bearer token; only `tt models` is unauthed (tt CLI + agentd)

**Observed:** `cmd_status` / `cmd_endpoint` / `cmd_run` / `cmd_stop` all go through
`authed_client()` (they fail locally with `no token stored for <host>` when unpaired). Only
`cmd_models` is unauthed. But the manual-probe discover path already reads status **without** a
token, so the status data is available unauthed.

**Preferred fix:** make `tt status` unauthed like `tt models` (drop the `authed_client`
requirement in `cmd_status`), assuming the agent's `GET /status` is / stays unauthed.

**Why it matters to the client:** the discovery UI wants a **live** status dot on *unpaired*
boxes. The app currently gates its `status`/`endpoint` calls behind `isPaired` precisely because
`tt status` is authed today (see the client's review-fix "#6"). If `tt status` becomes unauthed,
the client will relax that and show live dots for unpaired boxes instead of relying on the
(currently stale) mDNS TXT. Findings #1 and #2 are complementary; doing either helps, doing both
gives the best discovery UX.

## 3. Confirmation only — HF-style model ids with `/` round-trip correctly

`run meta-llama/Llama-3.3-70B-Instruct` → `status: serving:meta-llama/Llama-3.3-70B-Instruct`
parses correctly on the client (`ServingStatus` strips the `serving:` prefix and keeps the rest,
slashes included). The recent agent change to accept HF model ids works with the client's parser.
No change needed.

## 4. `tt run` returns an endpoint before the model is actually healthy (agentd)

**Observed:** `tt run meta-llama/Llama-3.3-70B-Instruct` returned an `Endpoint`
(`http://qb2-lab.local:8003/v1`) and `/status` immediately reported
`serving:…Llama-3.3-70B`, but `:8003` was **connection-refused** — the vLLM server had not
finished coming up (a 70B on p300x2 is slow to load / may have OOM'd). So a client that
copies the endpoint and hits `/v1` gets a connection error even though the box says "serving".

**Suggested fix:** have `/run` (or the run.py backend) gate the "serving" transition on the
serving container's `/health` actually being ready before returning the endpoint / flipping
status to `serving:<model>`. Otherwise clients need to poll `:8003/health` themselves.

## 5. Authed `GET /status` hangs when the serving backend is down (agentd + AgentClient)

**Observed:** with `:8003` refused (model not up), the **unauthed** `GET /status` returns
instantly (cached status string), but the **authed** path (`tt status` → `AgentClient`) hangs
indefinitely (>2 min). This appears to block on the dead backend somewhere in the authed
handler or client. It made the macOS app's `refresh()` spin forever (the client now guards
this with a subprocess timeout — see below — but the underlying hang is worth fixing).

**Suggested fix:** ensure the authed `/status` path never blocks on backend reachability
(return the same cached status the unauthed path does), or bound it with a short server-side
timeout.

## Not for the agent (tracked client-side)

- macOS **Local Network Privacy**: the app auto-discovery (mDNS via the spawned `tt`) needs the
  app bundle to declare `NSLocalNetworkUsageDescription` (and likely `NSBonjourServices` =
  `_tenstorrent._tcp`) and to be signed such that the LNP grant covers the child `tt` process.
  This is a client packaging fix, not an agent change.
- `RealProcessRunner` pipe-buffer hardening (only if any `tt --json` output ever exceeds ~64KB;
  today all outputs are KB-scale) and cosmetic UI polish.

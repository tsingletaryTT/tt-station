# macOS brief: the app never detects UN-pairing (pairing probe hits an unauthed endpoint)

**Audience:** the Claude working on `macos/TTStation` (coordinated by Taylor).
**From:** the box-side session. **Priority:** user-facing — a reset/unpaired box still shows "paired."

## Symptom

After the box is reset / its bearer tokens are cleared, the macOS app **still shows the box
as paired** and never flips to unpaired, no matter how many times you reset from the box side.

## Root cause (verified on the box)

`macos/TTStation/Sources/TTStationKit/BoxViewModel.swift` `refresh()` derives `isPaired`
from `commands.status()`:

```swift
// BoxViewModel.swift ~line 107-119
do {
    let s = try await commands.status(host: record.hostPort)   // <-- GET /status
    isPaired = true
    registry.markPaired(record.hostPort)
    ...
} catch let e as TTError where commands.isAuthError(e) {
    isPaired = false                                           // <-- never reached for /status
    registry.markUnpaired(record.hostPort)
    ...
}
```

But **`GET /status` is UNAUTHED** on the agent (it's the one endpoint that works without pairing —
that's by design, so `tt status`/discovery work pre-pair). It returns `200` for *any* reachable box
regardless of tokens. So `commands.status()` never throws an auth error → `isPaired` is set to
`true` on every refresh and the `isAuthError` branch is dead code. The doc comment above it
("a successful **authed** status call means the CLI holds a valid bearer token") is factually wrong —
`/status` carries no auth.

**Evidence:** with the box holding **zero** tokens (`~/.config/tt-station/agentd-tokens.json` absent —
confirmed) and `tt status` returning 200, the app still reports paired.

## Fix (Mac-side)

Derive `isPaired` from an **authed** endpoint, not `status()`. On the agent:
- `GET /status`, `/serving`, `/config`, `/catalog` — **unauthed** (work regardless of pairing).
- `GET /endpoint`, `POST /run|stop|reset` — **authed** (`BearerAuth`).

`GET /endpoint` is the natural cheap probe. Its responses:
- **`401` (auth error) → UNPAIRED** — the CLI has no valid token for this box.
- **`409` (Conflict, "idle") → PAIRED** — authed fine, just nothing serving right now.
- **`200` → PAIRED** and serving (carries the Endpoint).

So: keep `status()` for the *display* status (it's unauthed and always works), but set `isPaired`
from an `endpoint()` probe — **treat `409` as paired, only `401`/auth-error as unpaired.** Make sure
`commands.isAuthError` matches `401` specifically and does **not** classify `409` as an auth error
(else an idle paired box would be shown as unpaired).

Sketch:
```swift
// after fetching unauthed status/serving/config for display:
do {
    _ = try await commands.endpoint(host: record.hostPort)     // authed; 200 or 409(idle)
    isPaired = true; registry.markPaired(record.hostPort)
} catch let e as TTError where commands.isAuthError(e) {        // 401 only
    isPaired = false; registry.markUnpaired(record.hostPort)
} catch let e as TTError where commands.isIdleConflict(e) {     // 409 → authed, idle
    isPaired = true; registry.markPaired(record.hostPort)
} catch { /* network/timeout — leave isPaired untouched, as today */ }
```
(Confirm/adjust against how `TTClient.endpoint` currently maps `409` — it may already surface a
distinct "idle" error you can key off; if not, add one. Also fix the misleading comment.)

## Box-side status (already done, on `main`)

- The box's **Reset now actually clears the box's tokens**: the GTK panel's Reset does a hard local
  reset (stop agent → stop serving container → **delete the agent token store** → `tt-smi -r` →
  restart), commit `07e299b`. Previously it shelled `tt reset --host 127.0.0.1` which no-op'd
  (no operator token). `tt console`'s Reset still uses the authed path (shows a "pair first" hint).
- So once this Mac-side probe is authed: box Reset clears the token → the Mac's next `endpoint()`
  probe gets `401` → app flips to unpaired. The two fixes compose.

## Test

Pair the Mac to the box; reset the box (GTK panel Reset, or clear its token store + restart the
`tt-station-agentd` service); refresh in the app → it should now show **unpaired** (the `endpoint()`
probe returns `401`). Also verify an **idle but paired** box still shows **paired** (probe returns
`409`, not misread as unpaired).

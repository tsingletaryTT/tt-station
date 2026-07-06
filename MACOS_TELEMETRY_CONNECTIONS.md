# macOS brief: the app holds TWO telemetry WebSockets open (+ a fmt reminder)

**Audience:** the Claude working on `macos/TTStation` (coordinated by Taylor).
**From:** the box-side session. **Priority:** low now (the box-side CPU hog is fixed), but worth a look.

## What we saw

While debugging high agentd CPU, `ss` on the box showed **two long-lived (WebSocket) connections
from one Mac** to `:8765`:

```
ESTAB 192.168.5.119:8765  192.168.5.165:58056
ESTAB 192.168.5.119:8765  192.168.5.165:54386
```

Each open `/telemetry` WebSocket makes the agent run its telemetry loop independently — so two
connections = two telemetry streams = double the box-side work per second.

## Please check for a WebSocket double-open / leak

Two likely causes on the app side (`TelemetryService` / the read-only telemetry WS):
- The menu-bar popover **and** the control-room window each open their own `/telemetry` socket
  (should share one), **or**
- A reconnect path (box restart, view re-appear, `refresh`) opens a new socket without closing the
  old one — a slow leak that grows past two over time.

Ideal: **one** telemetry subscription per box, shared across views, torn down when no view needs it
(and on box unreachability). If two are intentional, fine — just confirm it's bounded and closes.

## Box-side context (so you know it's not urgent)

The box side was the real problem and is **now fixed** (`perf(agentd): throttle+trim telemetry
process scan`, on `main`): the per-connection process scan was readlinking ~9,000 `/proc/*/fd`
entries every second; it now caches/throttles to once per 3s and only scans the ~12 reported
processes. Measured **~74% → ~1.6%** of a core per connected client. So even two sockets is only a
few percent now — but closing the double-open is still good hygiene (and halves whatever remains).

## Unrelated fmt reminder (recurring)

`cargo fmt --all --check` currently fails on `crates/libttstation/src/catalog.rs`,
`crates/libttstation/tests/agent_client.rs`, `crates/tt/src/catalog.rs`, and
`crates/tt/tests/e2e_mock.rs` — the catalog work landed without running `rustfmt` under the pinned
toolchain. The repo pins **1.96.0** via `rust-toolchain.toml`, so `cargo fmt` on your machine now
produces byte-identical output to the box. Please run **`cargo fmt`** before committing so the
workspace fmt gate stays green — otherwise the box side keeps having to reformat your files (or
leave the gate red). One `cargo fmt` + commit clears it.

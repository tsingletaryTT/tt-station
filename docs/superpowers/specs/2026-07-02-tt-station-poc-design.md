# tt-station PoC — Design Spec

*Date: 2026-07-02 · Author: Taylor Singletary (with Claude Code) · Status: Approved (self-approved per owner request; pending owner revisit)*

## Purpose

Make a Tenstorrent QuietBox 2 on the LAN **discoverable and usable from a Mac as
easily as an AirPlay device**: discover the box, pair once, run a model, and get a
live OpenAI-compatible `/v1` endpoint you can point any client at.

Explicit non-goal for this PoC: **no llama.cpp integration**. "Usable" is delivered
entirely through the OpenAI-compatible `/v1` that `tt-inference-server` (vLLM) already
exposes. Every mainstream Mac LLM client — and plain `curl` — already speaks that
protocol, so the endpoint *is* the product surface.

Cloud-burst to console.tenstorrent.com is **explicitly deferred** to a follow-on spec.

## Context & thesis

~80% of the stack already ships: `dstack` Blackhole support, `tt-inference-server`
(vLLM) `/v1`, `console.tenstorrent.com` `/v1` + keys, and macOS Bonjour/`.local`.
The unbuilt glue is a small **discovery/pairing daemon on the box** plus a **thin
CLI/menu-bar veneer on the Mac**. This spec builds the thinnest *real* end-to-end
slice on actual hardware (a QuietBox 2 on the owner's LAN).

See `CONTEXT.md` for the full fact-checked background and source links.

## Decisions (locked with owner)

| Decision | Choice | Notes |
|---|---|---|
| Hardware target | Real QuietBox 2 on LAN | End-to-end demo possible on real chips |
| First slice | Discovery+pairing CLI **and** menu-bar veneer | "1 and 3 together" |
| Core language | Rust | Matches other TT CLIs (tt-toplike); single binary; menu-bar wraps it |
| Serving control | Box-side agent, Docker now → dstack later | Demo via Docker; dstack is the direction |
| Menu-bar | SwiftUI shell over the Rust CLI | Native feel; all logic stays in Rust |
| Cloud burst | Deferred | Separate follow-on spec |

## Architecture

Three components. All logic lives in the Rust core; the SwiftUI shell is a dumb veneer.

```
┌─ macOS ──────────────┐         ┌─ QuietBox 2 (Ubuntu 22.04) ──────────┐
│  SwiftUI MenuBarExtra │         │  tt-station-agentd (Rust)            │
│      (thin veneer)    │         │   • advertises _tenstorrent._tcp     │
│          │ JSON       │         │   • 6-digit pairing → bearer token   │
│          ▼            │  LAN    │   • ServingBackend trait:            │
│   tt  (Rust CLI/core) │◄──────► │       DockerBackend  (now)           │
│   • discover          │  HTTP   │       DstackBackend  (later, stub)   │
│   • pair → Keychain   │  +JSON  │   • reports vLLM /v1 URL             │
│   • run/stop/endpoint │         └──────────────────────────────────────┘
└───────────────────────┘                        │
                                                  ▼
                                    tt-inference-server (vLLM)
                                    OpenAI-compatible /v1
```

### Component 1 — Rust core (`libttstation` + `tt` CLI)

A single Cargo workspace.

- **`libttstation`** (library crate): the brains.
  - `discovery` — a `DiscoveryProvider` trait with three impls (see below); returns a
    uniform `Box` record regardless of how the box was found.
  - `pairing` — the 6-digit handshake client.
  - `secrets` — token storage abstraction; macOS Keychain impl via `security-framework`,
    plus a file-backed impl for Linux/dev/testing.
  - `agent_client` — typed HTTP+JSON client for the box agent's control API.
  - `config` — known boxes and current default endpoint (small TOML/JSON in a config dir).
- **`tt`** (binary crate): a thin CLI over the library. Global `--json` flag so the
  SwiftUI shell consumes the exact same commands as a human would.

CLI surface:

| Command | Behavior |
|---|---|
| `tt discover` | List reachable boxes (all discovery providers), with status |
| `tt pair [host]` | Pair with a box (mDNS-selected or explicit host); store token in Keychain |
| `tt run <model>` | Ask the box to start serving `<model>`; wait until `/v1` is healthy |
| `tt stop` | Ask the box to stop the current model |
| `tt status` | Authenticated status of the paired box (idle / serving:<model>) |
| `tt endpoint` | Print shell exports for `OPENAI_BASE_URL` (+ key); `--json` for the shell |

### Component 2 — Box-side agent (`tt-station-agentd`, Rust)

Runs on the QuietBox 2 (Ubuntu 22.04). Responsibilities:

- **Advertise** `_tenstorrent._tcp` over mDNS (via `mdns-sd`, or register with the box's
  Avahi). TXT record keys: `name`, `apiver`, `chips` (e.g. `4xBH`),
  `status` (`idle` | `serving:<model>`), `ctrl` (agent control port).
- **Pair**: on request, generate a 6-digit code (printed to the agent's stdout/log/journal),
  accept the code back from a client, and issue a scoped bearer token.
- **Control API** (HTTP+JSON, bearer-authed): `POST /run {model}`, `POST /stop`,
  `GET /status`, `GET /endpoint`.
- **Serving** via the `ServingBackend` trait (below).

### Component 3 — SwiftUI menu-bar veneer (macOS)

`MenuBarExtra` app that shells out to `tt --json`. Shows discovered boxes with status
dots, a model picker, Run/Stop, "Copy endpoint," and "Open Cloud Console." Native
niceties: Keychain access and a `UNUserNotification` when a model reaches ready.
Deliberately contains no business logic.

## Key interfaces (the seams)

### `DiscoveryProvider` (robustness against blocked mDNS)

The notes' own reality-check flags that mDNS is often blocked on corporate LANs, so
discovery is an interface with three providers from day one. They return the same
`Box` record; the CLI and shell never care which one found the box.

```
trait DiscoveryProvider { fn discover(&self) -> Vec<Box>; }
```

1. **mDNS/Bonjour** — primary; the AirPlay-like freebie on macOS.
2. **Manual** — `tt pair <host-or-ip>`; always works, zero network magic.
3. **Tailscale MagicDNS** — the beyond-LAN / corporate-network escape hatch.

### `ServingBackend` (Docker now, dstack later)

The single most important seam: it lets Docker prove the story now and dstack take over
later **without the Mac ever noticing**.

```
trait ServingBackend {
    fn start(&self, model: &str) -> Result<Endpoint>;   // returns when /v1 healthy
    fn stop(&self, model: &str) -> Result<()>;
    fn status(&self) -> Result<ServingStatus>;
}
```

- **`DockerBackend`** (this PoC): run/stop the `tt-inference-server` container; poll
  `/v1/models` until healthy; return the resolved `Endpoint`.
- **`DstackBackend`** (stub now, real in a later milestone): submit a dstack serving
  task to the box's SSH fleet. Selected by agent config.

The agent picks a backend from config; the Mac only ever sees
`starting → ready at <url> → stopped`.

## Data flow (happy path)

1. Mac runs `tt discover` (or the menu-bar refreshes) → `DiscoveryProvider`s return the QB2.
2. `tt pair` → agent prints a 6-digit code → user enters it → agent issues a token →
   stored in Keychain, scoped to that box.
3. `tt run <model>` → agent's `ServingBackend.start(model)` → container up → `/v1` healthy.
4. Agent returns the `Endpoint`; `tt endpoint` prints `export OPENAI_BASE_URL=...`
   (and key if needed). The menu-bar's "Copy endpoint" does the same.
5. User points Cursor / `curl` / any OpenAI client at the endpoint. **Box is usable.**

## Discovery + pairing protocol

- mDNS service type: `_tenstorrent._tcp`. TXT keys: `name`, `apiver`, `chips`,
  `status`, `ctrl`.
- Pairing handshake:
  1. `tt pair` (or menu-bar) calls the agent's pair-init.
  2. Agent generates a 6-digit code, displays it on the box (stdout/journal), and holds
     it briefly with a short TTL.
  3. User enters the code on the Mac; agent verifies and returns a bearer token.
  4. Token stored in Keychain (macOS) / file-backed store (dev), scoped per box.
- All control-API calls carry the bearer token.

**Future path (out of scope, seam preserved):** token issuance can later be replaced by
the OAuth device-authorization flow that ties into `console.tenstorrent.com`, without
changing the CLI/menu-bar surface.

## Security & honest risks (PoC-scoped)

- **Trusted-LAN assumption.** Bearer token over plain HTTP for the demo. TLS/mTLS or
  Tailscale is the real answer; called out, not implemented.
- **New SPOF/authz surface.** A single-owner agent that can start/stop serving is a new
  attack/failure surface (flagged in `CONTEXT.md`). Acceptable for a PoC; documented.
- **mDNS-blocked networks.** Handled by the Manual and Tailscale discovery providers.
- **Token scope.** Tokens are per-box and revocable by restarting the agent (simple
  in-memory/file token set for the PoC).

## Milestones

- **M0 — Discovery.** Agent advertises `_tenstorrent._tcp`; `tt discover` lists the QB2.
- **M1 — Pairing.** 6-digit handshake works; token in Keychain; `tt status` authenticated.
- **M2 — Serve (the "it works" moment).** `tt run <model>` → `DockerBackend` serves →
  `tt endpoint` yields a `/v1` that answers a real `curl` chat completion on the QB2.
- **M3 — Veneer.** SwiftUI `MenuBarExtra` wraps M0–M2 via `tt --json`.
- **M4 — (stretch)** `DstackBackend` behind the same trait. Cloud-burst router is a
  separate follow-on spec.

## Testing strategy

- **Unit** (Rust): discovery record parsing, token store round-trip, agent client
  request/response shapes, TXT-record encode/decode.
- **Mock box** (a dev fixture): a process that advertises `_tenstorrent._tcp` and fakes
  a `/v1` + control API, so the CLI and menu-bar can be developed/tested without the box.
- **Integration (M2 gate):** an actual OpenAI chat completion against the QB2's `/v1`,
  driven end-to-end from `tt run` → `tt endpoint` → `curl`.
- Follow test-driven-development for each unit; each milestone is only "done" when its
  gate behavior is demonstrated (evidence before assertion).

## Repo layout (proposed)

```
tt-station/
  Cargo.toml                 # workspace
  crates/
    libttstation/            # discovery, pairing, secrets, agent_client, config
    tt/                      # the CLI binary
    tt-station-agentd/       # the box-side agent
    mock-box/                # dev fixture: mDNS advertiser + fake /v1 + control API
  macos/
    TTStation/               # SwiftUI MenuBarExtra shell (shells out to `tt --json`)
  docs/superpowers/specs/    # this spec + follow-ons
```

## Out of scope (this spec)

- llama.cpp integration.
- Cloud-burst / local-first placement policy / `tt budget` guardrails.
- dstack orchestration as the *primary* backend (stub only; real in M4).
- Cross-box Ethernet fabric ("TT/IP"), device remoting.
- Multi-user authz, TLS/mTLS hardening.

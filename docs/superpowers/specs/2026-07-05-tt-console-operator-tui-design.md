# tt console — operator TUI + shared agent-lifecycle state machine — design

**Date:** 2026-07-05
**Status:** Approved (self-approved per owner delegation — "self-approve and keep going")
**Author:** Claude (brainstormed with Taylor Singletary)

## Goal

Give box operators working over SSH a polished terminal experience for loading,
unloading, and monitoring the box agent — the SSH-native sibling of the GTK panel.
`tt console` is a ratatui TUI showing the live pairing code, service state, and
serving status, with Start/Stop/Restart/Reset. The agent runs as a **`systemctl --user`
service** (survives SSH disconnect and reboot). The GTK panel and the TUI **share one
agent-lifecycle state machine** so they behave identically, and **all CLI tool names
stay renameable** from a single place.

## Decisions (settled during brainstorming)

1. **systemd user service model.** Start/Stop/Restart = `systemctl --user
   start|stop|restart <unit>`; agent survives SSH/reboot (with `loginctl enable-linger`).
   Reset via the HTTP API. Pairing code read from the journal (no agent change).
2. **`tt console` — Rust/ratatui subcommand of the `tt` crate.** One binary already on
   the box, no runtime deps, reuses `libttstation`. Matches tt-toplike's ratatui.
3. **Shared state machine across the GTK panel and the TUI.** One definition of states,
   data sources, and actions. Single source of truth: a Rust `BoxLifecycleSnapshot`
   exposed as JSON (`tt console --snapshot`) that both front-ends consume; the panel
   migrates from child-supervision to the systemd model.
4. **Configurable tool names (global constraint).** No tool/binary/service name hardcoded
   in more than one place — see [[configurable-cli-tool-names]].

## Non-goals / deferred

- **Chip telemetry graphs** — that's tt-toplike. (A `t` hotkey to launch tt-toplike is a
  possible later touch, not v1.)
- **Remote / non-localhost management** — `systemctl` is box-local; this TUI runs on the
  box you SSH into.
- **SSH-key handshake auth** — a separate Claude on the macOS side is building an SSH key
  handshake the box side will adopt ([[incoming-ssh-key-handshake]]). v1 designs *around*
  today's auth (6-digit code → bearer token) but centralizes the auth touchpoints
  (pairing display, reset token, pair-localhost) so swapping in SSH-key auth later is a
  contained change, not a scatter.
- **App intents / deep links.**

## Forward-compatibility notes

- **SSH-key handshake:** the `Pairing` overlay in the state machine abstracts "a client is
  authenticating"; the reset path and `pair-localhost` action are the only auth-bearing
  touchpoints. Keep them behind the shared actions so the future handshake replaces the
  mechanism without touching the UIs.
- Pull `origin/main` frequently — the macOS agent commits/pushes often.

## Configurable tool names

A single `ToolNames` source of truth (in the `tt` crate, `console::names`), resolved from
env with defaults, threaded everywhere a tool/service name is referenced:

| Field | Env override | Default |
|---|---|---|
| `tt_bin` (CLI binary, for `pair`/`reset` shell-outs + docs) | `TTS_TT_BIN` | `tt` |
| `agent_bin` (agent binary/path, for the unit's `ExecStart`) | `TTS_AGENT_BIN` | `tt-station-agentd` |
| `service_name` (systemd unit) | `TTS_SERVICE_NAME` | `tt-station-agentd.service` |

`ToolNames::from_env()` is the ONLY place these strings are decided. `systemctl`,
`journalctl -u`, the unit template, and the panel all read from it (the panel via its
existing `TTS_*` env, kept in sync). Renaming `tt` → `tt-cli` is a one-env-var / one-default
change.

## Architecture

Three layers, cleanly separated so each is testable in isolation:

### 1. Lifecycle core (the shared state machine)

**Snapshot type** — `libttstation::model::BoxLifecycleSnapshot` (serde; lives in the shared
lib so the JSON contract is one definition and the Mac could consume it later):

```rust
pub struct BoxLifecycleSnapshot {
    pub service: ServiceState,          // from systemctl
    pub reachable: bool,                // did the agent's HTTP answer?
    pub status: Option<ServingStatus>,  // GET /status (carries device_mesh)
    pub name: Option<String>,           // GET /status
    pub chips: Option<String>,
    pub endpoint: Option<Endpoint>,     // serving endpoint, if any
    pub serving: Vec<ServingEntry>,     // GET /serving
    pub config: Option<ConfigSummary>,  // GET /config (active/available profiles)
    pub pairing: Option<PairingState>,  // parsed from the journal
}

pub enum ServiceState { Active, Inactive, Activating, Deactivating, Failed, Unknown }
pub struct PairingState { pub code: String, pub expires_in_secs: u64 }
```

**Derived state** (pure fn `derive_state(&BoxLifecycleSnapshot) -> LifecycleState`):

```rust
pub enum LifecycleState {
    Inactive,          // service not active
    Starting,          // Activating, or Active but agent not yet reachable
    Idle,              // Active + reachable + no model
    Serving(String),   // Active + reachable + serving model X
    Stopping,          // Deactivating
    Failed,            // service failed
}
```
The `Pairing` overlay is orthogonal (can be present in any Active state).

**Collectors** behind a `LifecycleEnv` trait so the logic is unit-testable with a fake
(mirrors agentd's `CommandRunner` pattern):

```rust
pub trait LifecycleEnv {
    fn systemctl_show(&self, unit: &str) -> anyhow::Result<String>; // `systemctl --user show <unit> -p ActiveState,SubState`
    fn journal_tail(&self, unit: &str, lines: u32) -> anyhow::Result<Vec<String>>; // `journalctl --user -u <unit> -n <lines> --no-pager`
    fn http_get(&self, path: &str) -> anyhow::Result<String>;       // localhost agent
    fn now_unix(&self) -> u64;                                      // for pairing TTL
}
```

**Pure parsers** (the bulk of the tests):
- `parse_service_state(systemctl_show_output) -> ServiceState`
- `parse_pairing(journal_lines, now) -> Option<PairingState>` — find the most recent code
  line, compute `expires_in_secs` from the agent's pairing TTL constant; `None` if none
  within TTL. (Same regex the panel uses today.)

**Snapshot assembler** `collect_snapshot(&dyn LifecycleEnv, &ToolNames) -> BoxLifecycleSnapshot`
— combines the collectors; each HTTP field degrades to `None`/`reachable=false` on error
(agent down is a normal state, not an error).

**Actions** `LifecycleActions` (also over `LifecycleEnv` + a `SecretStore` for reset):
- `start/stop/restart()` → `systemctl --user <verb> <unit>`
- `restart_with_profile(profile)` → write a drop-in
  `~/.config/systemd/user/<unit>.d/profile.conf` (`[Service]` `ExecStart=` reset + `--profile <p>`),
  `daemon-reload`, restart. Preserves the panel's existing profile-switch capability under
  systemd; used by both UIs.
- `reset()` → HTTP `POST /reset` with the localhost bearer token (via `libttstation`
  reset path); returns a typed error if no token so the UI can offer `pair_localhost`.
- `pair_localhost()` → read the live code from the snapshot's `pairing`, run
  pair-init/complete against localhost, store the token (enables `reset`).
- `install_service(agent_bin_path)` → render the unit template into
  `~/.config/systemd/user/<unit>`, `daemon-reload`; optionally `enable-linger`.

All actions live in `crates/tt/src/console/` and are the ONLY place `systemctl`/journal/
reset are invoked, so both front-ends share exactly one implementation path.

### 2. `tt console` TUI (ratatui + crossterm)

- `Command::Console { snapshot: bool, install_service: bool }` in `crates/tt`.
- `tt console` → interactive TUI: polls `collect_snapshot` every ~1s, tails the journal for
  the code, renders the layout below, dispatches keybindings to `LifecycleActions`.
- `tt console --snapshot` → prints one `BoxLifecycleSnapshot` as JSON and exits (respects
  global `--json`; always JSON here). This is what the GTK panel consumes.
- `tt console --install-service` → runs `install_service` non-interactively and exits.

**Layout** (left/bottom bars only, per the owner's terminal-UI rule):
```
╔══════════════════════════════════════════════
║  tt-station · qb2-lab          ● service: active
║  ctrl :8765 · p300x2 (4×BH) · profile: stable
╠══════════════════════════════════════════════
║   PAIRING CODE                    ⧗ 1:47 left
║      0 4 2 8 1 7
╠══════════════════════════════════════════════
║  serving: idle          endpoint: —
║  /serving: 1 external (tt-studio :8001)
╠══════════════════════════════════════════════
║  journal ▏ agentd started (backend=runpy)
║          ▏ auto-detected tt-device: p300x2
╚══════════════════════════════════════════════
  [s]tart [x]stop [r]estart [R]eset [p]air [c]profile [i]nstall [q]uit [?]help
```
- Reset (`R`) shows a confirm modal spelling out what it clears (model + all pairings +
  board reset).
- Colors follow the panel's teal-on-dark brand (`#4fd1c5` on `#070d14`).
- Renders through ratatui `TestBackend` in tests (assert buffer cells for key states).
- Graceful degradation: if `systemctl --user` is unavailable, service state shows
  `Unknown`, lifecycle actions surface a clear one-line error, monitoring (HTTP) still works.

### 3. systemd unit + install

Ship `deploy/tt-station-agentd.service` (a template; `%h` = home, `{{AGENT_BIN}}` filled by
install):
```ini
[Unit]
Description=tt-station box agent (QuietBox control plane)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={{AGENT_BIN}}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```
The agent reads `~/.config/tt-station/agentd.toml` for name/ctrl_port/profile, so the unit
needs no flags. `install_service` renders `{{AGENT_BIN}}` from `ToolNames.agent_bin`
(resolved to an absolute path), writes to `~/.config/systemd/user/<service_name>`, and
`daemon-reload`s. Docs cover `enable-linger` for boot survival.

### 4. GTK panel migration

The panel adopts the shared state machine so both UIs match:
- **Lifecycle:** replace `Popen`/`SIGINT` child-supervision (`start_agent`/`stop_agent`/
  `restart_agent`) with `systemctl --user start|stop|restart <service_name>`. Closing the
  panel no longer kills the agent.
- **State:** replace child-stdout parsing with `tt console --snapshot` (JSON) polled on the
  existing timer — the panel now renders the SAME snapshot the TUI does (single source of
  truth for service state, pairing code+TTL, status/endpoint/serving/profile).
- **Profile dropdown:** switching a profile calls the shared drop-in path (via
  `tt console`… or directly writing the drop-in + `systemctl --user restart`), preserving
  today's capability under systemd.
- **Reset / autostart / tooltips / branding icon:** unchanged (reset already shells `tt`).
  `TTS_AUTOSTART` becomes "ensure the service is started" rather than spawning a child.
- Config it reads (`TTS_*`) stays; `TTS_SERVICE_NAME` is added and kept consistent with
  `ToolNames`.

## Data flow

```
systemctl --user show ─┐
journalctl --user -u  ─┼─► collect_snapshot ─► BoxLifecycleSnapshot ─┬─► tt console TUI (render + keys → actions)
GET /status,/serving, ─┘        (LifecycleEnv)                        └─► `tt console --snapshot` (JSON) ─► GTK panel
    /config (localhost)
```
Actions (both UIs) → `LifecycleActions` → systemctl / drop-in / HTTP reset.

## Error handling

| Situation | Behavior |
|---|---|
| Agent HTTP unreachable | `reachable=false`; status/endpoint/serving/config `None`; UI shows "agent not responding" but service state still rendered. |
| `systemctl --user` missing/no user bus | `ServiceState::Unknown`; lifecycle actions error with a clear message; monitoring still works. |
| Reset with no localhost token | Typed error; UI offers `pair-localhost` then retry. |
| Journal has no recent code | `pairing = None`; UI shows "no active pairing". |
| Drop-in profile switch on a box with no config profiles | Action is a no-op/disabled; UI hides profile switch when `available_profiles` is empty. |
| `--install-service` when unit exists | Idempotent (rewrite only if content differs); report what changed. |

## Testing

- **Pure parsers (bulk):** `parse_service_state` (each ActiveState/SubState combo →
  enum), `parse_pairing` (recent-code extraction + TTL math + expiry drop), `derive_state`
  (each snapshot shape → `LifecycleState`).
- **Snapshot assembler:** with a fake `LifecycleEnv`, assert HTTP-error fields degrade to
  `None`/`reachable=false` and a healthy env yields a full snapshot.
- **Actions:** fake `LifecycleEnv` asserts the exact `systemctl`/`journalctl` argv and the
  drop-in file content; reset-without-token returns the typed error.
- **Names:** `ToolNames::from_env` precedence (env override vs default) for all three.
- **TUI render:** ratatui `TestBackend` buffer assertions for Inactive / Idle / Serving /
  Pairing / agent-unreachable states.
- **Snapshot JSON:** `tt console --snapshot` round-trips to `BoxLifecycleSnapshot`
  (extend the mock-box e2e where practical).
- **Panel:** `read` helpers (snapshot JSON parse) unit-checkable in Python; systemctl/GUI
  paths are owner-verified live over SSH (like the Mac app's LaunchController).
- Manual: SSH into the box, `tt console`, exercise start/stop/restart/reset/pair, confirm
  the agent survives disconnect; confirm the panel shows the identical state.

## Rollout

1. Land the lifecycle core + `tt console` + unit + install (additive; nothing else changes).
2. Install the user service on the box; verify TUI over SSH.
3. Migrate the GTK panel to the shared snapshot + systemd model (its own task, independently
   reviewable) — the one behavior change (panel no longer owns the agent child).

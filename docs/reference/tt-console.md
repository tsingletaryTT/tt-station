# `tt console` — operator TUI (reference)

*Sourced from the shipped code: `crates/tt/src/console/{names,state,env,actions,ui,mod}.rs`,
`crates/tt/src/main.rs` (`Command::Console`), `deploy/tt-station-agentd.service`,
`crates/libttstation/src/model.rs` (`BoxLifecycleSnapshot`), and `box-panel/tt-station-panel.py`.
Live SSH click-through of the interactive TUI is **owner-verified, not automated** — the
test suite covers the pure line-builders and a `ratatui::TestBackend` render, plus unit
tests for the collector/actions against fake environments (no real `systemctl`/`journalctl`/
HTTP in CI).*

## What it is

`tt console` is a ratatui/crossterm terminal UI for loading, unloading, and monitoring
**this box's own agent** (`tt-station-agentd`) — the SSH-native sibling of the GTK box
panel. Run it directly on the box (over SSH or on its own console); it always talks to
`127.0.0.1:<ctrl-port>` and to the local `systemctl --user`/`journalctl --user`, never a
remote box. There is no `--host` flag, unlike every other `tt` subcommand.

```
tt console                                  # launch the interactive TUI
tt console --snapshot                       # print one BoxLifecycleSnapshot as JSON and exit
tt console --install-service                # install/refresh the systemd user unit and exit
tt console --ctrl-port 8765                 # agent control port on 127.0.0.1 (default 8765)
```

`--install-service` takes priority over `--snapshot` if both are passed (an odd
combination, but not rejected); with neither flag, the interactive TUI launches.

## The systemd user-service model

The agent runs as a `systemctl --user` service so it survives an SSH disconnect (and, with
`loginctl enable-linger` — see below, reboot). Start/Stop/Restart in `tt console` are
literally `systemctl --user start|stop|restart <unit>` — there is no child-process
supervision anymore; closing the TUI (or an SSH session) does **not** stop the agent.

- **Install:** `tt console --install-service` renders `deploy/tt-station-agentd.service`
  (baked into the `tt` binary at compile time via `include_str!`, so the binary needs no
  copy of `deploy/` on the target box) into
  `~/.config/systemd/user/<service_name>` (or `$XDG_CONFIG_HOME/systemd/user/...` if that's
  set), filling in the `{{AGENT_BIN}}` placeholder with the resolved absolute path to the
  agent binary, then runs `systemctl --user daemon-reload`. It's idempotent: if the file's
  content is already identical, it's left untouched (no needless reload-triggering rewrite).
- **Agent binary resolution:** scan `$PATH` first, then fall back to
  `$HOME/.local/bin/<agent_bin>` (matches this project's documented install convention),
  then the bare name unresolved as a last resort (a systemd unit with an unresolved
  `ExecStart=` will simply fail to start until fixed — a clearer failure mode than refusing
  to install).
- **Boot survival:** `tt console --install-service` does **not** run `loginctl
  enable-linger` itself — that's a one-time manual step the operator runs once per user so
  the systemd `--user` instance (and this unit) keeps running after logout / across reboots
  without an active login session:
  ```
  loginctl enable-linger "$USER"
  ```
  Without linger, a `--user` unit is torn down when the last session for that user ends.
- **The unit template** (`deploy/tt-station-agentd.service`):
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
  The agent takes no CLI flags in the unit — it reads its own config
  (`~/.config/tt-station/agentd.toml`) for name/ctrl-port/profile.
- **Profile pinning:** pressing `c` in the TUI (or the panel's equivalent) doesn't edit the
  unit file directly — it writes a systemd drop-in at
  `~/.config/systemd/user/<unit>.d/profile.conf`:
  ```ini
  [Service]
  ExecStart=
  ExecStart=<agent_bin> --profile <profile>
  ```
  (the blank `ExecStart=` first clears the unit's original `ExecStart=`, since systemd
  otherwise treats a drop-in's `ExecStart=` as an ADDITIONAL command rather than a
  replacement), then `daemon-reload` + `restart`.

## Keybindings

From `crates/tt/src/console/ui.rs` (`footer_lines`/`event_loop`) — this is the literal
footer text the TUI renders:

```
s start  x stop  r restart  R reset  p pair  c profile  i install  q/Esc quit
```

| Key | Action |
|---|---|
| `s` | Start the agent (`systemctl --user start <unit>`) |
| `x` | Stop the agent (`systemctl --user stop <unit>`) |
| `r` | Restart the agent (`systemctl --user restart <unit>`) |
| `R` | Reset — opens a confirmation modal (`Reset this box? This clears pairing + serving. [y/N]`); only `y`/`Y` proceeds, anything else cancels |
| `p` | Pair-localhost — uses the pairing code the collector already found in the journal |
| `c` | Cycle to the next profile in `config.available_profiles` (wraps around) and apply it via the profile drop-in + restart |
| `i` | Install/refresh the systemd unit (same as `--install-service`, run from inside the TUI) |
| `q` / `Esc` | Quit the TUI (only in normal mode, not while the reset confirmation is open) |

Every action result (`ok` or an error message) is shown as a one-line overlay at the
bottom of the screen until the next keypress. The snapshot also auto-refreshes on a ~1s
tick even with no key pressed.

Terminal safety: raw mode + the alternate screen are entered/exited via an RAII guard
(`TerminalGuard`) whose `Drop` impl restores the terminal on every exit path — including a
panic — so a bug can't leave an operator's SSH session stuck in raw mode.

## `--snapshot` JSON contract

`tt console --snapshot` prints one `BoxLifecycleSnapshot` (from
`libttstation::model`) as pretty JSON and exits. This is the **single shared wire
contract** — the interactive TUI, `--snapshot`, and the GTK box panel (which polls `tt
console --snapshot --ctrl-port <port>` as a subprocess instead of re-implementing the
systemctl/journalctl/HTTP collection logic in Python) all consume exactly this shape, so
all three surfaces can never disagree about what state the box is in.

```rust
pub struct BoxLifecycleSnapshot {
    pub service: ServiceState,            // from `systemctl --user show <unit>`
    pub reachable: bool,                  // did GET /status succeed?
    pub name: Option<String>,             // from GET /status
    pub chips: Option<String>,            // from GET /status
    pub status: Option<ServingStatus>,    // from GET /status ("idle" | "serving:<model>")
    pub endpoint: Option<Endpoint>,       // always None today (see below)
    pub serving: Vec<ServingEntry>,       // from GET /serving
    pub config: Option<ConfigSummary>,    // from GET /config
    pub pairing: Option<PairingState>,    // parsed from the journal tail
}

pub enum ServiceState { Active, Inactive, Activating, Deactivating, Failed, Unknown }
// serializes snake_case: "active" | "inactive" | "activating" | "deactivating" | "failed" | "unknown"

pub struct PairingState {
    pub code: String,           // the 6-digit pairing code
    pub expires_in_secs: u64,   // always full TTL (120s) for a freshly-tailed sighting —
                                 // `journalctl -o cat` strips timestamps, so there's no way
                                 // to compute how long ago a line was actually logged
}

pub struct ConfigSummary {
    pub active_profile: Option<String>,
    pub available_profiles: Vec<String>,
    pub backend: String,
    pub serving_host: String,
    pub serving_port: u16,
    pub serving_image: Option<String>,
    pub tt_inference_repo: Option<String>,
    pub tt_device: Option<String>,        // None = auto-detected
}

pub struct ServingEntry {
    pub model: String,
    pub base_url: String,
    pub host_port: u16,
    pub container: String,
    pub source: String,                   // "agent" | "external"
}

pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    pub requires_key: bool,
}
```

Notes on how the snapshot is assembled (`console::env::collect_snapshot`):

- **The agent being down is a normal state, not an error.** `service` still reflects
  whatever `systemctl` reports (independent of whether the HTTP API answers — e.g. `active`
  but wedged is representable). `reachable` is exactly "did `GET /status` succeed." Every
  other HTTP-sourced field independently degrades to `None`/empty on its own connection or
  parse error — one field's failure never blocks the others.
- **`endpoint` is always `None` in v1.** `GET /endpoint` is an authed route and the
  collector only makes unauthenticated probes (`/status`, `/config`, `/serving`); it has no
  bearer token in scope. This is a known limitation, not a bug.
- **`pairing` comes from the journal, not HTTP** — `journalctl --user -u <unit> -n 40
  --no-pager -o cat`, scanning for the most recent line matching
  `tt-station-agentd: pairing code: NNNNNN` (case-insensitive "pairing"/"code" + a
  standalone 6-digit run). This means a crashed-but-recently-logged agent can still surface
  its last pairing code even while `reachable` is `false`.

## Configurable tool names

Every tool/service name `tt console` shells out to is resolved once, from
`crates/tt/src/console/names.rs::ToolNames::from_env()` — a single source of truth so a
future rename is a one-place change:

| Field | Env override | Default |
|---|---|---|
| `tt_bin` (the `tt` CLI binary, used for the `reset`/`pair` shell-outs below) | `TTS_TT_BIN` | `tt` |
| `agent_bin` (agent binary name/path, used to build the unit's `ExecStart=`) | `TTS_AGENT_BIN` | `tt-station-agentd` |
| `service_name` (the systemd unit name) | `TTS_SERVICE_NAME` | `tt-station-agentd.service` |

An empty-string override is treated as unset and falls back to the default. The GTK panel
reads the same `TTS_*` env vars independently (see `box-panel/tt-station-panel.py`), kept
in sync with these defaults by convention — there is currently no single runtime source
both processes read from (each resolves its own copy of `ToolNames`/`TTS_*` at startup).

## Reset precondition: localhost pairing

`POST /reset` on the agent is bearer-guarded — the same auth as `/run`/`/stop`/`/endpoint`.
`tt console` does not duplicate the agent's HTTP client or auth logic: pressing `R` (after
confirming) and `p` both shell out to the `tt` binary itself
(`tt reset --host 127.0.0.1:<ctrl-port> --yes` and `tt pair 127.0.0.1:<ctrl-port> --code
<code>` respectively), reusing the CLI's own token store as the one auth touchpoint.

- If `tt console` (or the underlying `tt` CLI) has **no bearer token stored for
  `127.0.0.1:<ctrl-port>`**, `R`eset fails with a message hinting at the fix:
  `reset failed: <error> (no token for this box? press 'p' to pair)`.
- Press `p` to **pair-localhost** first: this uses the pairing code the collector already
  found in the agent's journal (`snap.pairing`) and runs `tt pair` against
  `127.0.0.1:<ctrl-port>` with that code, storing a token locally. If no pairing code is
  currently active, `p` reports `no pairing code available -- start a pairing on the box
  first` instead of attempting anything (a pairing must be initiated on the box — or the
  GTK panel — first; see `pairing_lines` in `ui.rs`).
- Once paired, `R` → confirm (`y`) actually resets: **stops the currently-served model,
  clears ALL issued bearer tokens** (in-memory + the persisted token store — invalidating
  every paired client, including the one that just requested the reset), and **resets the
  board** (`tt-smi -r`, via the serving backend's own reset path). This is the same
  `cmd_reset`/`POST /reset` semantics as `tt reset` everywhere else in the CLI — `tt
  console` doesn't reimplement it, just triggers it.

## Related

- The GTK box panel (`box-panel/tt-station-panel.py`) shares this exact model: it polls `tt
  console --snapshot --ctrl-port <port>` on a timer instead of parsing child-process
  stdout, and its Start/Stop/Restart buttons shell the same `systemctl --user <verb>
  <service_name>` commands `tt console` uses. The panel and the TUI can never disagree
  about box state.
- `docs/reference/agentd-config.md` — the `GET /config` / `tt config` contract that feeds
  `BoxLifecycleSnapshot.config`.

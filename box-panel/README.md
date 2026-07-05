# tt-station box panel

A tiny GTK4 control surface that runs **on the QuietBox** — the physical box's
little face for tt-station. Not a dashboard (that's a different tool); just
enough to know "hey, it's working," pair a client, and start/stop the agent.

## The systemd model (single source of truth)

The agent (`tt-station-agentd`) runs as a `systemctl --user` service
(`tt-station-agentd.service`) — the **same lifecycle model `tt console`** (the
operator TUI) uses. The panel does **not** spawn or supervise a child process:

- **Start · Stop · Restart** just shell `systemctl --user start|stop|restart
  <unit>`.
- **All rendered state** — service state, pairing code + TTL, serving
  status/endpoint, active profile — comes from a single poll (every 2s) of
  `tt console --snapshot`, which prints one `BoxLifecycleSnapshot` JSON. This
  is the exact same JSON `tt console`'s interactive TUI renders, so the panel
  and the TUI can never disagree about what state the box is in.
- Closing the panel window does **not** stop the service — it only stops
  watching it. The agent keeps running (and serving, if it was) independent
  of whether anything is polling its state.

If `tt console --snapshot` itself fails (missing `tt` binary, no systemd user
session, unparseable output — e.g. before `tt console --install-service` has
ever been run), the panel renders a safe "unknown / can't read box state"
pill rather than crashing.

It shows:
- the **6-digit pairing code, big**, the instant a client pairs (read from
  the snapshot's `pairing` field), with a TTL countdown
- a **status pill** + line: stopped / failed / starting… / stopping… / idle /
  `serving:<model>`, chips, and the `/v1` endpoint when a model is up
- **Start · Stop · Restart** the `tt-station-agentd.service` systemd unit
- a **profile dropdown**, read from the box-local `agentd.toml` (the agent's
  named-profile config file — see the agentd config-profiles doc), populated
  from the file's `[profile.*]` table names, defaulting to `default_profile`.
  An **Apply** button next to it pins the selection by writing a systemd
  drop-in (`~/.config/systemd/user/<unit>.d/profile.conf`, in the exact
  format the Rust `tt console` side uses), reloading the systemd user
  manager, and restarting the service so it takes effect. If there's no
  config file (or it fails to parse), the dropdown+button are hidden.
- an **active profile line**, straight from the snapshot's `config` field
  (falls back to the dropdown's current pick while the agent is still
  starting), so what's actually serving is visible even if the dropdown
  selection hasn't been Applied yet
- **Reset** — return the box to a fresh state via `tt reset` (stop model, clear
  pairings, reset the board) — unchanged; this still talks to the agent's own
  HTTP control API directly, independent of the systemd plumbing above

## Run

    python3 box-panel/tt-station-panel.py

Requires GTK4 + PyGObject (`python3-gi`, `gir1.2-gtk-4.0` — present on this box)
and a `tt-station-agentd.service` systemd `--user` unit installed (see
`tt console --install-service`, or `deploy/tt-station-agentd.service`). Run it
from the repo root so the default `tt` binary path (`./target/release/tt`)
resolves, or set the env vars below.

## Config (env vars, sensible box defaults)

| Var | Default | Meaning |
|-----|---------|---------|
| `TTS_SERVICE_NAME` | `tt-station-agentd.service` | the systemd `--user` unit to start/stop/restart/poll — matches the Rust `console::names::ToolNames::service_name` default |
| `TTS_AGENT_BIN` | `tt-station-agentd` | binary name/path baked into a profile drop-in's `ExecStart=` line — matches `ToolNames::agent_bin`; **not** used to launch a process directly anymore |
| `TTS_TT_BIN` | `./target/release/tt` | `tt` CLI, used for `tt console --snapshot` (state) and `tt reset` |
| `TTS_NAME` | `qb2-lab` | box name, shown in the window title |
| `TTS_CTRL_PORT` | `8765` | `--ctrl-port` passed to `tt console --snapshot` and used for `tt reset --host` |
| `TTS_SERVING_HOST` | `<name>.local` | fallback endpoint host, used only when the snapshot has no `endpoint` yet |
| `TTS_SERVING_PORT` | `8003` | fallback endpoint port (see above) |
| `TTS_REPO` | `~/code/tt-inference-server` | base dir for `TTS_HF_ENV`'s default (display-only now) |
| `TTS_HF_ENV` | `<repo>/.env` | file to read `HF_TOKEN` from (display-only — the systemd service's own environment is what the agent actually uses) |
| `TTS_CONFIG` | `$TT_CONFIG_DIR/agentd.toml` or `~/.config/tt-station/agentd.toml` | box-local `agentd.toml` path, read for the profile dropdown |
| `TTS_AUTOSTART` | *(unset)* | `1` → `systemctl --user start` the service as soon as the panel opens |

`TTS_SERVICE_NAME` and `TTS_AGENT_BIN` deliberately mirror the exact env var
names and defaults the Rust `tt console` binary resolves via
`console::names::ToolNames::from_env()` — a box that doesn't override either
one has the panel and `tt console` agreeing on which unit "the agent" is
without any extra configuration.

## Named profiles

If `agentd.toml` (at `TTS_CONFIG`, or the agent's own default path) defines
one or more `[profile.<name>]` tables, the panel shows a **profile:**
dropdown populated with their names (sorted), pre-selected to the file's
`default_profile` (or the first one), plus an **Apply** button. Clicking
Apply writes `~/.config/systemd/user/<TTS_SERVICE_NAME>.d/profile.conf`:

```
[Service]
ExecStart=
ExecStart=<TTS_AGENT_BIN> --profile <selected>
```

(the blank `ExecStart=` line clears the unit's original one — systemd
otherwise treats a drop-in's `ExecStart=` as an *additional* command to run,
not a replacement), then runs `systemctl --user daemon-reload` and
`systemctl --user restart <TTS_SERVICE_NAME>`. The status area's **active
profile:** line then reflects the snapshot's `config.active_profile` — the
ground truth for what's actually serving, independent of whatever the
dropdown currently shows. With no config file (or a malformed one), the
dropdown and Apply button are hidden entirely.

# TTStation box workbench launchers — design

**Date:** 2026-07-04
**Status:** approved (owner directed "keep planning/approving/iterating" toward an end-to-end demo)
**Builds on:** the window surface (`BoxWorkspaceView`) + the existing Connect launchers
(`OpenWebUILauncher`/`OpenCodeLauncher`/`LaunchController`).

## Goal

Make the Mac app the hub that ties the room together: from a discovered box, one click each to
open box-connected tools. Extend the window's workspace with a **Workbench** row of launchers,
all keyed off the box the app already knows (its host + control port), reusing the pure-builder +
`LaunchController`-glue pattern.

New launchers (the existing Open WebUI / opencode stay as the "serving model" launchers):
- **Terminal → box** — Terminal.app SSH'd into the box.
- **tt-toplike** — Terminal.app running `tt-toplike-tui --remote <host>:<ctrlPort>` (live telemetry).
- **VSCode → box** — a VSCode Remote-SSH window on the box (integrated terminal on the box; the
  `tenstorrent.tt-vscode-toolkit` extension is installed locally).

## What these key off

Unlike Open WebUI/opencode (which take the serving `Endpoint`'s `/v1` base_url), these take the
**box itself**: `host` + `ctrlPort` + an SSH `user`. Source of truth:
- `host` = the box's canonical host from `BoxRecord` (the mDNS/manual host, trailing `.` stripped —
  `BoxRecord.hostPort` already strips it; expose the host component).
- `ctrlPort` = `BoxRecord.ctrlPort` (e.g. 8765) — for tt-toplike's `--remote`.
- `user` = SSH user, default `NSUserName()` (the Mac user, e.g. `tsingletary`), overridable via
  `UserDefaults` key `tt.sshUser`.

So these launchers are available whenever a box is **known** (paired or not — SSH and telemetry
don't require pairing). They live in the workspace as a "Workbench" section shown for any selected
box.

## Architecture (mirrors the existing launchers)

### Pure builders in `TTStationKit` (unit-tested)
- **`TerminalSSHLauncher.command(user:host:) -> String`** →
  `ssh -o StrictHostKeyChecking=accept-new '<user>@<host>'` (accept-new so a first connect to a
  box whose host key isn't yet known doesn't hard-fail; the interactive shell still prompts for a
  password if key auth isn't set up — that's fine).
- **`TTToplikeLauncher.command(host:ctrlPort:) -> String`** →
  `tt-toplike-tui --remote '<host>:<ctrlPort>'`.
- **`VSCodeLauncher.remoteArgs(user:host:path:) -> [String]`** →
  `["--remote", "ssh-remote+<user>@<host>", "<path>"]` (path default `/home/<user>`).
- **`SSHTarget`** helper: `struct SSHTarget { let user: String; let host: String }` with a
  `static func resolve(host:defaultsUser:) -> SSHTarget` that canonicalizes the host (strip a
  trailing `.`) and picks the user (override else `NSUserName()`). Pure; unit-tested.

### Glue in `LaunchController` (AppShell)
Add, following the existing precheck→spawn→error pattern:
- `openTerminalSSH(host:)` — resolve `SSHTarget`; `osascript` Terminal.app `do script` the ssh
  command. (No binary precheck — `ssh` is always present on macOS.)
- `openTTToplike(host:ctrlPort:)` — precheck `tt-toplike-tui` via the existing
  `resolveBrewBinary`-style lookup (`~/.local/bin/tt-toplike-tui`, `/opt/homebrew/bin`,
  `/usr/local/bin`); if missing, surface "tt-toplike not installed — build it from
  ~/code/tt-toplike (inference-server-monitoring)"; else `osascript` Terminal.app the command.
- `openVSCode(host:)` — resolve `SSHTarget`; precheck the `code` CLI (`/usr/local/bin/code`,
  `/opt/homebrew/bin/code`); if missing, surface "VS Code `code` CLI not found — install it from
  VS Code (Shell Command: Install 'code')"; else run `code` with `remoteArgs`. (Remote-SSH is
  installed; a first connect prompts for the box password in VSCode if key auth isn't set up.)
- New in-flight/error state fields per launcher (`isLaunchingTerminal`, `isLaunchingToplike`,
  `isLaunchingVSCode`, and matching `*Error` strings), mirroring the existing WebUI/opencode ones.

### View — a "Workbench" section in `BoxWorkspaceView`
A `Workbench` group (shown for any selected box, independent of serving state) with three buttons —
**Terminal**, **tt-toplike**, **VS Code** — each with its own spinner + inline error, wired to the
new `LaunchController` methods, passing `box.record`'s host/ctrlPort. The existing serving-model
Connect row (Open WebUI / opencode) stays where it is (in the `if let ep = box.endpoint` block).

## Data flow

Selected box → `BoxWorkspaceView` reads `box.record.host` / `box.record.ctrlPort` → Workbench
buttons call `LaunchController.openTerminalSSH/openTTToplike/openVSCode` → builders produce the
command/argv → `osascript`/`code` launches the box-connected tool. No new backend calls; SSH/VSCode
auth is handled interactively by the launched tool.

## Error handling

Each launcher surfaces a precheck/tool-missing message inline (no silent failure), same as the
existing launchers. SSH/host-key/password failures surface in the launched Terminal/VSCode session
(not the app) — expected for interactive tools.

## Testing

- **Unit (`swift test`):** the four pure builders — `TerminalSSHLauncher.command`,
  `TTToplikeLauncher.command`, `VSCodeLauncher.remoteArgs`, and `SSHTarget.resolve` (trailing-dot
  strip + user default/override) — against fixed inputs.
- **Build:** app `xcodebuild` succeeds; the new Workbench buttons compile.
- **Owner click-through:** with a discovered box, click Terminal (a Terminal SSHes in — password
  prompt is fine), tt-toplike (a Terminal shows live telemetry), VS Code (a Remote-SSH window opens
  on the box with a box terminal). Passwordless SSH (add the Mac key to the box) makes Terminal/
  VSCode seamless but isn't required to demo.

## Deferred (not now)

- Passwordless SSH setup automation (owner adds the Mac pubkey to the box; optional agent-advertised
  SSH hint).
- Installing `tenstorrent.tt-vscode-toolkit` on the *remote* (it's installed locally; whether it
  runs local vs. remote depends on its extensionKind — verify during click-through).
- Per-box SSH user override UI (UserDefaults key exists; no settings screen yet).
- Opening a specific project folder over Remote-SSH (defaults to the box home).

# TTStation "Connect" launchers — design

**Date:** 2026-07-03
**Status:** approved (brainstorming), pending implementation plan
**Depends on:** the TTStation menu-bar app (`macos/TTStation/`) and its live `Endpoint`
(`base_url`, `model`, `requires_key`) obtained via `tt endpoint`/`tt run`.

## Goal

Show how fast you go from the menu-bar app to a real chat/coding session on the model the box
just started, the Mac-native way: **one click in the app → a front-end connected to the box's
OpenAI-compatible `/v1`.** Two launchers ship today:
- **Open Web UI** — a browser chat UI, run locally via `uvx open-webui serve`, wired to the box.
- **Open in opencode** — a terminal coding agent, launched in Terminal.app wired to the box.

The app already holds the one artifact both need — the running model's `base_url` — so each
launcher just hands that to a *local* Mac tool and launches it. The box conversation stays in
`tt` (veneer preserved); the launchers only orchestrate local apps.

### Decisions (from brainstorming)
- Open WebUI runs **locally on this Mac via `uvx`** (no Docker; `uv` is present), pointed at the
  box endpoint.
- opencode launches in a **dedicated per-box scratch dir**, not the user's project folders.
- Both buttons appear only when the box is **serving** (there is an `endpoint`).

## Architecture

Pure, testable builders in `TTStationKit`; thin side-effecting glue in the app target
(`AppShell`), which owns Process spawning, `osascript`, and `NSWorkspace`.

### `TTStationKit` — pure builders (unit-tested)

- **`OpenCodeLauncher`**
  - `static func configJSON(for endpoint: Endpoint) -> String` — returns the `opencode.json`:
    ```json
    {
      "$schema": "https://opencode.ai/config.json",
      "provider": {
        "ttstation": {
          "npm": "@ai-sdk/openai-compatible",
          "name": "TT Station",
          "options": { "baseURL": "<endpoint.baseURL>" },
          "models": { "<endpoint.model>": { "name": "<endpoint.model> (TT)" } }
        }
      },
      "model": "ttstation/<endpoint.model>"
    }
    ```
    (`baseURL` is the full `.../v1` the endpoint reports.)
  - `static func terminalCommand(configDir: String) -> String` — the shell line Terminal runs:
    `cd '<configDir>' && opencode` (single-quoted dir; assume no single-quotes in our own
    scratch path).

- **`OpenWebUILauncher`**
  - `static func invocation(for endpoint: Endpoint) -> (executable: String, args: [String], env: [String: String])`
    → `("uvx", ["open-webui", "serve", "--port", "8080"], ["OPENAI_API_BASE_URL": endpoint.baseURL, "OPENAI_API_KEY": "sk-none", "WEBUI_AUTH": "false"])`.
    `WEBUI_AUTH=false` skips the account-creation wall for a quick demo.
  - `static let url = URL(string: "http://localhost:8080")!`
  - `static let healthURL = URL(string: "http://localhost:8080/health")!`

Endpoints reach these builders from the app; the builders never touch Process/AppKit.

### `AppShell` — glue (verified by launching, not unit tests)

- **`LaunchController`** (`@MainActor`), one method per launcher, each reporting progress/errors
  back to the view:
  - `openCode(endpoint:)`:
    1. Resolve a scratch dir `~/Library/Application Support/TTStation/opencode/<hostPort-sanitized>/`
       (create if needed).
    2. Write `OpenCodeLauncher.configJSON(for:)` to `<dir>/opencode.json`.
    3. `osascript -e 'tell application "Terminal" to do script "<terminalCommand>"'` then activate
       Terminal. Running in Terminal's login shell resolves `opencode` on PATH (sidesteps the
       GUI-PATH problem the app itself has).
    4. Precheck: if `opencode` is not found (probe `/opt/homebrew/bin/opencode` and a login-shell
       `command -v opencode`), surface "opencode not installed — `brew install sst/tap/opencode`"
       instead of opening a terminal that prints "command not found".
  - `openWebUI(endpoint:)`:
    1. If `:8080/health` already returns 200 → just `NSWorkspace.shared.open(url)` and return.
    2. Else spawn `uvx open-webui serve …` **detached** (survives app quit) with the env from
       `invocation(for:)`, tracked so we don't double-spawn.
    3. Poll `healthURL` (e.g. up to ~90s, since first run may still be resolving deps) with a
       spinner; on ready → open the browser; on timeout → error ("Open WebUI didn't come up —
       check the logs").
    4. Precheck: if `uvx` is missing, surface "uv not installed — `brew install uv`".

- **View wiring:** `BoxDetailView` gains a **Connect** row (only in the serving branch, guarded by
  `box.endpoint != nil`) with two buttons bound to `LaunchController`, each with its own
  `inFlight` spinner and error text. State lives in a small `@Observable` holder (or on the
  existing box view-model) so the buttons disable while launching.

## Data flow

Model serving → `box.endpoint` set → **Connect** row visible.
- **Open in opencode:** click → write per-box `opencode.json` (baseURL+model from the endpoint) →
  Terminal opens `cd <dir> && opencode` → coding against the box.
- **Open Web UI:** click → (spawn `uvx open-webui serve` if not already up) → poll health →
  browser opens `localhost:8080` → chatting with the box's model.

## Error handling / robustness

- Missing tool (`uvx`/`opencode`) → explicit, actionable message; no silent failure and no
  terminal-of-shame.
- Health-poll timeout for Open WebUI → surfaced error; the detached server keeps running so a
  retry can just reattach.
- Endpoint absent (box idle) → the Connect row isn't shown at all.
- Pre-warm note: `uvx open-webui`'s first fetch is slow; pre-warm once before demoing so the
  click is snappy. This is an operational note, not app logic.

## Testing

- **Unit (`swift test`):** `OpenCodeLauncher.configJSON` produces valid JSON containing the
  endpoint's `baseURL` and model and the `ttstation/<model>` selection; `terminalCommand`
  composes the expected `cd … && opencode`; `OpenWebUILauncher.invocation` yields the exact
  argv + env (incl. `WEBUI_AUTH=false`) and the localhost URL. Use the existing endpoint
  fixtures.
- **Manual/integration:** with the box serving, click each button and confirm — opencode opens
  a Terminal already talking to the box; Open WebUI opens a browser chat that completes against
  the box's `/v1`. (Owner-run, like the other GUI checks.)

## Deferred (not today)

- Docker/podman path for Open WebUI (uvx is enough on this Mac).
- Pre-adding a connection to an already-running remote Open WebUI.
- Choosing the opencode working folder / opening in a real project.
- Shortcuts / App Intents exposure (Approach 2) and deep links (Approach 3).
- Persisting/among multiple Open WebUI instances; lifecycle management beyond "spawn if not up".

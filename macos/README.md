# tt-station — macOS app (`TTStation`)

A native SwiftUI veneer over the `tt` CLI. A fast **`MenuBarExtra`** popover for glance +
quick actions (status, Run/Stop, a live temp chip, "Open window"), backed by a resizable
**control-room window**: a boxes sidebar plus a card-based detail pane —

- **Header** — box name, chips, and a detected **device-mesh badge** (`P300X2`).
- **Live device strip** — per-device temperature / power / aiclk, streamed from the agent's
  `/telemetry` WebSocket (the single read-only I/O path in Swift; all *control* still goes
  through `tt --json`). Temp is color-ramped; `Open tt-toplike ↗` for the deep view.
  The dashboard requests `GET /telemetry?view=lite` — a trimmed per-device
  temp/power/aiclk frame; the box skips its process scan and vLLM scrape for lite
  clients. **tt-toplike** instead opens the full `GET /telemetry` (no query), which
  carries the `tt_toplike` process/inference enrichment on top. The lite frame is a
  strict subset of the full tt-smi-ish shape, so an older agent that doesn't know
  `?view=lite` yet just sends the full frame — the app decodes it the same way either
  way, so nothing breaks pre-redeploy; the box load reduction only kicks in once the
  agent understands the query. The app opens **one shared telemetry socket per box**
  (ref-counted on `BoxViewModel`) so the window's device strip and the popover chip
  both ride the same connection instead of doubling it. `mock-box` honors
  `?view=lite` too (a trimmed canned frame, no `tt_toplike`), so this path is
  exercisable with no hardware.
- **Persistent Run/Stop action bar** — pinned below the scroll (`RunStopBar`), always
  visible regardless of scroll position. It's the single owner of the "what's this box
  serving right now, and at what endpoint" display — model picker, **Run / Stop**, and the
  live endpoint once serving. (Used to live inline with the model browser; the window
  redesign pulled it out so it never scrolls out of view.)
- **Hardware-aware, tt-inference-server-focused model browser (3 tiers)** — merges the
  box's live `/models` with the public Tenstorrent compatibility catalog
  (`compatibility.json`, fetched + 24h-cached by `tt`), classified for this box's mesh. The
  primary **Models** tier is what the box can actually serve today via `run.py` (vLLM,
  runnable now); tt-forge/tt-metal-supported-but-not-run.py-servable models surface under
  **Experimental** (bring these up with the tools — links to the Workbench) alongside other
  supported-with-more-setup entries; **Needs other hardware** is labeled with the mesh each
  needs. Only runnable models are Run-enabled; the smart default is compatible-first.
  Degrades to just the box's live models when the catalog is offline. Meshes include
  **P150 x1–x4** and P300/N300/T3K/GALAXY.
- **Config card** — a read-only summary of the box's resolved serving config (active
  profile + other available ones, backend, `serving_host:port`, image, device), from the
  unauthed `tt --json config` read. **Switching profiles happens on the box panel, not
  here** — the Mac only ever displays the resolved result.
- **Fast Connect** — Open WebUI / opencode, installing missing deps (`brew install …`) as needed.
- **Workbench** — Terminal (SSH), tt-toplike (remote telemetry), and VS Code Remote-SSH with
  the `Tenstorrent.tt-vscode-toolkit` extension installed. All SSH as **`ttuser`** (the
  QuietBox 2 default login) unless overridden via the `tt.sshUser` UserDefault.
- **Keyless SSH on pair** — the pair flow has an opt-in "Also enable Terminal / SSH access"
  toggle (default on). On a successful pair it installs this Mac's SSH **public** key on the
  box (as `ttuser`) via `tt ssh-authorize`, so the workbench launchers work with no password.
  The PIN handshake is the trust anchor; only the public key leaves the Mac; the installed
  key is tagged (`ttstation:<host>:<date>`) and revocable (`tt ssh-authorize --host <h>
  --revoke`). The SSH step is non-fatal — a pair that can't set up SSH still pairs.
- **Log viewing (box side, not yet in the app)** — the box now exposes `GET /logs`
  (`?source=container|run&tail=N`) and a `GET /logs/stream` WebSocket, both unauthed, for
  tailing/following the serving container's log (where model-load failures actually
  surface) or run.py's own launch log — see `docs/reference/logs.md`. Today this is only
  reachable via `tt logs [--follow]` or the `tt console` TUI's log pane; a "View logs"
  button in this app (streaming `/logs/stream` the same way the telemetry strip streams
  `/telemetry`) is a brief for a future macOS session, not yet built.

**Status:** v0.6.2 built (`macos/TTStation/`). Logic lives in the `TTStationKit` Swift package
(130 passing tests via `swift test`; ranking, mesh-match, telemetry decode, install-command
builders, and the `ttuser` SSH default are pure and unit-tested); the SwiftUI app target is
generated with XcodeGen and builds clean. End-to-end verifiable against `mock-box` (no
hardware — it reports `device_mesh`, streams a canned telemetry frame, and serves
`/ssh/authorize` against a temp file). The box's device mesh is sourced from Rust: the agent
detects it at startup and reports it in `/status` and the mDNS TXT record. Keyless SSH is
sourced from Rust too: `tt ssh-authorize` reads/generates the Mac keypair and posts the public
key to the agent's authed `POST /ssh/authorize`, which appends it to `ttuser`'s
`~/.ssh/authorized_keys` (idempotent, tagged; `DELETE` to revoke).

## Install (for users)

TTStation ships as a prebuilt **Apple Silicon** DMG on the repo's
[Releases](https://github.com/tsingletaryTT/tt-station/releases) page.

1. Download `TTStation-<version>-arm64.dmg` and open it.
2. Drag **TTStation.app** onto **Applications**.
3. The app is ad-hoc signed (no Apple Developer certificate yet), so macOS
   quarantines it and may say *"TTStation is damaged."* Clear the quarantine
   once:

   ```sh
   xattr -dr com.apple.quarantine /Applications/TTStation.app
   ```

4. Launch TTStation from Applications — it lives in the **menu bar**, not the
   Dock. On first run it offers to add the `tt` CLI to `~/.local/bin`
   (skippable; the app bundles its own copy and works either way). If you
   already have a different `tt` on your PATH, TTStation leaves it alone and
   offers to install as `tt-station` instead.

> Building from source instead? See `macos/install.sh` (needs Xcode + Rust).
> Notarizing to remove the quarantine step is a future upgrade once an Apple
> Developer certificate is available.

## Install (recommended)

    macos/install.sh            # build Release → ~/Applications/TTStation.app, then launch
    macos/install.sh --system   # → /Applications/TTStation.app (sudo)

It's a menu-bar app (`LSUIElement`) — after install look for the icon in the menu bar,
not the Dock. The version comes from `AppShell/project.yml`'s `MARKETING_VERSION` (flowed
into `Info.plist` via `$(MARKETING_VERSION)` — bump it there for each release).

## Build & run (dev)

    cd macos/TTStation && swift test                     # unit tests (pure logic)
    cd macos/TTStation/AppShell && xcodegen generate \
      && xcodebuild -project TTStation.xcodeproj -scheme TTStation \
           -destination 'platform=macOS' build

The built app lands under Xcode's DerivedData; find the exact path with:

    xcodebuild -project macos/TTStation/AppShell/TTStation.xcodeproj -scheme TTStation \
      -showBuildSettings | awk '/ BUILT_PRODUCTS_DIR /{d=$3} /FULL_PRODUCT_NAME/{p=$3} END{print d"/"p}'

## End-to-end with no hardware (`mock-box`)

`mock-box` (`crates/mock-box`) fakes an agent's control API + a canned `/v1` over plain
HTTP — no real box or Docker needed. Two verification paths, both proven for this task:

1. **CLI path** (what `crates/tt/tests/e2e_mock.rs` automates): build `tt` and `mock-box`
   (`cargo build --release -p tt -p mock-box`), then drive the same `tt --json` calls the
   app's `TTClient` issues, against `./target/release/mock-box serve --ctrl-port 18899
   --name quietbox-mock --chips 4xBH`:

       tt --json discover --host 127.0.0.1:18899 --no-mdns
       tt --json models   --host 127.0.0.1:18899
       tt --json pair     127.0.0.1:18899 --code 000000   # mock-box accepts any code
       tt --json run      mock-model --host 127.0.0.1:18899
       tt --json endpoint --host 127.0.0.1:18899
       tt --json status   --host 127.0.0.1:18899
       tt --json stop     --host 127.0.0.1:18899

   This proves the app's exact backend contract (argv shape + JSON decode types) without
   driving the GUI. The CLI stores the pairing token in the macOS Keychain, same as the app.

2. **GUI path** — see the manual checklist below; a menu-bar `MenuBarExtra` can't be driven
   programmatically, so a human has to click through it at least once per release.

### Manual smoke checklist (owner, at the Mac)

With `mock-box serve --ctrl-port 18899 --name quietbox-mock --chips 4xBH` running:

- [ ] Open `TTStation.app` — the menu-bar icon appears, no crash.
- [ ] The mock box appears in the list (or add it manually as `127.0.0.1:18899` if mDNS
      discovery is off/blocked on this network).
- [ ] Click **Pair**, read the 6-digit code mock-box prints to its console, enter it.
- [ ] Pick a model from the picker (`mock-model` / `mock-model-large`).
- [ ] Click **Run** — an endpoint (`http://127.0.0.1:18899/v1`) appears; status dot goes
      green.
- [ ] **Copy endpoint** — paste somewhere and confirm it matches.
- [ ] **Stop** — status returns to idle.
- [ ] Any CLI stderr shows up honestly in the UI (no silent failure) if you kill mock-box
      mid-flow and retry.

## The one rule: it's a veneer, not a brain

All logic lives in Rust. The app **shells out to `tt --json`** and renders the result —
no discovery, pairing, HTTP, or token handling reimplemented in Swift. If you find yourself
parsing mDNS or building requests in Swift, stop: add it to the CLI instead.

## The CLI contract (`tt --json`)

The Rust CLI is the source of truth — read `crates/tt/src/main.rs` for exact JSON shapes.
The commands the app drives (all accept a global `--json`):

| Intent | Command |
|---|---|
| List boxes on the network | `tt --json discover` (add `--host h:p` for manual, `--no-mdns` to skip Bonjour) |
| Pair with a box | `tt --json pair <host:port> --code <6-digit>` |
| Start a model | `tt --json run <model> --host <host:port>` → returns the `Endpoint` |
| Stop the current model | `tt --json stop --host <host:port>` |
| Current status | `tt --json status --host <host:port>` → `idle` / `serving:<model>` |
| Get the live endpoint | `tt --json endpoint --host <host:port>` → `{ base_url, model, requires_key }` |
| List all serving models | `tt --json serving --host <host:port>` (unauthed) → `{ serving: [ { model, base_url, host_port, container, source } ] }`; `source` is `agent` or `external` (e.g. tt-studio) |
| Resolved serving config | `tt --json config --host <host:port>` (unauthed) → `ConfigSummary`: `{ active_profile, available_profiles, backend, serving_host, serving_port, serving_image, tt_inference_repo, tt_device }`. Mirrors the agent's `GET /config`; see `docs/reference/agentd-config.md`. No secrets (`hf_token` is never in this struct). |
| Merged model catalog | `tt --json catalog --host <host:port>` (unauthed; `--refresh`, `--catalog-file <p>`) → `BoxCatalog`: `{ box_mesh, catalog_available, catalog_stale, runs_here, experimental, other_hardware }`. Merges the box's live `/models` with the public compatibility catalog (`https://d1oi7xemha0dsy.cloudfront.net/data/compatibility.json`, 24h-cached at `~/.cache/tt-station/`), classified for the box mesh; each entry `{ id, display_name, family, size, software, meshes, needed_hardware, available_now, status_here }`. Offline-tolerant. |

`base_url` is what you hand to any OpenAI client. Non-JSON `tt endpoint` prints
`export OPENAI_BASE_URL=…` for shells; the app uses the `--json` form and offers "Copy endpoint."

Tokens are stored in the macOS **Keychain** by the CLI (service `tt-station`), so the app
does not handle secrets directly.

### Opt out of the Keychain (stop the "tt wants to use tt-station" prompt)

The Keychain prompt recurs when `tt` is rebuilt: an **ad-hoc-signed** binary's code identity
(`CDHash`) changes every build, and macOS binds a "Always Allow" grant to that identity — so
each rebuild invalidates it. To skip the Keychain entirely, opt into the file-backed token
store (a `0600` JSON at `~/.config/tt-station/secrets.json`, the same store `tt` uses on
Linux/CI):

    printf 'file' > ~/.config/tt-station/secret_store   # or set TT_SECRET_STORE=file

Use a **marker file** (not just the env var) so the Finder-launched app's `tt` subprocess
agrees with terminal `tt` — Finder doesn't inherit shell env, so an env-only opt-in would
split the two into different stores. The default is unchanged (Keychain on macOS) unless you
opt in. Switching stores means **re-pairing each box once** (the old Keychain token isn't
read from the file store). Revert with `printf 'keychain' > ~/.config/tt-station/secret_store`
or deleting the marker. (The real fix for keeping the Keychain across rebuilds is to sign
`tt` with a stable self-signed identity instead of ad-hoc — not done yet.)

## What the menu should show (Task 14 scope)

- Discovered boxes, each with a **status dot** (green = serving, grey = idle) and its chips.
- A **model picker** + **Run / Stop** for the selected box (spinner until `/v1` is healthy).
- **Copy endpoint** (the `base_url`) and **Open Cloud Console** (`https://console.tenstorrent.com`).
- A **notification** (`UNUserNotification`) when a model reaches ready.
- Honest empty/error states — surface the CLI's stderr, don't swallow it.

## Prerequisites

- **Xcode** (not just Command Line Tools) — a SwiftUI app target needs the full IDE/SDK.
  Check: `xcode-select -p` and `swift --version`.
- The `tt` binary on `PATH` (or ship it inside the app bundle and resolve it at runtime).
  Build it with `cargo build --release -p tt`.

## Look & feel

Match the product family (see `site/index.html`): teal `#4FD1C5` accent on a deep
blue-black ground, JetBrains/monospace for endpoints and status. Keep it quiet and native —
the menu bar is not the place for the microsite's hero flourishes.

## Suggested layout

```
macos/
  README.md            ← you are here
  TTStation/           ← Xcode project (app target, SwiftUI MenuBarExtra)
    TTStationApp.swift
    …
```

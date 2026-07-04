# tt-station — macOS menu-bar app (`TTStation`)

A native SwiftUI **`MenuBarExtra`** veneer over the `tt` CLI. This is the "as your Mac
sees it" surface from the microsite: pick a box, run/stop a model, copy the endpoint — all
from the menu bar.

**Status:** v1 built (`macos/TTStation/`). Logic lives in the `TTStationKit` Swift package
(32 passing tests via `swift test`); the SwiftUI app target is generated with XcodeGen and
builds clean. End-to-end verified against `mock-box` (no hardware) — see below.

## Build & run

    cd macos/TTStation && swift test                     # unit tests (32 tests, layers 1–10)
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

`base_url` is what you hand to any OpenAI client. Non-JSON `tt endpoint` prints
`export OPENAI_BASE_URL=…` for shells; the app uses the `--json` form and offers "Copy endpoint."

Tokens are stored in the macOS **Keychain** by the CLI (service `tt-station`), so the app
does not handle secrets directly.

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

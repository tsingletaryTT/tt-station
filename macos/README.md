# tt-station — macOS menu-bar app (`TTStation`)

A native SwiftUI **`MenuBarExtra`** veneer over the `tt` CLI. This is the "as your Mac
sees it" surface from the microsite: pick a box, run/stop a model, copy the endpoint — all
from the menu bar.

**Status:** not built yet. This is the landing spot / contract for Task 14
(see `docs/superpowers/plans/2026-07-02-tt-station-poc.md`). Build it under `macos/TTStation/`.

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

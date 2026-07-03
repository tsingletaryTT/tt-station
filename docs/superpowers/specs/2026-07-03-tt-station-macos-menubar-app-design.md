# tt-station macOS menu-bar app (`TTStation`) — design

**Date:** 2026-07-03
**Status:** approved (brainstorming), pending implementation plan
**Task:** Task 14 from `docs/superpowers/plans/2026-07-02-tt-station-poc.md`
**Landing contract:** `macos/README.md`

## Goal

A native SwiftUI **`MenuBarExtra`** app that is a *veneer* over the `tt` CLI: the
"as your Mac sees it" surface from the microsite. Pick a box, run/stop a model, copy the
OpenAI-compatible endpoint — all from the menu bar. **All logic lives in Rust**; the app
shells out to `tt --json` and renders the result. No discovery, pairing, HTTP, or token
handling is reimplemented in Swift.

### v1 scope — "discovery hero + full loop"

The demo target (end of weekend): show the plug-and-play story end to end.

- Box **auto-appears** via mDNS (`tt discover`), with a **manual host:port fallback** for
  LANs that block mDNS (corp LANs often do — see project CLAUDE.md).
- Per-box **status dot** (green = serving, grey = idle) + chips (device inventory).
- **Pair** flow: enter the 6-digit code the box prints on its console.
- **Model picker** (from `tt models`, which is unauthed), **Run** / **Stop** with a
  spinner until the run returns an endpoint, and **Copy endpoint** (`base_url`).

### Explicitly deferred (v2)

- `UNUserNotification` on model-ready.
- "Open Cloud Console" link.
- Live background status polling (v1 refresh is **on-demand** only).
- Bundling the `tt` binary inside the app (v1 resolves it from disk).
- Tenstorrent tray icon art — when we add it, pull from `~/code/tt-vscode-toolkit` or
  `~/code/tt-local-generator` rather than recreating it.

## App shape & target

- SwiftUI `MenuBarExtra` with **`.menuBarExtraStyle(.window)`** — a real popover panel, not
  a plain `NSMenu`. Required because we host a model picker, a spinner, and a pairing-code
  text field, none of which fit the `.menu` style.
- **Agent app** (`LSUIElement = YES`, no Dock icon).
- Deployment target **macOS 14** (Sonoma). `MenuBarExtra` + `.window` style are 13+.
- **Swift 5 language mode** — deliberately *not* Swift 6 strict concurrency, to avoid
  spending the demo window fighting the concurrency checker. Tighten to Swift 6 later.
- Built with Xcode 26.6 (verified present on this machine). Project at `macos/TTStation/`.

## Architecture — Approach 3 (MVVM, five layers)

Each layer has one purpose and is testable in isolation.

### 1. `TTProcessRunner` (protocol)

The **only** thing that spawns a `Process`. 

```
protocol TTProcessRunner {
    func run(_ args: [String]) async throws -> (stdout: Data, stderr: String, exitCode: Int32)
}
```

- `RealProcessRunner` locates the binary via `TTBinaryLocator`, spawns it, captures streams.
- `FakeProcessRunner` returns canned `(stdout, stderr, exitCode)` for tests.

**`TTBinaryLocator`** resolves `tt` in order, because **GUI apps do not inherit the shell
`PATH`** (launchd gives them a minimal one, so `~/.local/bin/tt` is not found automatically):

1. User override path (UserDefaults key `tt.binaryPath`) — the single knob for any
   non-standard location, including a dev `target/release/tt`.
2. `~/.local/bin/tt` (where this repo installs it).
3. `/opt/homebrew/bin/tt`, `/usr/local/bin/tt`.
4. None found → a typed error listing every path tried (surfaced as a banner), with a hint
   to `cargo build --release -p tt` or set the override path.

### 2. `TTClient`

One typed `async` method per CLI command. Each builds argv (global `--json` first, e.g.
`["--json", "discover"]`), calls the runner, decodes stdout into a domain model, and maps a
non-zero exit code to `TTError(command, exitCode, stderr)`.

| Method | Command | Returns |
|---|---|---|
| `discover(manualHosts:noMdns:)` | `tt --json discover [--host h:p]… [--no-mdns]` | `[BoxRecord]` |
| `models(host:)` | `tt --json models --host h:p` | `[ModelInfo]` (unauthed) |
| `pair(host:code:)` | `tt --json pair h:p --code NNNNNN` | `PairResult` |
| `run(host:model:)` | `tt --json run <model> --host h:p` | `Endpoint` |
| `stop(host:)` | `tt --json stop --host h:p` | `Void` |
| `status(host:)` | `tt --json status --host h:p` | `ServingStatus` (unauthed) |
| `endpoint(host:)` | `tt --json endpoint --host h:p` | `Endpoint` |

The `--code` flag lets a SwiftUI text field feed the pairing code directly — no stdin
wrestling with the CLI's interactive prompt.

### 3. Domain models (`Codable` mirrors of the CLI JSON contract)

Ground truth is `crates/tt/src/main.rs` (print functions) and
`crates/libttstation/src/model.rs`. Verified shapes:

- **`BoxRecord`** — `{ name, host, ctrl_port, chips, status, apiver }`. `status` is the
  string wire form (`"idle"` / `"serving:<model>"`), *not* a serde-tagged enum. `ctrl_port`
  needs a CodingKey.
- **`Endpoint`** — `{ base_url, model, requires_key }` (CodingKeys for snake_case).
- **`ModelInfo`** — `{ name, devices: [String] }`; **`ModelsResponse`** — `{ models: [ModelInfo] }`.
- **`ServingStatus`** — parsed from the status string: `.idle` or `.serving(model)`.
- **`PairResult`** — `{ host, paired, token }` (the app ignores `token`; the CLI already
  stored it in Keychain).

> Note: the CLI's `--json` status form is `{"status": "idle"}` / `{"status": "serving:x"}`,
> and `discover` re-encodes status through `to_txt()` so every command speaks the same wire
> form. The Swift models parse that string; they never assume serde's default enum tagging.

### 4. State layer (MVVM)

- **`AppModel`** (`@Observable @MainActor`) — owns `boxes: [BoxViewModel]`, `selectedBoxID`,
  and a global `scanState` (idle / scanning / error). Drives on-demand refresh.
- **`DiscoveryService`** (protocol) — `func scan() async -> [BoxRecord]`. The
  `MDNSDiscoveryService` impl calls `TTClient.discover()` (which does the mDNS browse) and
  merges the results with the **manual-host registry** (persisted host:port list in
  UserDefaults), **deduped by `host:port`**. `discover` blocks for the full mDNS timeout, so
  the scan runs async behind the spinner — it never blocks the menu.
- **`BoxViewModel`** (`@Observable`) — wraps one `BoxRecord` plus live `status`, `endpoint`,
  `inFlight`, `error`, and `pairedState`. Exposes `refresh()`, `pair(code:)`, `run(model:)`,
  `stop()`, `loadModels()`.

**Paired-state tracking** (the app never touches Keychain): after a successful `tt pair`,
record the host in a UserDefaults `paired-hosts` set. On launch, hosts in that set are
assumed paired. If an authed call (`run`/`stop`/`endpoint`) returns an auth error, flip that
host back to unpaired and re-prompt for the code.

### 5. Views

- **`MenuContentView`** (root panel) — scan spinner / "Scan" affordance, list of
  `BoxRowView`, and an "Add host…" control opening `ManualHostSheet`.
- **`BoxRowView`** — status dot (green serving / grey idle), name, chips; selectable.
- **Selected-box detail** — if unpaired: a 6-digit code field + Pair button. If paired: a
  model `Picker`, **Run** / **Stop** buttons (Run shows a spinner until `Endpoint` returns),
  and **Copy endpoint** (copies `base_url` to the pasteboard).
- **`ManualHostSheet`** — host:port entry that appends to the manual-host registry.

Look & feel: teal `#4FD1C5` accent on a deep blue-black ground, monospace for endpoints and
status (matches `site/index.html` and `tt-vscode-toolkit`). Quiet and native — no microsite
hero flourishes in the menu bar.

## Data flow (the demo path)

1. Menu opens → `AppModel` starts a scan (async, spinner shown).
2. `DiscoveryService.scan()` → mDNS hits merged with manual hosts, deduped by `host:port`.
3. Each `BoxViewModel.refresh()` fires the **unauthed** `status` + `models` → rows render
   dots and are ready to act on.
4. User selects a box. Unpaired → 6-digit code field → `tt pair --code` → on success, mark
   paired and load models.
5. Pick a model → **Run** → `tt run` with a spinner until the `Endpoint` returns → show
   `base_url` + **Copy**.
6. **Stop** → `tt stop`. All refresh is on-demand (menu-open + after each action).

## Error handling

- Every `TTClient` call throws `TTError { command, exitCode, stderr }`. `BoxViewModel`
  catches and sets an inline `error` string that shows the CLI's **stderr verbatim** — per
  the README, surface it, don't swallow it.
- Binary-not-found → a prominent banner listing every path `TTBinaryLocator` tried, with a
  hint to `cargo build --release -p tt` and/or set the override path.
- Empty scan → "No boxes found — Add manually."
- Auth error on an authed call → flip host to unpaired + re-prompt (see paired-state above).

## Testing (TDD)

- **`TTClient`** against `FakeProcessRunner` using **real captured `tt --json` fixtures**
  (recorded from actual CLI output / mock-box) — argv construction, decoding, and
  non-zero-exit → `TTError` mapping.
- **Domain decoding** — `ServingStatus` idle/serving parsing, snake_case CodingKeys,
  `ModelsResponse`.
- **`DiscoveryService`** — mDNS + manual-host merge and `host:port` dedupe.
- **`BoxViewModel`** — state transitions (refresh, pair success/failure, run→endpoint, stop,
  auth-error → unpaired) via a fake `TTClient`.
- **End-to-end against `mock-box`** — `mock-box` advertises mDNS, fakes the control API, and
  serves canned `/v1`, so the *entire* app can be exercised locally with **no hardware**
  before pointing it at the real QB2. This is the primary pre-hardware integration target.

## Interfaces summary (what depends on what)

```
Views ──▶ AppModel / BoxViewModel ──▶ DiscoveryService + TTClient ──▶ TTProcessRunner ──▶ `tt` binary
                                          │                              │
                                          └─ domain models (Codable) ◀───┘
```

- Views depend only on the state layer. No `Process`, no JSON in views.
- The state layer depends on `TTClient` + `DiscoveryService` protocols (fakeable).
- `TTClient` depends on `TTProcessRunner` (fakeable) + domain models.
- Only `RealProcessRunner` touches `Process` and the filesystem (binary location).

## Open risks

- **mDNS on the demo LAN** — if the corp LAN blocks mDNS, `discover` returns nothing; the
  manual-host fallback is the safety net and must work independently of discovery.
- **`discover` full-timeout wait** — a known CLI behavior; mitigated by running scans async
  behind a spinner. (Listed as a deferred CLI ticket in project CLAUDE.md.)
- **Binary resolution in a GUI context** — the single most likely "works in terminal, not in
  the app" failure; `TTBinaryLocator` centralizes it with an explicit, surfaced error.

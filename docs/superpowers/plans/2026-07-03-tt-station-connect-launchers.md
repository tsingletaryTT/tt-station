# TTStation "Connect" Launchers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add one-click "Open Web UI" and "Open in opencode" launchers to the TTStation menu-bar app that connect a local front-end to the model the box is serving, using its OpenAI-compatible endpoint.

**Architecture:** Pure, unit-tested builders in the `TTStationKit` package (opencode config text; the `uvx open-webui` argv/env/URL); thin side-effecting glue in the `AppShell` app target (`LaunchController`: writes the config, spawns `uvx`, polls health, opens Terminal/the browser). The launchers only orchestrate *local* Mac tools with the endpoint the app already holds — the box conversation stays in `tt`.

**Tech Stack:** Swift 5, SwiftUI/Observation, Foundation `Process`/`URLSession`, `osascript` (Terminal.app), `NSWorkspace`, `uvx` (open-webui), `opencode`.

## Global Constraints

- macOS 14; Swift 5 language mode; code under `macos/TTStation/`.
- The app is a veneer: launchers use the `Endpoint` the app already obtained via `tt`; they must NOT re-implement box discovery/HTTP/pairing.
- Open WebUI runs locally via `uvx open-webui serve --port 8080`, env `OPENAI_API_BASE_URL=<base>`, `OPENAI_API_KEY=sk-none`, `WEBUI_AUTH=false`; opened at `http://localhost:8080`.
- opencode launches in a dedicated scratch dir `~/Library/Application Support/TTStation/opencode/<sanitized-hostPort>/` via Terminal.app running `cd '<dir>' && opencode`.
- Connect actions appear ONLY when `box.endpoint != nil` (serving).
- GUI apps don't inherit shell PATH: resolve `uvx`/`opencode` by absolute path (`/opt/homebrew/bin`, `/usr/local/bin`); running `opencode` *inside Terminal* is fine (login shell resolves it).
- `Endpoint` shape (from `TTStationKit`): `Endpoint { baseURL: String; model: String; requiresKey: Bool }`.

---

## File Structure

```
macos/TTStation/
  Sources/TTStationKit/Launchers.swift          # NEW: OpenCodeLauncher, OpenWebUILauncher (pure)
  Tests/TTStationKitTests/LaunchersTests.swift   # NEW: unit tests
  AppShell/Sources/LaunchController.swift         # NEW: @Observable glue (Process/osascript/NSWorkspace)
  AppShell/Sources/BoxDetailView.swift            # MODIFY: add "Connect" row in the endpoint branch
```

---

## Task 1: OpenCodeLauncher (pure builder)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/Launchers.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/LaunchersTests.swift`

**Interfaces:**
- Consumes: `Endpoint` (existing).
- Produces: `enum OpenCodeLauncher { static func configJSON(for: Endpoint) -> String; static func terminalCommand(configDir: String) -> String }`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/LaunchersTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class LaunchersTests: XCTestCase {
    private let ep = Endpoint(baseURL: "http://qb2-lab.local:8003/v1", model: "meta-llama/Llama-3.3-70B-Instruct", requiresKey: false)

    func testOpenCodeConfigJSON() throws {
        let json = OpenCodeLauncher.configJSON(for: ep)
        let obj = try JSONSerialization.jsonObject(with: Data(json.utf8)) as! [String: Any]
        XCTAssertEqual(obj["model"] as? String, "ttstation/meta-llama/Llama-3.3-70B-Instruct")
        let provider = obj["provider"] as! [String: Any]
        let tt = provider["ttstation"] as! [String: Any]
        XCTAssertEqual(tt["npm"] as? String, "@ai-sdk/openai-compatible")
        let options = tt["options"] as! [String: Any]
        XCTAssertEqual(options["baseURL"] as? String, "http://qb2-lab.local:8003/v1")
        let models = tt["models"] as! [String: Any]
        XCTAssertNotNil(models["meta-llama/Llama-3.3-70B-Instruct"])
    }

    func testOpenCodeTerminalCommand() {
        XCTAssertEqual(OpenCodeLauncher.terminalCommand(configDir: "/tmp/x"), "cd '/tmp/x' && opencode")
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter LaunchersTests`
Expected: FAIL — `OpenCodeLauncher` undefined.

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Sources/TTStationKit/Launchers.swift`:
```swift
import Foundation

/// Builds the files/commands to launch `opencode` pointed at a box endpoint.
/// Pure — the app layer performs the actual file write + Terminal launch.
public enum OpenCodeLauncher {
    /// Contents of a project `opencode.json` registering a custom
    /// OpenAI-compatible provider `ttstation` for `endpoint` and preselecting
    /// its served model.
    public static func configJSON(for endpoint: Endpoint) -> String {
        let dict: [String: Any] = [
            "$schema": "https://opencode.ai/config.json",
            "provider": [
                "ttstation": [
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "TT Station",
                    "options": ["baseURL": endpoint.baseURL],
                    "models": [endpoint.model: ["name": "\(endpoint.model) (TT)"]],
                ],
            ],
            "model": "ttstation/\(endpoint.model)",
        ]
        let data = try! JSONSerialization.data(
            withJSONObject: dict, options: [.prettyPrinted, .sortedKeys])
        return String(data: data, encoding: .utf8)!
    }

    /// The shell line Terminal runs: cd into the config dir and start opencode
    /// (Terminal's login shell resolves `opencode` on PATH).
    public static func terminalCommand(configDir: String) -> String {
        "cd '\(configDir)' && opencode"
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter LaunchersTests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/Launchers.swift macos/TTStation/Tests/TTStationKitTests/LaunchersTests.swift
git commit -m "feat(macos): OpenCodeLauncher config builder"
```

---

## Task 2: OpenWebUILauncher (pure builder)

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/Launchers.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/LaunchersTests.swift` (add cases)

**Interfaces:**
- Consumes: `Endpoint`.
- Produces: `enum OpenWebUILauncher { static func invocation(for: Endpoint) -> (executable: String, args: [String], env: [String: String]); static let url: URL; static let healthURL: URL }`.

- [ ] **Step 1: Write the failing test**

Append to `LaunchersTests.swift`:
```swift
extension LaunchersTests {
    func testOpenWebUIInvocation() {
        let inv = OpenWebUILauncher.invocation(for: ep)
        XCTAssertEqual(inv.executable, "uvx")
        XCTAssertEqual(inv.args, ["open-webui", "serve", "--port", "8080"])
        XCTAssertEqual(inv.env["OPENAI_API_BASE_URL"], "http://qb2-lab.local:8003/v1")
        XCTAssertEqual(inv.env["OPENAI_API_KEY"], "sk-none")
        XCTAssertEqual(inv.env["WEBUI_AUTH"], "false")
    }

    func testOpenWebUIURLs() {
        XCTAssertEqual(OpenWebUILauncher.url.absoluteString, "http://localhost:8080")
        XCTAssertEqual(OpenWebUILauncher.healthURL.absoluteString, "http://localhost:8080/health")
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter LaunchersTests`
Expected: FAIL — `OpenWebUILauncher` undefined.

- [ ] **Step 3: Write minimal implementation**

Append to `macos/TTStation/Sources/TTStationKit/Launchers.swift`:
```swift
/// Builds the `uvx open-webui serve` invocation (argv + env) and the URLs to
/// poll/open. Pure — the app layer spawns the process and opens the browser.
public enum OpenWebUILauncher {
    public static func invocation(for endpoint: Endpoint)
        -> (executable: String, args: [String], env: [String: String])
    {
        (
            executable: "uvx",
            args: ["open-webui", "serve", "--port", "8080"],
            env: [
                "OPENAI_API_BASE_URL": endpoint.baseURL,
                "OPENAI_API_KEY": "sk-none",
                "WEBUI_AUTH": "false",
            ]
        )
    }

    public static let url = URL(string: "http://localhost:8080")!
    public static let healthURL = URL(string: "http://localhost:8080/health")!
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter LaunchersTests`
Expected: PASS (4 tests in this file).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/Launchers.swift macos/TTStation/Tests/TTStationKitTests/LaunchersTests.swift
git commit -m "feat(macos): OpenWebUILauncher invocation builder"
```

---

## Task 3: LaunchController (app glue)

**Files:**
- Create: `macos/TTStation/AppShell/Sources/LaunchController.swift`

**Interfaces:**
- Consumes: `Endpoint`, `OpenCodeLauncher`, `OpenWebUILauncher` from `TTStationKit`.
- Produces: `@Observable @MainActor final class LaunchController` with `var isLaunchingWebUI: Bool`, `var isLaunchingOpenCode: Bool`, `var webUIError: String?`, `var openCodeError: String?`, `func openInOpenCode(endpoint: Endpoint) async`, `func openWebUI(endpoint: Endpoint) async`.

This task has no unit test (it drives external processes/UI); its deliverable is that it compiles and is wired in Task 4, and is verified by launching in Task 4's manual step. Build is the gate.

- [ ] **Step 1: Write the implementation**

`macos/TTStation/AppShell/Sources/LaunchController.swift`:
```swift
import AppKit
import Foundation
import Observation
import TTStationKit

@Observable @MainActor
final class LaunchController {
    var isLaunchingWebUI = false
    var isLaunchingOpenCode = false
    var webUIError: String?
    var openCodeError: String?

    // MARK: opencode

    func openInOpenCode(endpoint: Endpoint) async {
        isLaunchingOpenCode = true
        defer { isLaunchingOpenCode = false }
        openCodeError = nil
        guard Self.resolveBrewBinary("opencode") != nil else {
            openCodeError = "opencode not installed — run: brew install sst/tap/opencode"
            return
        }
        do {
            let dir = try Self.scratchDir(for: endpoint)
            let configURL = dir.appendingPathComponent("opencode.json")
            try OpenCodeLauncher.configJSON(for: endpoint)
                .write(to: configURL, atomically: true, encoding: .utf8)
            let cmd = OpenCodeLauncher.terminalCommand(configDir: dir.path)
            // Open Terminal.app and run the command in a new window.
            let script = """
            tell application "Terminal"
                activate
                do script "\(cmd.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\""))"
            end tell
            """
            try Self.runOsascript(script)
        } catch {
            openCodeError = error.localizedDescription
        }
    }

    // MARK: Open WebUI

    func openWebUI(endpoint: Endpoint) async {
        isLaunchingWebUI = true
        defer { isLaunchingWebUI = false }
        webUIError = nil

        // Already up? Just open the browser.
        if await Self.isHealthy() {
            NSWorkspace.shared.open(OpenWebUILauncher.url)
            return
        }
        guard let uvx = Self.resolveBrewBinary("uvx") else {
            webUIError = "uv not installed — run: brew install uv"
            return
        }
        let inv = OpenWebUILauncher.invocation(for: endpoint)
        do {
            try Self.spawnDetached(executable: uvx, args: inv.args, env: inv.env)
        } catch {
            webUIError = "failed to start Open WebUI: \(error.localizedDescription)"
            return
        }
        // Poll health up to ~90s (first run may still be resolving deps).
        for _ in 0..<90 {
            if await Self.isHealthy() {
                NSWorkspace.shared.open(OpenWebUILauncher.url)
                return
            }
            try? await Task.sleep(nanoseconds: 1_000_000_000)
        }
        webUIError = "Open WebUI didn't come up on :8080 — check the terminal/logs."
    }

    // MARK: helpers

    static func resolveBrewBinary(_ name: String) -> String? {
        for p in ["/opt/homebrew/bin/\(name)", "/usr/local/bin/\(name)"] {
            if FileManager.default.isExecutableFile(atPath: p) { return p }
        }
        return nil
    }

    static func scratchDir(for endpoint: Endpoint) throws -> URL {
        let safe = endpoint.baseURL
            .replacingOccurrences(of: "https://", with: "")
            .replacingOccurrences(of: "http://", with: "")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: ":", with: "_")
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("TTStation/opencode/\(safe)", isDirectory: true)
        try FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    static func runOsascript(_ script: String) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        p.arguments = ["-e", script]
        try p.run()
        p.waitUntilExit()
    }

    /// Spawn a long-lived process detached from the app (nohup + &) so it
    /// survives and keeps serving; merge a homebrew PATH so uvx works.
    static func spawnDetached(executable: String, args: [String], env: [String: String]) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        var environment = ProcessInfo.processInfo.environment
        environment["PATH"] = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
        for (k, v) in env { environment[k] = v }
        p.environment = environment
        let quoted = ([executable] + args).map { "'\($0)'" }.joined(separator: " ")
        p.arguments = ["-c", "nohup \(quoted) >/tmp/ttstation-openwebui.log 2>&1 &"]
        try p.run()
        p.waitUntilExit()
    }

    static func isHealthy() async -> Bool {
        var req = URLRequest(url: OpenWebUILauncher.healthURL)
        req.timeoutInterval = 2
        guard let (_, resp) = try? await URLSession.shared.data(for: req),
              let http = resp as? HTTPURLResponse else { return false }
        return http.statusCode == 200
    }
}
```

- [ ] **Step 2: Build to verify it compiles**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate && \
xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -4
```
Expected: `** BUILD SUCCEEDED **`. (LaunchController isn't referenced yet; it just needs to compile.)

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/AppShell/Sources/LaunchController.swift
git commit -m "feat(macos): LaunchController — spawn Open WebUI (uvx) + open opencode in Terminal"
```

---

## Task 4: Connect row in BoxDetailView + end-to-end launch

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/BoxDetailView.swift`

**Interfaces:**
- Consumes: `LaunchController` (Task 3), `BoxViewModel` (`box.endpoint: Endpoint?`).
- Produces: the wired UI. Verified by build + manual launch against the running box.

- [ ] **Step 1: Add the Connect row**

Read the current `macos/TTStation/AppShell/Sources/BoxDetailView.swift`. Inside the block that renders when the box is serving — the `if let ep = box.endpoint { ... }` block that already shows the endpoint + Copy button — add a `LaunchController` to the view and a Connect row below the Copy control. Concretely:

At the top of the `BoxDetailView` struct, add:
```swift
    @State private var launcher = LaunchController()
```

Immediately after the existing `if let ep = box.endpoint {` Copy-endpoint UI (still inside that `if let ep` block, so `ep` is in scope), add:
```swift
                // Connect a local front-end to the running model.
                HStack(spacing: 8) {
                    Text("Connect:").font(.caption).foregroundStyle(.secondary)
                    Button("Open Web UI") { Task { await launcher.openWebUI(endpoint: ep) } }
                        .disabled(launcher.isLaunchingWebUI)
                    Button("Open in opencode") { Task { await launcher.openInOpenCode(endpoint: ep) } }
                        .disabled(launcher.isLaunchingOpenCode)
                    if launcher.isLaunchingWebUI || launcher.isLaunchingOpenCode {
                        ProgressView().scaleEffect(0.6)
                    }
                }
                if let e = launcher.webUIError ?? launcher.openCodeError {
                    Text(e).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                }
```
(If the exact surrounding names differ, keep the intent: the row lives where `ep` is in scope, uses `launcher`, and shows spinners/errors. Do not move the existing Copy/endpoint UI.)

- [ ] **Step 2: Build**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate && \
xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | tail -4
```
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 3: Full package test (no regressions)**

Run: `cd macos/TTStation && swift test 2>&1 | tail -3`
Expected: whole suite passes (the Launchers unit tests plus all prior).

- [ ] **Step 4: Manual end-to-end (owner-run, box must be serving)**

Ensure the box is serving a model (`tt --json run <model> --host qb2-lab.local:8765`). Launch the app, open the box's panel, and:
- Click **Open in opencode** → a Terminal window opens running `opencode` in the scratch dir; confirm it can talk to the model (the `ttstation` provider is preselected).
- Click **Open Web UI** → (first time may take a bit while `uvx` resolves) a browser opens `localhost:8080`; send a message and confirm a completion from the box's model.
Capture any error text the buttons surface and fix in Task 3 if the invocation shape is wrong.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/Sources/BoxDetailView.swift
git commit -m "feat(macos): Connect row — one-click Open WebUI / opencode from the menu bar"
```

---

## Self-Review Notes

- **Spec coverage:** OpenCodeLauncher builder (Task 1) ✓; OpenWebUILauncher builder (Task 2) ✓; LaunchController glue with prechecks, detached uvx spawn, health poll, Terminal launch, scratch dir (Task 3) ✓; Connect row shown only when `endpoint != nil`, spinners + errors (Task 4) ✓; local-uvx + dedicated-scratch-dir decisions honored ✓; deferred items (Docker, remote Open WebUI, project cwd, Shortcuts) correctly absent ✓.
- **Placeholder scan:** none — every code step is complete; the one "adapt if names differ" note in Task 4 is guidance around a file the implementer must read, with the concrete code given.
- **Type consistency:** `Endpoint(baseURL:model:requiresKey:)`, `OpenCodeLauncher.configJSON/terminalCommand`, `OpenWebUILauncher.invocation/url/healthURL`, and `LaunchController`'s property/method names are used identically in Tasks 3–4 as defined in 1–3.
- **Known runtime notes:** first `uvx open-webui` run is slow (pre-warm before demo); `opencode` model id contains a `/`, giving a selection `ttstation/<vendor>/<model>` — opencode splits provider on the first `/`, so this resolves. Verify in Task 4's manual run.

# TTStation Box Workbench Launchers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a "Workbench" launcher row to the TTStation window — one-click **Terminal → box (SSH)**, **tt-toplike (remote telemetry)**, and **VS Code (Remote-SSH)** — all keyed off the box the app already knows.

**Architecture:** Pure command/argv builders in `TTStationKit` (unit-tested); glue in `AppShell`'s `LaunchController` (osascript Terminal / `code` process, prechecks, per-launcher state); buttons in `BoxWorkspaceView`. Mirrors the existing Open WebUI / opencode launchers, but keyed off the box's host+ctrlPort+ssh-user instead of the serving `/v1` endpoint.

**Tech Stack:** Swift 5, SwiftUI/Observation, Foundation `Process`, `osascript` (Terminal.app), `ssh`, `code` CLI (Remote-SSH installed), `tt-toplike-tui` (installed at `~/.local/bin`).

## Global Constraints

- macOS 14; Swift 5; code under `macos/TTStation/`; additive (existing launchers/views reused).
- Launchers key off the BOX: `host` (canonical, trailing `.` stripped), `ctrlPort` (`BoxRecord.ctrlPort`), and SSH `user` (default `NSUserName()`, override `UserDefaults` key `tt.sshUser`).
- Commands (exact):
  - Terminal SSH: `ssh -o StrictHostKeyChecking=accept-new '<user>@<host>'`
  - tt-toplike: `tt-toplike-tui --remote '<host>:<ctrlPort>'`
  - VS Code: argv `["--remote", "ssh-remote+<user>@<host>", "<path>"]`, path default `/home/<user>`
- `tt-toplike-tui` lives in `~/.local/bin` (extend binary lookup to include it); `code` is at `/usr/local/bin/code`; `ssh` is always present (no precheck).
- The Workbench section is shown for any selected box (SSH/telemetry don't require pairing/serving).
- Verify: `swift test` green (builders unit-tested); app `xcodebuild` succeeds; owner click-through.

---

## File Structure

```
macos/TTStation/
  Sources/TTStationKit/WorkbenchLaunchers.swift    # NEW: SSHTarget + 3 pure builders
  Tests/TTStationKitTests/WorkbenchLaunchersTests.swift  # NEW: unit tests
  AppShell/Sources/LaunchController.swift           # MODIFY: 3 launch methods + state + helpers
  AppShell/Sources/BoxWorkspaceView.swift           # MODIFY: add "Workbench" section
```

---

## Task 1: Pure builders — `SSHTarget` + Terminal/tt-toplike/VSCode

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/WorkbenchLaunchers.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/WorkbenchLaunchersTests.swift`

**Interfaces:**
- Consumes: nothing.
- Produces: `SSHTarget { user; host; static resolve(host:overrideUser:currentUser:) }`, `enum TerminalSSHLauncher { static command(user:host:) -> String }`, `enum TTToplikeLauncher { static command(host:ctrlPort:) -> String }`, `enum VSCodeLauncher { static remoteArgs(user:host:path:) -> [String]; static defaultRemotePath(user:) -> String }`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/WorkbenchLaunchersTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class WorkbenchLaunchersTests: XCTestCase {
    func testSSHTargetStripsTrailingDotAndDefaultsUser() {
        let t = SSHTarget.resolve(host: "qb.local.", overrideUser: nil, currentUser: "me")
        XCTAssertEqual(t.host, "qb.local")
        XCTAssertEqual(t.user, "me")
    }
    func testSSHTargetHonorsOverrideUserAndKeepsBareHost() {
        let t = SSHTarget.resolve(host: "qb.local", overrideUser: "boxuser", currentUser: "me")
        XCTAssertEqual(t.host, "qb.local")
        XCTAssertEqual(t.user, "boxuser")
    }
    func testSSHTargetEmptyOverrideFallsBackToCurrent() {
        let t = SSHTarget.resolve(host: "qb.local", overrideUser: "", currentUser: "me")
        XCTAssertEqual(t.user, "me")
    }
    func testTerminalSSHCommand() {
        XCTAssertEqual(TerminalSSHLauncher.command(user: "me", host: "qb.local"),
                       "ssh -o StrictHostKeyChecking=accept-new 'me@qb.local'")
    }
    func testTTToplikeCommand() {
        XCTAssertEqual(TTToplikeLauncher.command(host: "qb.local", ctrlPort: 8765),
                       "tt-toplike-tui --remote 'qb.local:8765'")
    }
    func testVSCodeRemoteArgs() {
        XCTAssertEqual(VSCodeLauncher.remoteArgs(user: "me", host: "qb.local", path: "/home/me"),
                       ["--remote", "ssh-remote+me@qb.local", "/home/me"])
        XCTAssertEqual(VSCodeLauncher.defaultRemotePath(user: "me"), "/home/me")
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter WorkbenchLaunchersTests`
Expected: FAIL — types undefined.

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Sources/TTStationKit/WorkbenchLaunchers.swift`:
```swift
import Foundation

/// An SSH target: which user on which host. `resolve` canonicalizes the host
/// (mDNS names arrive as FQDNs with a trailing dot) and picks the user
/// (an explicit override, else the current login name).
public struct SSHTarget: Equatable {
    public let user: String
    public let host: String
    public init(user: String, host: String) { self.user = user; self.host = host }

    public static func resolve(host: String, overrideUser: String?, currentUser: String) -> SSHTarget {
        let canonicalHost = host.hasSuffix(".") ? String(host.dropLast()) : host
        let user = (overrideUser.map { $0.isEmpty ? currentUser : $0 }) ?? currentUser
        return SSHTarget(user: user, host: canonicalHost)
    }
}

/// `ssh` into the box. `accept-new` lets a first connect to an unknown host key
/// through (still prompts for a password if key auth isn't set up — fine, that
/// happens in the Terminal the app opens).
public enum TerminalSSHLauncher {
    public static func command(user: String, host: String) -> String {
        "ssh -o StrictHostKeyChecking=accept-new '\(user)@\(host)'"
    }
}

/// tt-toplike's remote telemetry view against the box's control port.
public enum TTToplikeLauncher {
    public static func command(host: String, ctrlPort: Int) -> String {
        "tt-toplike-tui --remote '\(host):\(ctrlPort)'"
    }
}

/// A VS Code Remote-SSH window on the box (integrated terminal runs on the box).
public enum VSCodeLauncher {
    public static func remoteArgs(user: String, host: String, path: String) -> [String] {
        ["--remote", "ssh-remote+\(user)@\(host)", path]
    }
    public static func defaultRemotePath(user: String) -> String { "/home/\(user)" }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter WorkbenchLaunchersTests`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/WorkbenchLaunchers.swift macos/TTStation/Tests/TTStationKitTests/WorkbenchLaunchersTests.swift
git commit -m "feat(macos): workbench launcher builders (SSH/tt-toplike/VSCode)"
```

---

## Task 2: `LaunchController` — workbench launch methods

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/LaunchController.swift`

**Interfaces:**
- Consumes: `SSHTarget`, `TerminalSSHLauncher`, `TTToplikeLauncher`, `VSCodeLauncher` (Task 1); existing `runOsascript`, `resolveBrewBinary`.
- Produces, on `LaunchController`: state `isLaunchingTerminal/isLaunchingToplike/isLaunchingVSCode: Bool`, `terminalError/toplikeError/vscodeError: String?`; methods `openTerminalSSH(host:)`, `openTTToplike(host:ctrlPort:)`, `openVSCode(host:)`; helpers `terminalScript(_:)`, `runDetachedProcess(executable:args:)`, and an extended binary lookup that also checks `~/.local/bin`.

- [ ] **Step 1: Add the state, helpers, and methods**

In `LaunchController.swift`, add these observable fields alongside the existing launcher state:
```swift
    var isLaunchingTerminal = false
    var isLaunchingToplike = false
    var isLaunchingVSCode = false
    var terminalError: String?
    var toplikeError: String?
    var vscodeError: String?
```

Extend the binary lookup to include `~/.local/bin` (where `tt-toplike-tui`/`tt` live). Replace the body of the existing `resolveBrewBinary(_:)` so it also checks the user's `~/.local/bin`:
```swift
    static func resolveBrewBinary(_ name: String) -> String? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        for p in ["\(home)/.local/bin/\(name)", "/opt/homebrew/bin/\(name)", "/usr/local/bin/\(name)"] {
            if FileManager.default.isExecutableFile(atPath: p) { return p }
        }
        return nil
    }
```

Add helpers (place near `runOsascript`):
```swift
    /// Wrap a shell command in an AppleScript that opens/reuses Terminal.app and
    /// runs it in a new window. Escapes for embedding in the AppleScript literal.
    static func terminalScript(_ command: String) -> String {
        let escaped = command
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        return """
        tell application "Terminal"
            activate
            do script "\(escaped)"
        end tell
        """
    }

    /// Launch a GUI helper (e.g. `code`) without blocking; it returns promptly
    /// after signalling/launching its own window.
    static func runDetachedProcess(executable: String, args: [String]) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: executable)
        p.arguments = args
        try p.run()
    }

    /// SSH user/host for a box host string (override via UserDefaults `tt.sshUser`,
    /// else the current login name).
    private func sshTarget(host: String) -> SSHTarget {
        SSHTarget.resolve(
            host: host,
            overrideUser: UserDefaults.standard.string(forKey: "tt.sshUser"),
            currentUser: NSUserName()
        )
    }
```

Add the three launch methods:
```swift
    // MARK: Workbench (box-connected tools)

    func openTerminalSSH(host: String) async {
        isLaunchingTerminal = true; defer { isLaunchingTerminal = false }
        terminalError = nil
        let t = sshTarget(host: host)
        do { try Self.runOsascript(Self.terminalScript(TerminalSSHLauncher.command(user: t.user, host: t.host))) }
        catch { terminalError = error.localizedDescription }
    }

    func openTTToplike(host: String, ctrlPort: Int) async {
        isLaunchingToplike = true; defer { isLaunchingToplike = false }
        toplikeError = nil
        guard Self.resolveBrewBinary("tt-toplike-tui") != nil else {
            toplikeError = "tt-toplike not installed — build tt-toplike-tui from ~/code/tt-toplike (inference-server-monitoring branch)."
            return
        }
        let t = sshTarget(host: host)
        do { try Self.runOsascript(Self.terminalScript(TTToplikeLauncher.command(host: t.host, ctrlPort: ctrlPort))) }
        catch { toplikeError = error.localizedDescription }
    }

    func openVSCode(host: String) async {
        isLaunchingVSCode = true; defer { isLaunchingVSCode = false }
        vscodeError = nil
        guard let code = Self.resolveBrewBinary("code") else {
            vscodeError = "VS Code `code` CLI not found — in VS Code run “Shell Command: Install 'code' command in PATH”."
            return
        }
        let t = sshTarget(host: host)
        let args = VSCodeLauncher.remoteArgs(user: t.user, host: t.host, path: VSCodeLauncher.defaultRemotePath(user: t.user))
        do { try Self.runDetachedProcess(executable: code, args: args) }
        catch { vscodeError = "failed to open VS Code: \(error.localizedDescription)" }
    }
```
(If `runOsascript` is currently `private`, leave it as-is — these methods are on the same type. Import `TTStationKit` is already present for the launcher types; keep `import AppKit`/`Foundation`.)

- [ ] **Step 2: Build to verify**

Run: `cd macos/TTStation/AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | grep -iE "error:|warning:|BUILD SUCCEEDED|BUILD FAILED"`
Expected: `** BUILD SUCCEEDED **`, no warnings. (Methods are unused until Task 3 — but they're `internal`/instance methods, so no dead-code warning.)

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/AppShell/Sources/LaunchController.swift
git commit -m "feat(macos): LaunchController workbench methods (SSH terminal / tt-toplike / VSCode)"
```

---

## Task 3: `BoxWorkspaceView` — the Workbench section

**Files:**
- Modify: `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift`

**Interfaces:**
- Consumes: `LaunchController` methods from Task 2; `box.record.host`, `box.record.ctrlPort`.
- Produces: a "Workbench" section in the workspace with Terminal / tt-toplike / VS Code buttons.

- [ ] **Step 1: Add the Workbench section**

`BoxWorkspaceView` already has `@State private var launcher = LaunchController()`. Add a Workbench group to its `VStack` — put it just before the trailing `if let err = box.errorText` block, so it shows for any selected box (paired or not):
```swift
                Divider()
                Text("Workbench").font(.caption).foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    Button { Task { await launcher.openTerminalSSH(host: box.record.host) } } label: {
                        Label("Terminal", systemImage: "terminal")
                    }
                    .disabled(launcher.isLaunchingTerminal)
                    .help("Open a Terminal SSH'd into this box.")

                    Button { Task { await launcher.openTTToplike(host: box.record.host, ctrlPort: box.record.ctrlPort) } } label: {
                        Label("tt-toplike", systemImage: "waveform.path.ecg")
                    }
                    .disabled(launcher.isLaunchingToplike)
                    .help("Open tt-toplike showing this box's live telemetry.")

                    Button { Task { await launcher.openVSCode(host: box.record.host) } } label: {
                        Label("VS Code", systemImage: "chevron.left.forwardslash.chevron.right")
                    }
                    .disabled(launcher.isLaunchingVSCode)
                    .help("Open a VS Code Remote-SSH window on this box.")

                    if launcher.isLaunchingTerminal || launcher.isLaunchingToplike || launcher.isLaunchingVSCode {
                        ProgressView().scaleEffect(0.6)
                    }
                }
                .controlSize(.small)
                if let e = launcher.terminalError ?? launcher.toplikeError ?? launcher.vscodeError {
                    Text(e).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                }
```
Do not remove or alter the existing pairing / model browser / Run / serving-model Connect / `/serving` sections — this is purely additive.

- [ ] **Step 2: Build to verify**

Run: `cd macos/TTStation/AppShell && xcodegen generate && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build 2>&1 | grep -iE "error:|warning:|BUILD SUCCEEDED|BUILD FAILED"`
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 3: Commit**

```bash
git add macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift
git commit -m "feat(macos): Workbench section — Terminal / tt-toplike / VS Code launchers in the window"
```

---

## Task 4: Build-verify + owner click-through

**Files:** none (verification).

- [ ] **Step 1: Package tests**

Run: `cd macos/TTStation && swift test 2>&1 | tail -3`
Expected: all pass (adds the 6 `WorkbenchLaunchersTests`).

- [ ] **Step 2: App build**

Run the app build command. Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 3: Owner click-through (box discoverable)**

Launch the app, open the window, select the box, and in the **Workbench** section:
- **Terminal** → a Terminal window opens running `ssh …@<box>` (accept the host key / enter password if prompted).
- **tt-toplike** → a Terminal opens showing the box's live telemetry (`tt-toplike-tui --remote <box>:8765`).
- **VS Code** → a Remote-SSH window opens on the box (its integrated terminal is on the box; `tenstorrent.tt-vscode-toolkit` is installed locally).
Capture anything broken and fix in the owning task. Note: passwordless SSH (Mac key on the box) makes Terminal/VS Code seamless; without it they prompt for a password, which is acceptable.

---

## Self-Review Notes

- **Spec coverage:** SSHTarget + 3 builders (Task 1) ✓; LaunchController glue with prechecks + `~/.local/bin` lookup for tt-toplike-tui (Task 2) ✓; Workbench section for any selected box, keyed off `box.record.host`/`ctrlPort` (Task 3) ✓; existing Open WebUI/opencode untouched ✓; unit tests for the pure builders (Task 1) + build + click-through (Task 4) ✓; deferred items (passwordless SSH automation, remote toolkit install, SSH-user UI) correctly absent ✓.
- **Placeholder scan:** none — every code step is complete.
- **Type consistency:** `SSHTarget.resolve(host:overrideUser:currentUser:)`, `TerminalSSHLauncher.command(user:host:)`, `TTToplikeLauncher.command(host:ctrlPort:)`, `VSCodeLauncher.remoteArgs(user:host:path:)`/`defaultRemotePath(user:)`, and `LaunchController.{openTerminalSSH(host:),openTTToplike(host:ctrlPort:),openVSCode(host:),isLaunching*/,*Error,resolveBrewBinary,runOsascript,terminalScript,runDetachedProcess}` are used identically across tasks. `box.record.host`/`box.record.ctrlPort` match `BoxRecord`.
- **Note for Task 2:** `resolveBrewBinary` is extended (not replaced in behavior) to also check `~/.local/bin`; existing callers (`uvx`, `opencode`, `code`) are unaffected since those dirs are still checked.

import AppKit
import Foundation
import Observation
import TTStationKit

/// Side-effecting glue that turns a box `Endpoint` into a running local
/// front-end: opencode in Terminal.app, or Open WebUI in the browser.
///
/// The pure builders (`OpenCodeLauncher`, `OpenWebUILauncher`) decide *what* to
/// run; this type does the `Process`/`osascript`/`NSWorkspace` work and tracks
/// in-flight state + errors so the view can disable/spin its buttons. It is
/// owner-verified by launching (not unit-tested), so it stays as thin as
/// possible over the tested builders.
@Observable @MainActor
final class LaunchController {
    var isLaunchingWebUI = false
    var isLaunchingOpenCode = false
    var webUIError: String?
    var openCodeError: String?
    var isLaunchingTerminal = false
    var isLaunchingToplike = false
    var isLaunchingVSCode = false
    var terminalError: String?
    var toplikeError: String?
    var vscodeError: String?

    // MARK: opencode

    /// Write a per-box `opencode.json` and open Terminal.app running
    /// `cd <scratchDir> && opencode`. Prechecks for a `opencode` binary first
    /// so a missing install surfaces an actionable message instead of a
    /// terminal that just prints "command not found".
    func openInOpenCode(endpoint: Endpoint) async {
        isLaunchingOpenCode = true
        defer { isLaunchingOpenCode = false }
        openCodeError = nil

        // Precheck: opencode present? Probe the usual homebrew locations. (We
        // still run it *inside* Terminal's login shell, but this catches the
        // "not installed" case up front with a fixable message.)
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
            // Escape backslashes then double-quotes so the command survives
            // being embedded in the AppleScript string literal below.
            let escaped = cmd
                .replacingOccurrences(of: "\\", with: "\\\\")
                .replacingOccurrences(of: "\"", with: "\\\"")
            let script = """
            tell application "Terminal"
                activate
                do script "\(escaped)"
            end tell
            """
            try Self.runOsascript(script)
        } catch {
            openCodeError = error.localizedDescription
        }
    }

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
            vscodeError = "VS Code `code` CLI not found — in VS Code run \"Shell Command: Install 'code' command in PATH\"."
            return
        }
        let t = sshTarget(host: host)
        let args = VSCodeLauncher.remoteArgs(user: t.user, host: t.host, path: VSCodeLauncher.defaultRemotePath(user: t.user))
        do { try Self.runDetachedProcess(executable: code, args: args) }
        catch { vscodeError = "failed to open VS Code: \(error.localizedDescription)" }
    }

    // MARK: Open WebUI

    /// Ensure a local Open WebUI is up on :8080 wired to `endpoint`, then open
    /// the browser. Reuses an already-running instance (health 200), otherwise
    /// spawns `uvx open-webui serve …` detached and polls health (~90s, since
    /// the first run may still be resolving deps).
    func openWebUI(endpoint: Endpoint) async {
        isLaunchingWebUI = true
        defer { isLaunchingWebUI = false }
        webUIError = nil

        // Already up? Just open the browser (reattach — don't double-spawn).
        if await Self.isHealthy() {
            NSWorkspace.shared.open(OpenWebUILauncher.url)
            return
        }
        // Precheck: uvx present?
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

    /// Resolve a homebrew-installed binary by absolute path. GUI apps don't
    /// inherit the shell PATH, so we can't rely on `command -v` from the app
    /// process — probe the known install dirs directly.
    static func resolveBrewBinary(_ name: String) -> String? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        for p in ["\(home)/.local/bin/\(name)", "/opt/homebrew/bin/\(name)", "/usr/local/bin/\(name)"] {
            if FileManager.default.isExecutableFile(atPath: p) { return p }
        }
        return nil
    }

    /// A dedicated per-box scratch dir under Application Support, keyed by a
    /// filesystem-safe form of the endpoint's host:port. Created if needed.
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

    /// Spawn a long-lived process detached from the app (`nohup … &`) so it
    /// survives the app quitting and keeps serving. Merge a homebrew PATH so
    /// `uvx` can resolve its own subtools.
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

    /// True when Open WebUI's health endpoint returns 200. Short timeout so the
    /// reuse-check and each poll tick stay snappy.
    static func isHealthy() async -> Bool {
        var req = URLRequest(url: OpenWebUILauncher.healthURL)
        req.timeoutInterval = 2
        guard let (_, resp) = try? await URLSession.shared.data(for: req),
              let http = resp as? HTTPURLResponse else { return false }
        return http.statusCode == 200
    }
}

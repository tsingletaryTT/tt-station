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
    /// Progress note for the current opencode launch stage (e.g.
    /// "Installing opencode…"), so the Connect card can show it while the
    /// spinner is up. Cleared on completion/failure.
    var openCodePhase: String?
    /// Progress note for the current Open WebUI launch stage. Cleared on
    /// completion/failure.
    var webUIPhase: String?
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
        defer { isLaunchingOpenCode = false; openCodePhase = nil }
        openCodeError = nil

        // Precheck: opencode present? Probe the usual homebrew locations. (We
        // still run it *inside* Terminal's login shell, but this catches the
        // "not installed" case up front.) If it's missing, install it now via
        // brew instead of erroring — Connect actions should come up fast, not
        // send the user off to a terminal to install a dependency by hand.
        if Self.resolveBrewBinary("opencode") == nil {
            openCodePhase = "Installing opencode…"
            let installed = await Self.runBrewInstall(formula: Provisioning.opencodeFormula)
            guard installed, Self.resolveBrewBinary("opencode") != nil else {
                openCodeError = Self.resolveBrewBinary("brew") == nil
                    ? "Homebrew not found — install it from https://brew.sh, then retry."
                    : "opencode install failed — run `brew install \(Provisioning.opencodeFormula)` manually to see why."
                return
            }
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
        let remoteHost = Self.resolveIPv4(t.host) ?? t.host
        do { try Self.runOsascript(Self.terminalScript(TTToplikeLauncher.command(host: remoteHost, ctrlPort: ctrlPort))) }
        catch { toplikeError = error.localizedDescription }
    }

    /// Opens a Remote-SSH window on the box and, as a separate best-effort
    /// `code` call, installs Tenstorrent's tt-vscode-toolkit extension —
    /// preferring a locally-cached `.vsix` (seeded by install.sh from the
    /// latest GitHub release) so it installs regardless of which marketplace
    /// the user's VS Code is pointed at, falling back to the marketplace ID
    /// only when no `.vsix` is cached. The toolkit install is non-fatal: if it
    /// fails, the Remote-SSH window still opens; only a failure to open the
    /// window itself surfaces an error.
    func openVSCode(host: String) async {
        isLaunchingVSCode = true; defer { isLaunchingVSCode = false }
        vscodeError = nil
        guard let code = Self.resolveBrewBinary("code") else {
            vscodeError = "VS Code `code` CLI not found — in VS Code run \"Shell Command: Install 'code' command in PATH\"."
            return
        }
        // Resolve `.local` to IPv4 first. VS Code Remote-SSH runs
        // `ssh -o ConnectTimeout=15 ttuser@<host>`, and macOS resolves mDNS
        // names IPv6-first — so ssh burns the whole timeout cycling through
        // unreachable link-local `fe80::…` addresses and gives up before it
        // reaches the working IPv4. Handing VS Code the IPv4 makes it connect
        // immediately (same fix as the Open WebUI / tt-toplike launchers).
        let t = sshTarget(host: Self.resolveIPv4(host) ?? host)
        let path = VSCodeLauncher.defaultRemotePath(user: t.user)

        // Toolkit install is a SEPARATE, best-effort `code` invocation run
        // first: `--install-extension` makes the CLI run headless and exit
        // WITHOUT opening a window, so it can never share an invocation with
        // the window-open below (that was the "does nothing" bug). Its failure
        // is a nice-to-have miss, not a reason to skip the window.
        //
        // Prefer a locally-cached `.vsix` (seeded by install.sh from the latest
        // GitHub release) — that install is gallery-independent, so it works
        // even when the user's VS Code isn't pointed at the default marketplace.
        // Only fall back to the marketplace ID when no `.vsix` is cached.
        let installArgs = Self.cachedToolkitVsix().map { VSCodeLauncher.installVsixArgs(vsixPath: $0.path) }
            ?? VSCodeLauncher.installExtensionArgs()
        try? Self.runDetachedProcess(executable: code, args: installArgs)

        // The window-open is the primary action — surface an error only if THIS
        // fails (a failed extension install must not block or error the window).
        do {
            try Self.runDetachedProcess(
                executable: code,
                args: VSCodeLauncher.remoteArgs(user: t.user, host: t.host, path: path)
            )
        } catch {
            vscodeError = "failed to open VS Code: \(error.localizedDescription)"
        }
    }

    // MARK: Open WebUI (box-hosted)

    /// Ensure Open WebUI is up **on the box** wired to the box's local vLLM,
    /// then open a browser tab to it. Open WebUI runs as a docker container on
    /// the QuietBox (not on the Mac) — this removes every Mac-side install
    /// failure mode; the Mac only SSHes the launch and opens the browser.
    ///
    /// Flow: derive the box host + serving port from `endpoint.baseURL`;
    /// reuse the container if it's already healthy (open browser, done);
    /// otherwise SSH the idempotent `docker run` to the box and poll the box's
    /// health endpoint (up to ~180s — the first run pulls the image and
    /// initializes) before opening the browser.
    func openWebUI(endpoint: Endpoint) async {
        isLaunchingWebUI = true
        defer { isLaunchingWebUI = false; webUIPhase = nil }
        webUIError = nil

        guard let comps = URLComponents(string: endpoint.baseURL),
            let rawHost = comps.host,
            let servingPort = comps.port
        else {
            webUIError = "couldn't parse the box endpoint: \(endpoint.baseURL)"
            return
        }
        let canonical = rawHost.hasSuffix(".") ? String(rawHost.dropLast()) : rawHost
        // Resolve `.local` to IPv4 up front. macOS resolves mDNS names
        // IPv6-first → a zoned link-local `fe80::…` that URLSession can't use
        // (so the "already healthy → just open the browser" fast path would
        // never fire) and that SSH would connect from/to. Same fix the
        // tt-toplike launcher already applies (commit 2fd6ef2). Fall back to
        // the name if resolution fails.
        let host = Self.resolveIPv4(canonical) ?? canonical
        let webURL = OpenWebUILauncher.url(host: host)
        let healthURL = OpenWebUILauncher.healthURL(host: host)

        // Already up on the box? Just open the browser (reuse — don't relaunch).
        if await Self.isHealthy(healthURL) {
            NSWorkspace.shared.open(webURL)
            return
        }

        webUIPhase = "Starting Open WebUI on the box…"
        let target = sshTarget(host: host)
        let ok = await Self.runSSHCommand(
            user: target.user, host: target.host,
            command: OpenWebUILauncher.dockerCommand(servingPort: servingPort))
        guard ok else {
            webUIError =
                "couldn't start Open WebUI on the box over SSH — check that `ttuser` SSH and docker are set up (pair installs the key)."
            return
        }

        // Poll the box health endpoint (~180s: first run pulls the image).
        for _ in 0..<180 {
            if await Self.isHealthy(healthURL) {
                NSWorkspace.shared.open(webURL)
                return
            }
            try? await Task.sleep(nanoseconds: 1_000_000_000)
        }
        webUIError =
            "Open WebUI didn't come up on \(host):\(OpenWebUILauncher.hostPort) — first-run image pull may still be going; retry shortly."
    }

    // MARK: helpers

    /// Resolve `host` to its first IPv4 address. macOS resolves `.local` mDNS
    /// names IPv6-first, and tt-toplike's WS client connects to the first
    /// address (a broken link-local IPv6) instead of the working IPv4 — so we
    /// hand it an IPv4 explicitly. Returns nil (caller falls back to the name)
    /// if resolution fails.
    static func resolveIPv4(_ host: String) -> String? {
        var hints = addrinfo()
        hints.ai_family = AF_INET
        hints.ai_socktype = SOCK_STREAM
        var res: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &res) == 0, let head = res else { return nil }
        defer { freeaddrinfo(res) }
        var node: UnsafeMutablePointer<addrinfo>? = head
        while let n = node {
            if let sa = n.pointee.ai_addr, n.pointee.ai_family == AF_INET {
                var storage = sockaddr_in()
                memcpy(&storage, sa, min(Int(n.pointee.ai_addrlen), MemoryLayout<sockaddr_in>.size))
                var addr = storage.sin_addr
                var buf = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
                if inet_ntop(AF_INET, &addr, &buf, socklen_t(INET_ADDRSTRLEN)) != nil {
                    return String(cString: buf)
                }
            }
            node = n.pointee.ai_next
        }
        return nil
    }

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

    /// Runs `brew install <formula>` (args from `Provisioning.brewInstallArgs`)
    /// and waits for it to finish. Homebrew itself is the one dependency we
    /// never auto-install (callers surface a brew.sh pointer when it's
    /// missing); everything downstream of it (opencode, uv, …) is fair game
    /// to install on demand so Connect actions come up without a detour to a
    /// terminal.
    ///
    /// Uses a `terminationHandler` + continuation rather than
    /// `waitUntilExit()` so the (potentially tens-of-seconds-long) install
    /// doesn't block this actor's synchronous execution — `await` here is a
    /// real suspension point, not a busy-wait.
    static func runBrewInstall(formula: String) async -> Bool {
        guard let brew = resolveBrewBinary("brew") else { return false }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: brew)
        p.arguments = Provisioning.brewInstallArgs(formula: formula)
        return await withCheckedContinuation { continuation in
            p.terminationHandler = { proc in
                continuation.resume(returning: proc.terminationStatus == 0)
            }
            do {
                try p.run()
            } catch {
                continuation.resume(returning: false)
            }
        }
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

    /// Run a command on the box over SSH and wait for it to finish, returning
    /// whether it exited 0. Used to launch the box-hosted Open WebUI container
    /// (`docker run` includes a first-run image pull, so this can take a
    /// while — hence the async continuation rather than a blocking
    /// `waitUntilExit`, mirroring `runBrewInstall`).
    ///
    /// Auth is pinned to the exact key the pair flow authorizes on the box
    /// (`tt ssh-authorize` installs `~/.ssh/id_ed25519.pub`):
    /// - `-i ~/.ssh/id_ed25519` + `IdentitiesOnly=yes` offer only that key
    ///   (not whatever an agent happens to hold);
    /// - `PreferredAuthentications=publickey` + `BatchMode=yes` never fall back
    ///   to a password prompt on a headless launch — so a rejected key fails
    ///   fast with a real "publickey" error instead of the confusing repeated
    ///   `Failed password` the box logged.
    /// `accept-new` lets a first connection to an unknown host key through.
    static func runSSHCommand(user: String, host: String, command: String) async -> Bool {
        let key = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".ssh/id_ed25519").path
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
        p.arguments = [
            "-i", key,
            "-o", "IdentitiesOnly=yes",
            "-o", "PreferredAuthentications=publickey",
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "ConnectTimeout=10",
            "\(user)@\(host)", command,
        ]
        return await withCheckedContinuation { continuation in
            p.terminationHandler = { proc in
                continuation.resume(returning: proc.terminationStatus == 0)
            }
            do {
                try p.run()
            } catch {
                continuation.resume(returning: false)
            }
        }
    }

    /// The local cache dir where install.sh drops the tt-vscode-toolkit
    /// `.vsix` downloaded from the latest GitHub release.
    static func vsixCacheDir() -> URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("TTStation/vsix", isDirectory: true)
    }

    /// The newest cached toolkit `.vsix`, or nil if none is present. Picks the
    /// most recently modified `*.vsix` so a re-download (newer release) wins.
    static func cachedToolkitVsix() -> URL? {
        let dir = vsixCacheDir()
        guard let files = try? FileManager.default.contentsOfDirectory(
            at: dir, includingPropertiesForKeys: [.contentModificationDateKey])
        else { return nil }
        return files
            .filter { $0.pathExtension == "vsix" }
            .max { a, b in
                let da = (try? a.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate ?? .distantPast
                let db = (try? b.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate ?? .distantPast
                return da < db
            }
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

    /// True when `healthURL` returns 200. Short timeout so the reuse-check and
    /// each poll tick stay snappy.
    static func isHealthy(_ healthURL: URL) async -> Bool {
        var req = URLRequest(url: healthURL)
        req.timeoutInterval = 2
        guard let (_, resp) = try? await URLSession.shared.data(for: req),
              let http = resp as? HTTPURLResponse else { return false }
        return http.statusCode == 200
    }
}

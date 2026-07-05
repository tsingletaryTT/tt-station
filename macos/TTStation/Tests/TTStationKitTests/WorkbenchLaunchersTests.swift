import XCTest
@testable import TTStationKit

final class WorkbenchLaunchersTests: XCTestCase {
    func testSSHTargetStripsTrailingDotAndDefaultsUser() {
        let t = SSHTarget.resolve(host: "qb.local.", overrideUser: nil, currentUser: "me")
        XCTAssertEqual(t.host, "qb.local")
        XCTAssertEqual(t.user, "ttuser")
    }
    func testSSHTargetHonorsOverrideUserAndKeepsBareHost() {
        let t = SSHTarget.resolve(host: "qb.local", overrideUser: "boxuser", currentUser: "me")
        XCTAssertEqual(t.host, "qb.local")
        XCTAssertEqual(t.user, "boxuser")
    }
    func testSSHTargetEmptyOverrideFallsBackToDefault() {
        let t = SSHTarget.resolve(host: "qb.local", overrideUser: "", currentUser: "me")
        XCTAssertEqual(t.user, "ttuser")
    }
    // Task 8: the QuietBox 2 login is `ttuser`, not the Mac login name — the
    // default MUST be `ttuser` regardless of who's logged into the Mac.
    func testSSHTargetDefaultsToTtuser() {
        let t = SSHTarget.resolve(host: "qb2-lab.local", overrideUser: nil, currentUser: "tsingletary")
        XCTAssertEqual(t.user, "ttuser")   // NOT the Mac login
    }
    func testSSHTargetOverrideWins() {
        let t = SSHTarget.resolve(host: "qb2-lab.local", overrideUser: "someone", currentUser: "tsingletary")
        XCTAssertEqual(t.user, "someone")
    }
    func testTerminalSSHCommand() {
        XCTAssertEqual(TerminalSSHLauncher.command(user: "me", host: "qb.local"),
                       "ssh -o StrictHostKeyChecking=accept-new 'me@qb.local'")
    }
    func testTTToplikeCommand() {
        XCTAssertEqual(TTToplikeLauncher.command(host: "qb.local", ctrlPort: 8765),
                       "tt-toplike-tui --remote 'qb.local:8765'")
    }
    // NOTE: a naive `!cmd.contains("'; open")` check is unsound here — the
    // *correctly* escaped output for this payload legitimately contains that
    // substring (POSIX-escaping a quote emits `'\''`, whose trailing `''`
    // butts up against the literal `"; open"` that follows). That would fail
    // this test even on a correct implementation. Instead we pin down the
    // exact expected escaped string, built independently of the production
    // `shellSingleQuoted` helper, so the test actually proves correctness
    // rather than being tautological or accidentally unsatisfiable.
    func testTerminalSSHCommandNeutralizesSingleQuoteInjection() {
        let maliciousHost = "x'; open -a Calculator; '"
        let escapedQuote = "'\\''" // POSIX: close-quote, escaped quote, reopen-quote
        let escapedHost = "x" + escapedQuote + "; open -a Calculator; " + escapedQuote
        let expected = "ssh -o StrictHostKeyChecking=accept-new 'me@\(escapedHost)'"
        let cmd = TerminalSSHLauncher.command(user: "me", host: maliciousHost)
        XCTAssertEqual(cmd, expected)
        XCTAssertTrue(cmd.contains(escapedQuote), "expected the embedded quote to be escaped")
    }
    func testTTToplikeCommandNeutralizesSingleQuoteInjection() {
        let maliciousHost = "x'; open -a Calculator; '"
        let escapedQuote = "'\\''"
        let escapedCombined = "x" + escapedQuote + "; open -a Calculator; " + escapedQuote + ":8765"
        let expected = "tt-toplike-tui --remote '\(escapedCombined)'"
        let cmd = TTToplikeLauncher.command(host: maliciousHost, ctrlPort: 8765)
        XCTAssertEqual(cmd, expected)
        XCTAssertTrue(cmd.contains(escapedQuote), "expected the embedded quote to be escaped")
    }
    func testVSCodeRemoteArgs() {
        XCTAssertEqual(VSCodeLauncher.remoteArgs(user: "me", host: "qb.local", path: "/home/me"),
                       ["--remote", "ssh-remote+me@qb.local", "/home/me"])
        XCTAssertEqual(VSCodeLauncher.defaultRemotePath(user: "me"), "/home/me")
    }
    // Regression: the window-open args must NEVER carry `--install-extension`.
    // Combining install + `--remote <folder>` in one `code` invocation makes the
    // CLI run headless (install then exit 0) and never open a window — the toolkit
    // install has to be a SEPARATE `code` call (see `installExtensionArgs`).
    func testVSCodeRemoteArgsHasNoInstallFlag() {
        let args = VSCodeLauncher.remoteArgs(user: "u", host: "h", path: "/home/u")
        XCTAssertEqual(args, ["--remote", "ssh-remote+u@h", "/home/u"])
        XCTAssertFalse(args.contains("--install-extension"),
                       "remoteArgs must not combine install with window-open")
    }
    func testVSCodeInstallExtensionArgs() {
        XCTAssertEqual(VSCodeLauncher.installExtensionArgs(),
                       ["--install-extension", "Tenstorrent.tt-vscode-toolkit"])
    }
}

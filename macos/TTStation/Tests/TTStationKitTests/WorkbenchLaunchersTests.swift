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

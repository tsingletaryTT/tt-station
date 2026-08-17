import XCTest
@testable import TTStationKit

final class CLILinkPlannerTests: XCTestCase {
    let link = "/Users/x/.local/bin/tt-station"
    let bundled = "/App/TTStation.app/Contents/Resources/bin/tt-station"

    func testAbsentCreatesSymlink() {
        let action = CLILinkPlanner.plan(linkPath: link, bundledTT: bundled, state: .absent)
        XCTAssertEqual(action, .create(link: link, target: bundled))
    }

    func testOurStaleSymlinkGetsRepointed() {
        // A symlink pointing into some (possibly older) TTStation.app is ours.
        let action = CLILinkPlanner.plan(
            linkPath: link, bundledTT: bundled,
            state: .symlink(target: "/Applications/TTStation.app/Contents/Resources/bin/tt-station"))
        XCTAssertEqual(action, .repoint(link: link, target: bundled))
    }

    // A symlink at `~/.local/bin/tt-station` that points somewhere other than a
    // TTStation.app is not ours to touch. Unlike the old `~/.local/bin/tt` link
    // path — where a foreign entry was probably Tenstorrent's official `tt` CLI
    // — nothing else is expected to claim this name, so all we can do is report
    // what is in the way.
    func testForeignSymlinkIsLeftAlone() {
        let action = CLILinkPlanner.plan(
            linkPath: link, bundledTT: bundled,
            state: .symlink(target: "/some/other/tool/tt-station"))
        XCTAssertEqual(action, .foreign(existing: "/some/other/tool/tt-station"))
    }

    func testForeignRegularFileIsLeftAlone() {
        let action = CLILinkPlanner.plan(linkPath: link, bundledTT: bundled, state: .regularFile)
        XCTAssertEqual(action, .foreign(existing: link))
    }
}

import XCTest
@testable import TTStationKit

final class CLILinkPlannerTests: XCTestCase {
    let link = "/Users/x/.local/bin/tt"
    let bundled = "/App/TTStation.app/Contents/Resources/bin/tt"

    func testAbsentCreatesSymlink() {
        let action = CLILinkPlanner.plan(linkPath: link, bundledTT: bundled, state: .absent)
        XCTAssertEqual(action, .create(link: link, target: bundled))
    }

    func testOurStaleSymlinkGetsRepointed() {
        // A symlink pointing into some (possibly older) TTStation.app is ours.
        let action = CLILinkPlanner.plan(
            linkPath: link, bundledTT: bundled,
            state: .symlink(target: "/Applications/TTStation.app/Contents/Resources/bin/tt"))
        XCTAssertEqual(action, .repoint(link: link, target: bundled))
    }

    func testForeignSymlinkIsLeftAloneWithAlternative() {
        let action = CLILinkPlanner.plan(
            linkPath: link, bundledTT: bundled,
            state: .symlink(target: "/some/other/tool/tt"))
        XCTAssertEqual(action, .foreign(existing: "/some/other/tool/tt", alternative: "/Users/x/.local/bin/tt-station"))
    }

    func testForeignRegularFileIsLeftAloneWithAlternative() {
        let action = CLILinkPlanner.plan(linkPath: link, bundledTT: bundled, state: .regularFile)
        XCTAssertEqual(action, .foreign(existing: link, alternative: "/Users/x/.local/bin/tt-station"))
    }
}

import XCTest
@testable import TTStationKit

final class PowerControlsTests: XCTestCase {
    func testConfirmsAndMachineOpFlags() {
        XCTAssertFalse(PowerAction.resetChips.isMachineOp)
        XCTAssertFalse(PowerAction.resetChips.confirms)
        for a in [PowerAction.suspend, .reboot, .shutdown] {
            XCTAssertTrue(a.isMachineOp)
            XCTAssertTrue(a.confirms)
        }
    }
    func testIssuingMachineOpSetsMatchingState() {
        XCTAssertEqual(PowerTransition.next(issued: .reboot, reachable: true), .rebooting)
        XCTAssertEqual(PowerTransition.next(issued: .suspend, reachable: true), .suspending)
        XCTAssertEqual(PowerTransition.next(issued: .shutdown, reachable: true), .poweredOff)
        XCTAssertNil(PowerTransition.next(issued: .resetChips, reachable: true))
    }
    func testReachabilityClearsTransientButPoweredOffNeedsWake() {
        XCTAssertNil(PowerTransition.onReachabilityChange(.rebooting, reachable: true))
        XCTAssertEqual(PowerTransition.onReachabilityChange(.rebooting, reachable: false), .rebooting)
        // Powered-off box coming back (post-wake) clears; still-unreachable stays.
        XCTAssertNil(PowerTransition.onReachabilityChange(.poweredOff, reachable: true))
        XCTAssertEqual(PowerTransition.onReachabilityChange(.poweredOff, reachable: false), .poweredOff)
    }
}

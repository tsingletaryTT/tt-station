import XCTest
@testable import TTStationKit

final class ProvisioningTests: XCTestCase {
    func testBrewInstallArgs() {
        XCTAssertEqual(Provisioning.brewInstallArgs(formula: "uv"), ["install", "uv"])
        XCTAssertEqual(Provisioning.brewInstallArgs(formula: Provisioning.opencodeFormula),
                       ["install", "sst/tap/opencode"])
    }
}

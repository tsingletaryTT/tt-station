import XCTest
@testable import TTStationKit

final class TelemetrySnapshotTests: XCTestCase {
    func testDecodesCanonicalFrame() {
        let frame = #"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"61.4"}}]}"#
        let snap = TelemetrySnapshot.decode(frame)
        XCTAssertEqual(snap.devices.count, 1)
        XCTAssertEqual(snap.devices[0].index, 0)
        XCTAssertEqual(snap.devices[0].boardType, "p300c")
        XCTAssertEqual(snap.devices[0].tempC, 61.4)
    }

    func testTempMayBeNumericOrString() {
        let frame = #"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":60}}]}"#
        XCTAssertEqual(TelemetrySnapshot.decode(frame).devices.first?.tempC, 60)
    }

    func testMissingTelemetryYieldsNilTemp() {
        let frame = #"{"device_info":[{"board_info":{"board_type":"p300c"}}]}"#
        let snap = TelemetrySnapshot.decode(frame)
        XCTAssertEqual(snap.devices.count, 1)
        XCTAssertNil(snap.devices[0].tempC)
    }

    func testGarbageYieldsEmptySnapshot() {
        XCTAssertTrue(TelemetrySnapshot.decode("not json").devices.isEmpty)
        XCTAssertTrue(TelemetrySnapshot.decode(#"{"device_info":[]}"#).devices.isEmpty)
    }
}

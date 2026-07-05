import XCTest
@testable import TTStationKit

final class ModelRankingTests: XCTestCase {
    private func m(_ name: String, _ devices: [String]) -> ModelInfo {
        ModelInfo(name: name, devices: devices)
    }

    func testMeshMatchesIsCaseInsensitive() {
        XCTAssertTrue(ModelRanking.meshMatches(["P300X2", "T3K"], boxMesh: "p300x2"))
        XCTAssertFalse(ModelRanking.meshMatches(["T3K"], boxMesh: "p300x2"))
    }

    func testNilBoxMeshMatchesNothingButRankingKeepsAllCompatible() {
        // Unknown hardware: everything goes in the compatible tier (no split).
        let models = [m("Qwen3-8B", ["P300X2"]), m("Llama-3.1-70B", ["T3K"])]
        let ranked = ModelRanking.rankForHardware(models, boxMesh: nil)
        XCTAssertTrue(ranked.incompatible.isEmpty)
        XCTAssertEqual(ranked.compatible.flatMap { $0.models }.count, 2)
    }

    func testCompatibleTierExcludesIncompatibleModels() {
        let models = [m("Qwen3-8B", ["P300X2"]), m("Big-70B", ["T3K"])]
        let ranked = ModelRanking.rankForHardware(models, boxMesh: "p300x2")
        XCTAssertEqual(ranked.compatible.flatMap { $0.models }.map(\.name), ["Qwen3-8B"])
        XCTAssertEqual(ranked.incompatible.map(\.name), ["Big-70B"])
    }

    func testCompatibleTierIsFamilyGrouped() {
        let models = [m("Qwen3-8B", ["P300X2"]), m("Llama-3.1-8B-Instruct", ["P300X2"])]
        let ranked = ModelRanking.rankForHardware(models, boxMesh: "p300x2")
        XCTAssertEqual(ranked.compatible.map(\.family).sorted(), ["Llama", "Qwen"])
    }

    func testCompatibilityLabel() {
        XCTAssertEqual(
            ModelRanking.compatibilityLabel(for: m("Qwen3-8B", ["P300X2"]), boxMesh: "p300x2"),
            "Runs on P300X2")
        XCTAssertEqual(
            ModelRanking.compatibilityLabel(for: m("Big-70B", ["T3K"]), boxMesh: "p300x2"),
            "Needs T3K")
        XCTAssertEqual(
            ModelRanking.compatibilityLabel(for: m("Qwen3-8B", ["P300X2"]), boxMesh: nil),
            "")
    }
}

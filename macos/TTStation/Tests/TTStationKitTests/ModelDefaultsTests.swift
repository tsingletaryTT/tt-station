import XCTest
@testable import TTStationKit

final class ModelDefaultsTests: XCTestCase {

    private func m(_ name: String, _ devices: [String] = ["P300X2"]) -> ModelInfo {
        ModelInfo(name: name, devices: devices)
    }

    // MARK: pickDefaultModel

    func testEmptyReturnsNil() {
        XCTAssertNil(ModelDefaults.pickDefaultModel(from: [], lastUsed: nil))
        XCTAssertNil(ModelDefaults.pickDefaultModel(from: [], lastUsed: "Qwen/Qwen3-8B"))
    }

    func testLastUsedWinsWhenPresent() {
        let models = [m("Qwen/Qwen3-8B"), m("meta-llama/Llama-3.3-70B-Instruct")]
        XCTAssertEqual(
            ModelDefaults.pickDefaultModel(from: models, lastUsed: "Qwen/Qwen3-8B"),
            "Qwen/Qwen3-8B"
        )
    }

    func testLastUsedIgnoredWhenAbsent() {
        let models = [m("Qwen/Qwen3-8B"), m("Qwen/Qwen3-32B")]
        // Stale last-used no longer in the catalog → fall back to scoring.
        XCTAssertEqual(
            ModelDefaults.pickDefaultModel(from: models, lastUsed: "old/Gone-13B"),
            "Qwen/Qwen3-8B"
        )
    }

    func testInstructBeatsBaseSameSize() {
        let models = [m("Qwen/Qwen3-8B"), m("Qwen/Qwen2.5-7B-Instruct")]
        // Instruct outranks base even though base is a hair larger.
        XCTAssertEqual(
            ModelDefaults.pickDefaultModel(from: models, lastUsed: nil),
            "Qwen/Qwen2.5-7B-Instruct"
        )
    }

    func testSizeSweetSpotMidBeatsHugeBeatsTiny() {
        // Same tuning class (all instruct): 8B > 70B > 1B.
        let models = [
            m("meta-llama/Llama-3.1-8B-Instruct"),
            m("meta-llama/Llama-3.3-70B-Instruct"),
            m("meta-llama/Llama-3.2-1B-Instruct"),
        ]
        XCTAssertEqual(
            ModelDefaults.pickDefaultModel(from: models, lastUsed: nil),
            "meta-llama/Llama-3.1-8B-Instruct"
        )
        // Ordering check: 70B ranks above 1B.
        XCTAssertGreaterThan(
            ModelDefaults.score("Llama-3.3-70B-Instruct"),
            ModelDefaults.score("Llama-3.2-1B-Instruct")
        )
    }

    func testDeterministicTieBreakByName() {
        // Two identically-scored instruct 8B models → alphabetically first.
        let models = [m("Zed/Zed-8B-Instruct"), m("Ace/Ace-8B-Instruct")]
        XCTAssertEqual(
            ModelDefaults.pickDefaultModel(from: models, lastUsed: nil),
            "Ace/Ace-8B-Instruct"
        )
    }

    func testParamCountParsing() {
        XCTAssertEqual(ModelDefaults.paramCountB("Qwen/Qwen3-8B"), 8)
        XCTAssertEqual(ModelDefaults.paramCountB("meta-llama/Llama-3.3-70B-Instruct"), 70)
        XCTAssertEqual(ModelDefaults.paramCountB("Qwen/Qwen2.5-0.5B-Instruct"), 0.5)
        // "2.5" in Qwen2.5 is not a size (no trailing B); the "7B" is.
        XCTAssertEqual(ModelDefaults.paramCountB("Qwen/Qwen2.5-7B-Instruct"), 7)
        XCTAssertNil(ModelDefaults.paramCountB("some/mystery-model"))
    }

    func testPickDefaultPrefersCompatibleModel() {
        let models = [
            ModelInfo(name: "Llama-3.1-8B-Instruct", devices: ["T3K"]),   // higher score, wrong hw
            ModelInfo(name: "Qwen3-7B-Instruct", devices: ["P300X2"]),    // compatible
        ]
        let pick = ModelDefaults.pickDefaultModel(from: models, lastUsed: nil, boxMesh: "p300x2")
        XCTAssertEqual(pick, "Qwen3-7B-Instruct")
    }

    func testPickDefaultLastUsedWinsOnlyIfCompatible() {
        let models = [
            ModelInfo(name: "Qwen3-7B-Instruct", devices: ["P300X2"]),
            ModelInfo(name: "Old-Pick", devices: ["T3K"]),
        ]
        // Last-used is incompatible → fall back to best compatible.
        let pick = ModelDefaults.pickDefaultModel(from: models, lastUsed: "Old-Pick", boxMesh: "p300x2")
        XCTAssertEqual(pick, "Qwen3-7B-Instruct")
    }

    func testPickDefaultFallsBackToGlobalWhenNoneCompatible() {
        let models = [ModelInfo(name: "Llama-3.1-8B-Instruct", devices: ["T3K"])]
        let pick = ModelDefaults.pickDefaultModel(from: models, lastUsed: nil, boxMesh: "p300x2")
        XCTAssertEqual(pick, "Llama-3.1-8B-Instruct")
    }

    // MARK: groupModelsByFamily

    func testFamilyName() {
        XCTAssertEqual(ModelDefaults.familyName(for: "Qwen/Qwen3-32B"), "Qwen")
        XCTAssertEqual(ModelDefaults.familyName(for: "meta-llama/Llama-3.1-8B-Instruct"), "Llama")
        XCTAssertEqual(ModelDefaults.familyName(for: "Qwen2.5-7B-Instruct"), "Qwen")
        XCTAssertEqual(ModelDefaults.familyName(for: "mistralai/Mistral-7B-Instruct-v0.2"), "Mistral")
    }

    func testGroupingAndSorting() {
        let models = [
            m("Qwen/Qwen3-32B"),
            m("meta-llama/Llama-3.1-8B-Instruct"),
            m("Qwen/Qwen2.5-7B-Instruct"),
            m("meta-llama/Llama-3.3-70B-Instruct"),
        ]
        let groups = ModelDefaults.groupModelsByFamily(models)
        // Two families, sorted case-insensitively: Llama, then Qwen.
        XCTAssertEqual(groups.map(\.family), ["Llama", "Qwen"])
        // Models within each family sorted by name.
        XCTAssertEqual(
            groups[0].models.map(\.name),
            ["meta-llama/Llama-3.1-8B-Instruct", "meta-llama/Llama-3.3-70B-Instruct"]
        )
        XCTAssertEqual(
            groups[1].models.map(\.name),
            ["Qwen/Qwen2.5-7B-Instruct", "Qwen/Qwen3-32B"]
        )
    }

    func testGroupingEmpty() {
        XCTAssertTrue(ModelDefaults.groupModelsByFamily([]).isEmpty)
    }

    // MARK: HostRegistry last-model round-trip

    func testLastModelRoundTrip() {
        let reg = HostRegistry(store: InMemoryStore())
        XCTAssertNil(reg.lastModel(forHost: "h:8080"))
        reg.setLastModel("Qwen/Qwen3-8B", forHost: "h:8080")
        XCTAssertEqual(reg.lastModel(forHost: "h:8080"), "Qwen/Qwen3-8B")
        // Per-host isolation: a different host is unaffected.
        XCTAssertNil(reg.lastModel(forHost: "other:8080"))
        // Overwrite keeps only the latest.
        reg.setLastModel("Qwen/Qwen3-32B", forHost: "h:8080")
        XCTAssertEqual(reg.lastModel(forHost: "h:8080"), "Qwen/Qwen3-32B")
    }
}

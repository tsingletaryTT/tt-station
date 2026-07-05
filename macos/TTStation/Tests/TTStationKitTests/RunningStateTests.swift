import XCTest
@testable import TTStationKit

final class RunningStateTests: XCTestCase {
    private func entry(_ model: String, source: String) -> ServingEntry {
        ServingEntry(model: model, baseURL: "http://x:8000/v1", hostPort: 8000, container: "c", source: source)
    }

    func testStartingTakesPrecedenceOverServingEntries() {
        let state = RunningState.runningState(
            serving: [entry("Qwen3-8B", source: "agent")],
            status: .serving(model: "Qwen3-8B"),
            starting: true
        )
        XCTAssertEqual(state, .starting)
    }

    func testStartingTakesPrecedenceEvenWithNoStatusYet() {
        let state = RunningState.runningState(serving: [], status: nil, starting: true)
        XCTAssertEqual(state, .starting)
    }

    func testAgentEntriesOrderedBeforeExternalAndDeduped() {
        let state = RunningState.runningState(
            serving: [
                entry("tt-studio-model", source: "external"),
                entry("Qwen3-8B", source: "agent"),
                entry("Qwen3-8B", source: "agent"), // duplicate, should collapse
                entry("Llama-3-70B", source: "external"),
            ],
            status: nil,
            starting: false
        )
        // Order should be: agent entries first (Qwen3-8B, deduped), then
        // external entries in their original relative order.
        XCTAssertEqual(state, .serving(primary: "Qwen3-8B", others: 2))
    }

    func testServingEmptyFallsBackToStatusServing() {
        let state = RunningState.runningState(
            serving: [],
            status: .serving(model: "Qwen3-8B"),
            starting: false
        )
        XCTAssertEqual(state, .serving(primary: "Qwen3-8B", others: 0))
    }

    func testServingEmptyAndStatusIdleIsIdle() {
        let state = RunningState.runningState(serving: [], status: .idle, starting: false)
        XCTAssertEqual(state, .idle)
    }

    func testServingEmptyAndStatusNilIsIdle() {
        let state = RunningState.runningState(serving: [], status: nil, starting: false)
        XCTAssertEqual(state, .idle)
    }

    func testMultipleModelsReportsOthersCount() {
        let state = RunningState.runningState(
            serving: [
                entry("Qwen3-8B", source: "agent"),
                entry("Llama-3-70B", source: "external"),
                entry("Mixtral-8x7B", source: "external"),
            ],
            status: nil,
            starting: false
        )
        XCTAssertEqual(state, .serving(primary: "Qwen3-8B", others: 2))
    }

    func testDedupPreservesFirstOccurrenceOrderAcrossSources() {
        let state = RunningState.runningState(
            serving: [
                entry("A", source: "external"),
                entry("B", source: "agent"),
                entry("A", source: "agent"), // duplicate of the external "A"
            ],
            status: nil,
            starting: false
        )
        // Agent entries come first: "B" then "A" (agent's own "A"), the
        // external "A" is a dup and collapses into the earlier occurrence's
        // position — agent-first ordering wins, so primary is "B".
        XCTAssertEqual(state, .serving(primary: "B", others: 1))
    }
}

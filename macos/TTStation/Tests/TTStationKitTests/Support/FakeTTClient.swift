import Foundation
@testable import TTStationKit

final class FakeTTClient: TTCommands {
    var models_ = [ModelInfo(name: "Qwen3-8B", devices: ["P300X2"])]
    var serving_: [ServingEntry] = []
    var servingError: TTError?
    var statusResult: ServingStatus = .idle
    var statusError: TTError?
    var pairShouldSucceed = true
    var runEndpoint = Endpoint(baseURL: "http://h:8000/v1", model: "Qwen3-8B", requiresKey: false)
    var runError: TTError?
    var statusCalled = false
    var statusCallCount = 0
    var pairInitResult = "mock-pair-id"
    var pairInitError: TTError?
    var pairInitCalled = false
    var pairCompleteSucceeds = true
    var pairCompleteError: TTError?
    var pairCompleteCalled = false

    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord] { [] }
    func models(host: String) async throws -> [ModelInfo] { models_ }
    func status(host: String) async throws -> ServingStatus {
        statusCalled = true
        statusCallCount += 1
        if let statusError { throw statusError }
        return statusResult
    }
    func endpoint(host: String) async throws -> Endpoint { runEndpoint }
    func serving(host: String) async throws -> [ServingEntry] {
        if let servingError { throw servingError }
        return serving_
    }
    func pair(host: String, code: String) async throws -> PairResult {
        if pairShouldSucceed { return PairResult(host: host, paired: true) }
        throw TTError.commandFailed(command: [], exitCode: 1, stderr: "invalid code")
    }
    func pairInit(host: String) async throws -> PairInitResult {
        pairInitCalled = true
        if let pairInitError { throw pairInitError }
        return PairInitResult(pairId: pairInitResult)
    }
    func pairComplete(host: String, pairId: String, code: String) async throws -> PairResult {
        pairCompleteCalled = true
        if let pairCompleteError { throw pairCompleteError }
        if pairCompleteSucceeds { return PairResult(host: host, paired: true) }
        throw TTError.commandFailed(command: [], exitCode: 1, stderr: "invalid code")
    }
    func run(host: String, model: String) async throws -> Endpoint {
        if let runError { throw runError }
        return runEndpoint
    }
    func stop(host: String) async throws {}
    func isAuthError(_ error: TTError) -> Bool {
        if case let .commandFailed(_, _, s) = error { return s.lowercased().contains("no token") }
        return false
    }
}

import Foundation
@testable import TTStationKit

final class FakeTTClient: TTCommands {
    var models_ = [ModelInfo(name: "Qwen3-8B", devices: ["P300X2"])]
    var statusResult: ServingStatus = .idle
    var pairShouldSucceed = true
    var runEndpoint = Endpoint(baseURL: "http://h:8000/v1", model: "Qwen3-8B", requiresKey: false)
    var runError: TTError?

    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord] { [] }
    func models(host: String) async throws -> [ModelInfo] { models_ }
    func status(host: String) async throws -> ServingStatus { statusResult }
    func endpoint(host: String) async throws -> Endpoint { runEndpoint }
    func pair(host: String, code: String) async throws -> PairResult {
        if pairShouldSucceed { return PairResult(host: host, paired: true) }
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

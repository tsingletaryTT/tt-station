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
    /// Set to make `endpoint(host:)` throw instead of returning `runEndpoint`
    /// -- the hook `BoxViewModelTests` uses to drive the authed pairing probe
    /// in `refresh()` through its 401 (unpaired) / 409 (idle) / success
    /// branches.
    var endpointError: TTError?
    var statusCalled = false
    var statusCallCount = 0
    var pairInitResult = "mock-pair-id"
    var pairInitError: TTError?
    var pairInitCalled = false
    var pairCompleteSucceeds = true
    var pairCompleteError: TTError?
    var pairCompleteCalled = false
    var sshAuthorizeResult = SshAuthorizeInfo(authorized: true, sshUser: "ttuser", alreadyPresent: false)
    var sshAuthorizeError: TTError?
    var sshAuthorizeCalled = false
    var configResult = BoxConfig(
        activeProfile: "stable",
        availableProfiles: ["stable", "bleeding"],
        backend: "runpy",
        servingHost: "127.0.0.1",
        servingPort: 8000,
        servingImage: nil,
        ttInferenceRepo: nil,
        ttDevice: "p300x2")
    var configError: TTError?
    var catalogResult = BoxCatalog(
        boxMesh: "p300x2",
        catalogAvailable: true,
        catalogStale: false,
        runsHere: [
            CatalogEntry(
                id: "qwen3-8b", displayName: "Qwen3-8B", family: "Qwen3", size: "8B",
                software: ["vllm"], meshes: ["p300x2"], neededHardware: [],
                availableNow: true, statusHere: "supported")
        ],
        experimental: [
            CatalogEntry(
                id: "llama-3.1-70b", displayName: "Llama-3.1-70B", family: "Llama", size: "70B",
                software: ["vllm"], meshes: ["t3k"], neededHardware: [],
                availableNow: false, statusHere: "experimental")
        ],
        otherHardware: [
            CatalogEntry(
                id: "llama-3.1-405b", displayName: "Llama-3.1-405B", family: "Llama", size: "405B",
                software: ["vllm"], meshes: ["t3k"], neededHardware: ["T3K"],
                availableNow: false, statusHere: "needs_other_hardware")
        ])
    var catalogError: TTError?

    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord] { [] }
    func models(host: String) async throws -> [ModelInfo] { models_ }
    func status(host: String) async throws -> ServingStatus {
        statusCalled = true
        statusCallCount += 1
        if let statusError { throw statusError }
        return statusResult
    }
    func endpoint(host: String) async throws -> Endpoint {
        if let endpointError { throw endpointError }
        return runEndpoint
    }
    func serving(host: String) async throws -> [ServingEntry] {
        if let servingError { throw servingError }
        return serving_
    }
    func config(host: String) async throws -> BoxConfig {
        if let configError { throw configError }
        return configResult
    }
    func catalog(host: String) async throws -> BoxCatalog {
        if let catalogError { throw catalogError }
        return catalogResult
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
    func sshAuthorize(host: String) async throws -> SshAuthorizeInfo {
        sshAuthorizeCalled = true
        if let sshAuthorizeError { throw sshAuthorizeError }
        return sshAuthorizeResult
    }
    func isAuthError(_ error: TTError) -> Bool {
        // Mirrors `TTClient.isAuthError` exactly (widened from the original
        // "no token"-only check) so fake-driven tests exercise the same
        // matching the real client uses in production.
        if case let .commandFailed(_, _, s) = error {
            let low = s.lowercased()
            return low.contains("no token") || low.contains("unauthorized") || low.contains("401")
        }
        return false
    }
    func isIdleConflict(_ error: TTError) -> Bool {
        // Mirrors `TTClient.isIdleConflict`.
        if case let .commandFailed(_, _, s) = error {
            let low = s.lowercased()
            return low.contains("409") || low.contains("no model is currently serving")
        }
        return false
    }
}

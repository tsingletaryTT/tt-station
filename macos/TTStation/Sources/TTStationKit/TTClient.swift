import Foundation

/// Protocol so view-models can be tested against a fake.
public protocol TTCommands {
    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord]
    func models(host: String) async throws -> [ModelInfo]
    func status(host: String) async throws -> ServingStatus
    func endpoint(host: String) async throws -> Endpoint
    func pair(host: String, code: String) async throws -> PairResult
    func run(host: String, model: String) async throws -> Endpoint
    func stop(host: String) async throws
    func isAuthError(_ error: TTError) -> Bool
}

/// Typed façade over `tt --json`. One method per subcommand; the only place
/// argv is assembled and stdout is decoded.
public final class TTClient {
    private let runner: TTProcessRunner
    public init(runner: TTProcessRunner) { self.runner = runner }

    // MARK: Read commands

    public func discover(manualHosts: [String] = [], noMdns: Bool = false) async throws -> [BoxRecord] {
        var args = ["--json", "discover"]
        for h in manualHosts { args += ["--host", h] }
        if noMdns { args.append("--no-mdns") }
        return try await call(args, decode: [BoxRecord].self)
    }

    public func models(host: String) async throws -> [ModelInfo] {
        let resp = try await call(["--json", "models", "--host", host], decode: ModelsResponse.self)
        return resp.models
    }

    public func status(host: String) async throws -> ServingStatus {
        let resp = try await call(["--json", "status", "--host", host], decode: StatusResponse.self)
        do { return try ServingStatus(raw: resp.status) }
        catch { throw TTError.decodeFailed(command: ["--json", "status", "--host", host], detail: "bad status: \(resp.status)") }
    }

    public func endpoint(host: String) async throws -> Endpoint {
        try await call(["--json", "endpoint", "--host", host], decode: Endpoint.self)
    }

    // MARK: Helpers

    func call<T: Decodable>(_ args: [String], decode type: T.Type) async throws -> T {
        let result = try await runner.run(args)
        guard result.exitCode == 0 else {
            throw TTError.commandFailed(command: args, exitCode: result.exitCode, stderr: result.stderr.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        do { return try JSONDecoder().decode(T.self, from: result.stdout) }
        catch { throw TTError.decodeFailed(command: args, detail: String(describing: error)) }
    }
}

extension TTClient {
    // MARK: Action commands

    public func pair(host: String, code: String) async throws -> PairResult {
        try await call(["--json", "pair", host, "--code", code], decode: PairResult.self)
    }

    public func run(host: String, model: String) async throws -> Endpoint {
        try await call(["--json", "run", model, "--host", host], decode: Endpoint.self)
    }

    public func stop(host: String) async throws {
        let args = ["--json", "stop", "--host", host]
        let result = try await runner.run(args)
        guard result.exitCode == 0 else {
            throw TTError.commandFailed(command: args, exitCode: result.exitCode, stderr: result.stderr.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    public func isAuthError(_ error: TTError) -> Bool {
        if case let .commandFailed(_, _, stderr) = error {
            let s = stderr.lowercased()
            return s.contains("no token") || s.contains("unauthorized") || s.contains("401")
        }
        return false
    }
}

extension TTClient: TTCommands {}

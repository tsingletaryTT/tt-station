import Foundation

/// Protocol so view-models can be tested against a fake.
public protocol TTCommands {
    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord]
    func models(host: String) async throws -> [ModelInfo]
    func status(host: String) async throws -> ServingStatus
    func endpoint(host: String) async throws -> Endpoint
    func serving(host: String) async throws -> [ServingEntry]
    func config(host: String) async throws -> BoxConfig
    func catalog(host: String) async throws -> BoxCatalog
    func pair(host: String, code: String) async throws -> PairResult
    func pairInit(host: String) async throws -> PairInitResult
    func pairComplete(host: String, pairId: String, code: String) async throws -> PairResult
    func run(host: String, model: String) async throws -> Endpoint
    func stop(host: String) async throws
    func sshAuthorize(host: String) async throws -> SshAuthorizeInfo
    func isAuthError(_ error: TTError) -> Bool
    func isIdleConflict(_ error: TTError) -> Bool
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
        // mDNS discovery can legitimately take longer than a typical control
        // call while it waits out its own scan window.
        return try await call(args, decode: [BoxRecord].self, timeout: 25)
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

    /// Every currently-serving `/v1` endpoint on the box — including containers
    /// this box's agent did *not* launch (e.g. tt-studio), flagged
    /// `source == "external"`. Unauthed, like `models`/`status`.
    public func serving(host: String) async throws -> [ServingEntry] {
        try await call(["--json", "serving", "--host", host], decode: ServingList.self).serving
    }

    /// The box's resolved serving configuration. Unauthed, like `models`/
    /// `status`/`serving`, so it can be fetched regardless of pairing.
    public func config(host: String) async throws -> BoxConfig {
        try await call(["--json", "config", "--host", host], decode: BoxConfig.self)
    }

    /// The box's curated model catalog (runs-here / experimental /
    /// other-hardware tiers). Unauthed, like `models`/`status`/`serving`/
    /// `config`, so it can be fetched regardless of pairing.
    public func catalog(host: String) async throws -> BoxCatalog {
        try await call(["--json", "catalog", "--host", host], decode: BoxCatalog.self)
    }

    // MARK: Helpers

    /// `timeout` defaults to 20s, generous for a local control-plane
    /// round-trip but short enough that a hung box (e.g. serving backend
    /// down) fails the UI action instead of spinning it forever. `run(...)`
    /// overrides it — model loads are slow — and `discover` overrides it too.
    func call<T: Decodable>(_ args: [String], decode type: T.Type, timeout: TimeInterval = 20) async throws -> T {
        let result = try await runner.run(args, timeout: timeout)
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

    public func pairInit(host: String) async throws -> PairInitResult {
        try await call(["--json", "pair-init", host], decode: PairInitResult.self)
    }

    public func pairComplete(host: String, pairId: String, code: String) async throws -> PairResult {
        try await call(["--json", "pair-complete", host, "--pair-id", pairId, "--code", code], decode: PairResult.self)
    }

    public func run(host: String, model: String) async throws -> Endpoint {
        // Model loads can be slow (large weights, cold cache) — give this
        // one a long leash instead of the default 20s.
        try await call(["--json", "run", model, "--host", host], decode: Endpoint.self, timeout: 600)
    }

    public func stop(host: String) async throws {
        let args = ["--json", "stop", "--host", host]
        let result = try await runner.run(args, timeout: 20)
        guard result.exitCode == 0 else {
            throw TTError.commandFailed(command: args, exitCode: result.exitCode, stderr: result.stderr.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    /// Installs this Mac's SSH public key on the box as `ttuser`, so
    /// Terminal/tt-toplike/VS Code work keylessly right after pairing. Called
    /// as an opt-in, non-fatal follow-up to a successful pair — see
    /// `BoxViewModel.completePairing`.
    public func sshAuthorize(host: String) async throws -> SshAuthorizeInfo {
        try await call(["--json", "ssh-authorize", "--host", host], decode: SshAuthorizeInfo.self)
    }

    public func isAuthError(_ error: TTError) -> Bool {
        if case let .commandFailed(_, _, stderr) = error {
            let s = stderr.lowercased()
            return s.contains("no token") || s.contains("unauthorized") || s.contains("401")
        }
        return false
    }

    /// True when `error` is the agent's `409` ("authed fine, nothing is
    /// currently serving") rather than an auth failure -- see
    /// `libttstation::agent_client::endpoint`'s idle-bail message, which
    /// carries a stable `"(409)"` marker for exactly this purpose. Matches on
    /// two independent signals (the numeric code and the human-readable
    /// phrase) so a rewording of either one alone can't silently break this.
    public func isIdleConflict(_ error: TTError) -> Bool {
        if case let .commandFailed(_, _, stderr) = error {
            let s = stderr.lowercased()
            return s.contains("409") || s.contains("no model is currently serving")
        }
        return false
    }
}

extension TTClient: TTCommands {}

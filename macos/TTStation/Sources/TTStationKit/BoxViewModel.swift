import Foundation
import Observation

@Observable @MainActor
public final class BoxViewModel: Identifiable {
    // `record` and `id` are `nonisolated`: `Identifiable.id` is a nonisolated
    // protocol requirement, and `BoxRecord` is an immutable, Sendable-safe
    // value type, so reading it off the main actor is sound. Without this,
    // the compiler treats the whole `Identifiable` conformance as crossing
    // into main-actor-isolated code — a warning today, a hard error in the
    // Swift 6 language mode.
    nonisolated public let record: BoxRecord
    nonisolated public var id: String { record.hostPort }

    public var status: ServingStatus?
    public var endpoint: Endpoint?
    public var models: [ModelInfo] = []
    public var selectedModel: String?
    public var isPaired: Bool
    public var inFlight = false
    public var errorText: String?

    private let commands: TTCommands
    private let registry: HostRegistry

    public init(record: BoxRecord, commands: TTCommands, registry: HostRegistry) {
        self.record = record
        self.commands = commands
        self.registry = registry
        self.isPaired = registry.pairedHosts.contains(record.hostPort)
    }

    public func refresh() async {
        do {
            status = try await commands.status(host: record.hostPort)
            if isPaired { await loadModels() }
        } catch { record(error) }
    }

    public func loadModels() async {
        do {
            models = try await commands.models(host: record.hostPort)
            if selectedModel == nil { selectedModel = models.first?.name }
        } catch { record(error) }
    }

    public func pair(code: String) async {
        inFlight = true; defer { inFlight = false }
        do {
            _ = try await commands.pair(host: record.hostPort, code: code)
            isPaired = true
            registry.markPaired(record.hostPort)
            errorText = nil
            await loadModels()
        } catch { record(error) }
    }

    public func run() async {
        guard let model = selectedModel else { errorText = "Pick a model first."; return }
        inFlight = true; defer { inFlight = false }
        do {
            endpoint = try await commands.run(host: record.hostPort, model: model)
            status = .serving(model: model)
            errorText = nil
        } catch { record(error) }
    }

    public func stop() async {
        inFlight = true; defer { inFlight = false }
        do {
            try await commands.stop(host: record.hostPort)
            endpoint = nil
            status = .idle
            errorText = nil
        } catch { record(error) }
    }

    private func record(_ error: Error) {
        if let tt = error as? TTError {
            if commands.isAuthError(tt) {
                isPaired = false
                registry.markUnpaired(record.hostPort)
            }
            if case let .commandFailed(_, _, stderr) = tt { errorText = stderr.isEmpty ? "Command failed." : stderr }
            else { errorText = String(describing: tt) }
        } else {
            errorText = error.localizedDescription
        }
    }
}

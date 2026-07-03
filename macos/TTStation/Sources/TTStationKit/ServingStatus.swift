import Foundation

/// Mirror of the CLI's `ServingStatus` wire form (`idle` / `serving:<model>`).
public enum ServingStatus: Equatable {
    case idle
    case serving(model: String)

    public struct ParseError: Error, Equatable { public let raw: String }

    public init(raw: String) throws {
        if raw == "idle" {
            self = .idle
        } else if raw.hasPrefix("serving:") {
            self = .serving(model: String(raw.dropFirst("serving:".count)))
        } else {
            throw ParseError(raw: raw)
        }
    }

    public var isServing: Bool {
        if case .serving = self { return true }
        return false
    }
}

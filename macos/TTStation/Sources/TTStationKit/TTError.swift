import Foundation

/// Every failure the app surfaces. `commandFailed` carries the CLI's stderr
/// verbatim so the UI can show it (README: surface stderr, don't swallow it).
public enum TTError: Error, Equatable {
    case commandFailed(command: [String], exitCode: Int32, stderr: String)
    case binaryNotFound(triedPaths: [String])
    case decodeFailed(command: [String], detail: String)
}

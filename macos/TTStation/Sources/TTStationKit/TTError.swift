import Foundation

/// Every failure the app surfaces. `commandFailed` carries the CLI's stderr
/// verbatim so the UI can show it (README: surface stderr, don't swallow it).
public enum TTError: Error, Equatable {
    case commandFailed(command: [String], exitCode: Int32, stderr: String)
    case binaryNotFound(triedPaths: [String])
    case decodeFailed(command: [String], detail: String)
    /// A subprocess invocation of `tt` didn't exit within its allotted
    /// `seconds`; the process was terminated. Distinct from `commandFailed`
    /// so callers (e.g. `BoxViewModel`) can tell "the box didn't answer" from
    /// "the box answered with an error" — and, in particular, not treat it as
    /// an auth failure.
    case timedOut(command: [String], seconds: Double)
}

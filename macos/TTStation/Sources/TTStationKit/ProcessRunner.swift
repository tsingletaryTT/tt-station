import Foundation

public struct ProcessResult: Equatable {
    public let stdout: Data
    public let stderr: String
    public let exitCode: Int32
    public init(stdout: Data, stderr: String, exitCode: Int32) {
        self.stdout = stdout; self.stderr = stderr; self.exitCode = exitCode
    }
}

/// The only abstraction that runs `tt`. Real impl added in Task 6.
public protocol TTProcessRunner {
    func run(_ args: [String]) async throws -> ProcessResult
}

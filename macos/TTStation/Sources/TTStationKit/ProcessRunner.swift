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

/// Spawns the real `tt` binary. The only type in the package that touches
/// `Process` or the filesystem.
public final class RealProcessRunner: TTProcessRunner {
    private let locator: TTBinaryLocator
    public init(locator: TTBinaryLocator) { self.locator = locator }

    public func run(_ args: [String]) async throws -> ProcessResult {
        let path = try locator.locate()
        return try await withCheckedThrowingContinuation { continuation in
            let process = Process()
            process.executableURL = URL(fileURLWithPath: path)
            process.arguments = args
            let outPipe = Pipe(), errPipe = Pipe()
            process.standardOutput = outPipe
            process.standardError = errPipe
            process.terminationHandler = { proc in
                let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
                let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
                continuation.resume(returning: ProcessResult(
                    stdout: outData,
                    stderr: String(data: errData, encoding: .utf8) ?? "",
                    exitCode: proc.terminationStatus
                ))
            }
            do { try process.run() }
            catch { continuation.resume(throwing: error) }
        }
    }
}

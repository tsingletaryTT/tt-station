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
///
/// The timeout is a protocol requirement (not a default-parameterized method)
/// so every conformer — real and fake — has to at least accept one. The
/// `run(_:)` convenience overload below is what keeps pre-existing call
/// sites and tests compiling unchanged.
public protocol TTProcessRunner {
    func run(_ args: [String], timeout: TimeInterval) async throws -> ProcessResult
}

extension TTProcessRunner {
    /// Convenience overload for callers that don't care to pick a timeout.
    /// 30s is a generic "shouldn't normally take this long" ceiling; callers
    /// that know better (e.g. `TTClient`) pass an explicit value instead.
    public func run(_ args: [String]) async throws -> ProcessResult { try await run(args, timeout: 30) }
}

/// Spawns the real `tt` binary. The only type in the package that touches
/// `Process` or the filesystem.
public final class RealProcessRunner: TTProcessRunner {
    private let locator: TTBinaryLocator
    public init(locator: TTBinaryLocator) { self.locator = locator }

    public func run(_ args: [String], timeout: TimeInterval) async throws -> ProcessResult {
        let path = try locator.locate()
        return try await withCheckedThrowingContinuation { continuation in
            let process = Process()
            process.executableURL = URL(fileURLWithPath: path)
            process.arguments = args
            let outPipe = Pipe(), errPipe = Pipe()
            process.standardOutput = outPipe
            process.standardError = errPipe

            // Exactly one of {terminationHandler, the timeout block} may
            // resume `continuation` — resuming a `CheckedContinuation` twice
            // is a fatal error. `resumed` + `lock` make the check-and-set
            // atomic across the two independent callback paths (termination
            // fires on a Process-internal queue; the timeout fires on
            // DispatchQueue.global()).
            let lock = NSLock()
            var resumed = false

            process.terminationHandler = { proc in
                lock.lock()
                guard !resumed else { lock.unlock(); return }
                resumed = true
                lock.unlock()
                let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
                let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
                continuation.resume(returning: ProcessResult(
                    stdout: outData,
                    stderr: String(data: errData, encoding: .utf8) ?? "",
                    exitCode: proc.terminationStatus
                ))
            }

            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) {
                lock.lock()
                guard !resumed else { lock.unlock(); return }
                resumed = true
                lock.unlock()
                process.terminate()
                continuation.resume(throwing: TTError.timedOut(command: args, seconds: timeout))
            }

            do { try process.run() }
            catch {
                lock.lock()
                guard !resumed else { lock.unlock(); return }
                resumed = true
                lock.unlock()
                continuation.resume(throwing: error)
            }
        }
    }
}

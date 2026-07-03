import Foundation
@testable import TTStationKit

final class FakeProcessRunner: TTProcessRunner {
    var nextResult: ProcessResult?
    var nextError: Error?
    private(set) var lastArgs: [String] = []
    private(set) var callCount = 0

    private(set) var lastTimeout: TimeInterval?

    func run(_ args: [String], timeout: TimeInterval) async throws -> ProcessResult {
        lastArgs = args
        lastTimeout = timeout
        callCount += 1
        if let nextError { throw nextError }
        return nextResult ?? ProcessResult(stdout: Data(), stderr: "", exitCode: 0)
    }
}

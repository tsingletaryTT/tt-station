import Foundation
@testable import TTStationKit

final class FakeProcessRunner: TTProcessRunner {
    var nextResult: ProcessResult?
    var nextError: Error?
    private(set) var lastArgs: [String] = []
    private(set) var callCount = 0

    func run(_ args: [String]) async throws -> ProcessResult {
        lastArgs = args
        callCount += 1
        if let nextError { throw nextError }
        return nextResult ?? ProcessResult(stdout: Data(), stderr: "", exitCode: 0)
    }
}

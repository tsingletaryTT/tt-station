import XCTest
@testable import TTStationKit

final class ProcessRunnerTests: XCTestCase {
    func testFakeRecordsArgsAndReturnsResult() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data("[]".utf8), stderr: "", exitCode: 0)
        let result = try await fake.run(["--json", "discover"])
        XCTAssertEqual(fake.lastArgs, ["--json", "discover"])
        XCTAssertEqual(result.exitCode, 0)
        XCTAssertEqual(String(data: result.stdout, encoding: .utf8), "[]")
    }

    func testTTErrorCarriesStderr() {
        let err = TTError.commandFailed(command: ["--json", "run", "x"], exitCode: 2, stderr: "boom")
        if case let .commandFailed(_, code, stderr) = err {
            XCTAssertEqual(code, 2)
            XCTAssertEqual(stderr, "boom")
        } else {
            XCTFail("wrong case")
        }
    }
}

extension ProcessRunnerTests {
    // Uses /bin/echo as a deterministic stand-in for `tt` to prove spawn/capture.
    func testRealRunnerCapturesStdoutAndExit() async throws {
        let locator = TTBinaryLocator(override: "/bin/echo", candidates: []) { _ in true }
        let runner = RealProcessRunner(locator: locator)
        let result = try await runner.run(["hello"])
        XCTAssertEqual(result.exitCode, 0)
        XCTAssertEqual(String(data: result.stdout, encoding: .utf8), "hello\n")
    }

    func testRealRunnerThrowsWhenBinaryMissing() async {
        let locator = TTBinaryLocator(override: nil, candidates: ["/nope/tt"]) { _ in false }
        let runner = RealProcessRunner(locator: locator)
        do {
            _ = try await runner.run(["--json", "discover"])
            XCTFail("expected throw")
        } catch {
            XCTAssertEqual(error as? TTError, .binaryNotFound(triedPaths: ["/nope/tt"]))
        }
    }

    func testRealRunnerTimesOutAndThrows() async throws {
        let locator = TTBinaryLocator(override: "/bin/sleep", candidates: []) { _ in true }
        let runner = RealProcessRunner(locator: locator)
        let start = Date()
        do {
            _ = try await runner.run(["5"], timeout: 0.5)
            XCTFail("expected timeout throw")
        } catch {
            XCTAssertEqual(error as? TTError, .timedOut(command: ["5"], seconds: 0.5))
        }
        // Should resolve well before the sleep's own 5s duration would elapse.
        XCTAssertLessThan(Date().timeIntervalSince(start), 3.0)
    }
}

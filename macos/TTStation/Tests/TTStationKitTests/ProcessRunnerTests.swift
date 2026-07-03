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

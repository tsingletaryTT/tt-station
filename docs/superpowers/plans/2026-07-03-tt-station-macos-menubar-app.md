# TTStation macOS Menu-Bar App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `TTStation`, a native SwiftUI `MenuBarExtra` app that veneers the `tt` CLI so a user can discover a QuietBox, pair, run/stop a model, and copy its OpenAI endpoint — entirely from the menu bar.

**Architecture:** Approach 3 (MVVM, five layers). Layers 1–4 (process runner, `tt` client, domain models, discovery + view-models) live in a **Swift package `TTStationKit`** that is unit-tested via `swift test` with no Xcode UI. Layer 5 (SwiftUI views + the `MenuBarExtra` app entry) is a **thin XcodeGen-generated app target** that imports the package. All logic shells out to `tt --json`; no discovery/pairing/HTTP is reimplemented in Swift.

**Tech Stack:** Swift 5 language mode, SwiftUI + Observation (`@Observable`), Foundation `Process`, SwiftPM for the library + tests, XcodeGen + `xcodebuild` for the app target, `mock-box` (existing Rust dev fixture) as the pre-hardware end-to-end target.

## Global Constraints

- Deployment target: **macOS 14.0** (both package `platforms` and app `deploymentTarget`).
- Language mode: **Swift 5** (not Swift 6 strict concurrency).
- The app is an **agent app**: Info.plist `LSUIElement = true`, no Dock icon.
- `MenuBarExtra` uses **`.menuBarExtraStyle(.window)`** (popover panel, not `NSMenu`).
- The app **never** touches the Keychain, spawns HTTP, or parses mDNS itself — only `tt --json`.
- Every `tt` invocation puts the global flag first: `["--json", <subcommand>, …]`.
- CLI JSON wire shapes (ground truth: `crates/tt/src/main.rs`, `crates/libttstation/src/model.rs`):
  - discover → `[{ "name","host","ctrl_port","chips","status","apiver" }]`, `status` string is `"idle"` or `"serving:<model>"`.
  - status → `{ "status": "idle" | "serving:<model>" }`.
  - endpoint / run → `{ "base_url","model","requires_key" }`.
  - models → `{ "release_version": <string|null>, "models": [ { "name","devices":[…] } ] }`.
  - pair → `{ "host","paired":true,"token":"…" }` (token ignored; CLI stored it in Keychain).
- Root of all app code: `macos/TTStation/`.

---

## File Structure

```
macos/TTStation/
  Package.swift                              # library TTStationKit + test target (macOS 14)
  Sources/TTStationKit/
    ServingStatus.swift                      # status enum + parse
    Models.swift                             # BoxRecord, Endpoint, ModelInfo, ModelsResponse, PairResult
    TTError.swift                            # typed error carrying stderr
    ProcessRunner.swift                      # TTProcessRunner protocol + RealProcessRunner
    BinaryLocator.swift                      # TTBinaryLocator
    TTClient.swift                           # one async method per CLI command
    HostRegistry.swift                       # manual-host + paired-host persistence (injectable store)
    DiscoveryService.swift                   # protocol + MDNSDiscoveryService (merge + dedupe)
    BoxViewModel.swift                       # per-box @Observable state
    AppModel.swift                           # root @Observable @MainActor state
  Tests/TTStationKitTests/
    ServingStatusTests.swift
    ModelsTests.swift
    ProcessRunnerTests.swift
    BinaryLocatorTests.swift
    TTClientTests.swift
    DiscoveryServiceTests.swift
    BoxViewModelTests.swift
    Support/FakeProcessRunner.swift          # test double
    Support/FakeTTClient.swift               # test double
    Support/InMemoryStore.swift              # test double for persistence
    Fixtures/*.json                          # captured/authored tt --json outputs
  AppShell/
    project.yml                              # XcodeGen: TTStation app target -> package
    Sources/
      TTStationApp.swift                     # @main App + MenuBarExtra(.window)
      MenuContentView.swift
      BoxRowView.swift
      BoxDetailView.swift
      ManualHostSheet.swift
    Info.plist                               # LSUIElement = true
```

---

## Task 1: Swift package skeleton

**Files:**
- Create: `macos/TTStation/Package.swift`
- Create: `macos/TTStation/Sources/TTStationKit/Placeholder.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/SkeletonTests.swift`

**Interfaces:**
- Consumes: nothing.
- Produces: a buildable `TTStationKit` library target and a runnable test target. Establishes `swift test` as the TDD loop for Tasks 2–9.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/SkeletonTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class SkeletonTests: XCTestCase {
    func testPackageBuilds() {
        XCTAssertEqual(TTStationKit.marker, "ttstation")
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test`
Expected: FAIL — no such module/target `TTStationKit` (nothing exists yet).

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Package.swift`:
```swift
// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "TTStationKit",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "TTStationKit", targets: ["TTStationKit"]),
    ],
    targets: [
        .target(name: "TTStationKit"),
        .testTarget(name: "TTStationKitTests", dependencies: ["TTStationKit"]),
    ]
)
```

`macos/TTStation/Sources/TTStationKit/Placeholder.swift`:
```swift
public enum TTStationKit {
    public static let marker = "ttstation"
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test`
Expected: PASS (1 test).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Package.swift macos/TTStation/Sources macos/TTStation/Tests
git commit -m "feat(macos): TTStationKit swift package skeleton"
```

---

## Task 2: ServingStatus parsing

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/ServingStatus.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/ServingStatusTests.swift`

**Interfaces:**
- Consumes: nothing.
- Produces: `enum ServingStatus: Equatable { case idle; case serving(model: String) }` with `init(raw: String) throws` and `var isServing: Bool`. Consumed by `Models`, `TTClient`, `BoxViewModel`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/ServingStatusTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class ServingStatusTests: XCTestCase {
    func testIdle() throws {
        XCTAssertEqual(try ServingStatus(raw: "idle"), .idle)
    }
    func testServing() throws {
        XCTAssertEqual(try ServingStatus(raw: "serving:Qwen3-8B"), .serving(model: "Qwen3-8B"))
    }
    func testServingKeepsColonsInModel() throws {
        XCTAssertEqual(try ServingStatus(raw: "serving:a:b"), .serving(model: "a:b"))
    }
    func testInvalidThrows() {
        XCTAssertThrowsError(try ServingStatus(raw: "bogus"))
    }
    func testIsServing() throws {
        XCTAssertFalse(try ServingStatus(raw: "idle").isServing)
        XCTAssertTrue(try ServingStatus(raw: "serving:x").isServing)
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter ServingStatusTests`
Expected: FAIL — `ServingStatus` undefined.

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Sources/TTStationKit/ServingStatus.swift`:
```swift
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter ServingStatusTests`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/ServingStatus.swift macos/TTStation/Tests/TTStationKitTests/ServingStatusTests.swift
git commit -m "feat(macos): ServingStatus wire-form parsing"
```

---

## Task 3: Domain models + JSON decoding

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/Models.swift`
- Create: `macos/TTStation/Tests/TTStationKitTests/Fixtures/discover.json`
- Create: `macos/TTStation/Tests/TTStationKitTests/Fixtures/models.json`
- Create: `macos/TTStation/Tests/TTStationKitTests/Fixtures/endpoint.json`
- Test: `macos/TTStation/Tests/TTStationKitTests/ModelsTests.swift`
- Modify: `macos/TTStation/Package.swift` (add resources to the test target)

**Interfaces:**
- Consumes: `ServingStatus`.
- Produces:
  - `struct BoxRecord: Codable, Equatable { name; host; ctrlPort:Int; chips; statusRaw:String; apiver:Int; var status: ServingStatus? }` with CodingKeys mapping `ctrlPort→ctrl_port`, `statusRaw→status`.
  - `struct Endpoint: Codable, Equatable { baseURL:String; model:String; requiresKey:Bool }` mapping `baseURL→base_url`, `requiresKey→requires_key`.
  - `struct ModelInfo: Codable, Equatable { name; devices:[String] }`.
  - `struct ModelsResponse: Codable, Equatable { releaseVersion:String?; models:[ModelInfo] }` mapping `releaseVersion→release_version`.
  - `struct PairResult: Codable, Equatable { host; paired:Bool }`.
  - `struct StatusResponse: Codable, Equatable { status:String }`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/Fixtures/discover.json`:
```json
[{"name":"quietbox2","host":"192.168.5.119","ctrl_port":8080,"chips":"4xBH","status":"serving:Qwen3-8B","apiver":1}]
```

`macos/TTStation/Tests/TTStationKitTests/Fixtures/models.json`:
```json
{"release_version":"0.14.0","models":[{"name":"Qwen3-8B","devices":["P300X2"]},{"name":"Llama-3.1-8B-Instruct","devices":["P300X2","T3K"]}]}
```

`macos/TTStation/Tests/TTStationKitTests/Fixtures/endpoint.json`:
```json
{"base_url":"http://192.168.5.119:8000/v1","model":"Qwen3-8B","requires_key":false}
```

`macos/TTStation/Tests/TTStationKitTests/ModelsTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class ModelsTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        let url = Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures")
        return try Data(contentsOf: try XCTUnwrap(url))
    }

    func testDecodeDiscover() throws {
        let boxes = try JSONDecoder().decode([BoxRecord].self, from: fixture("discover"))
        XCTAssertEqual(boxes.count, 1)
        XCTAssertEqual(boxes[0].name, "quietbox2")
        XCTAssertEqual(boxes[0].ctrlPort, 8080)
        XCTAssertEqual(boxes[0].status, .serving(model: "Qwen3-8B"))
    }

    func testDecodeModelsWithReleaseVersion() throws {
        let resp = try JSONDecoder().decode(ModelsResponse.self, from: fixture("models"))
        XCTAssertEqual(resp.releaseVersion, "0.14.0")
        XCTAssertEqual(resp.models.map(\.name), ["Qwen3-8B", "Llama-3.1-8B-Instruct"])
        XCTAssertEqual(resp.models[1].devices, ["P300X2", "T3K"])
    }

    func testDecodeModelsNullReleaseVersion() throws {
        let data = Data(#"{"release_version":null,"models":[]}"#.utf8)
        let resp = try JSONDecoder().decode(ModelsResponse.self, from: data)
        XCTAssertNil(resp.releaseVersion)
        XCTAssertTrue(resp.models.isEmpty)
    }

    func testDecodeEndpoint() throws {
        let ep = try JSONDecoder().decode(Endpoint.self, from: fixture("endpoint"))
        XCTAssertEqual(ep.baseURL, "http://192.168.5.119:8000/v1")
        XCTAssertEqual(ep.model, "Qwen3-8B")
        XCTAssertFalse(ep.requiresKey)
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter ModelsTests`
Expected: FAIL — model types undefined (and/or resources not bundled).

- [ ] **Step 3: Write minimal implementation**

Modify `macos/TTStation/Package.swift` test target to bundle fixtures:
```swift
.testTarget(
    name: "TTStationKitTests",
    dependencies: ["TTStationKit"],
    resources: [.copy("Fixtures")]
),
```

`macos/TTStation/Sources/TTStationKit/Models.swift`:
```swift
import Foundation

public struct BoxRecord: Codable, Equatable {
    public let name: String
    public let host: String
    public let ctrlPort: Int
    public let chips: String
    public let statusRaw: String
    public let apiver: Int

    enum CodingKeys: String, CodingKey {
        case name, host, chips, apiver
        case ctrlPort = "ctrl_port"
        case statusRaw = "status"
    }

    /// `host:port` — the identity string every `tt` command keys off of.
    public var hostPort: String { "\(host):\(ctrlPort)" }

    public var status: ServingStatus? { try? ServingStatus(raw: statusRaw) }
}

public struct Endpoint: Codable, Equatable {
    public let baseURL: String
    public let model: String
    public let requiresKey: Bool

    enum CodingKeys: String, CodingKey {
        case model
        case baseURL = "base_url"
        case requiresKey = "requires_key"
    }
}

public struct ModelInfo: Codable, Equatable {
    public let name: String
    public let devices: [String]
}

public struct ModelsResponse: Codable, Equatable {
    public let releaseVersion: String?
    public let models: [ModelInfo]

    enum CodingKeys: String, CodingKey {
        case models
        case releaseVersion = "release_version"
    }
}

public struct PairResult: Codable, Equatable {
    public let host: String
    public let paired: Bool
}

public struct StatusResponse: Codable, Equatable {
    public let status: String
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter ModelsTests`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/Models.swift macos/TTStation/Package.swift macos/TTStation/Tests/TTStationKitTests/ModelsTests.swift macos/TTStation/Tests/TTStationKitTests/Fixtures
git commit -m "feat(macos): domain models + JSON decoding against captured fixtures"
```

---

## Task 4: TTError + process-runner protocol + fake

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/TTError.swift`
- Create: `macos/TTStation/Sources/TTStationKit/ProcessRunner.swift` (protocol only in this task)
- Create: `macos/TTStation/Tests/TTStationKitTests/Support/FakeProcessRunner.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/ProcessRunnerTests.swift`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `struct ProcessResult: Equatable { stdout: Data; stderr: String; exitCode: Int32 }`.
  - `protocol TTProcessRunner { func run(_ args: [String]) async throws -> ProcessResult }`.
  - `struct TTError: Error, Equatable { command:[String]; exitCode:Int32; stderr:String }` plus `case binaryNotFound([String])` — modeled as an enum below.
  - `final class FakeProcessRunner: TTProcessRunner` (test double): records `lastArgs`, returns a queued `ProcessResult` or throws a queued error.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/ProcessRunnerTests.swift`:
```swift
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter ProcessRunnerTests`
Expected: FAIL — `FakeProcessRunner` / `TTError` / `ProcessResult` undefined.

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Sources/TTStationKit/TTError.swift`:
```swift
import Foundation

/// Every failure the app surfaces. `commandFailed` carries the CLI's stderr
/// verbatim so the UI can show it (README: surface stderr, don't swallow it).
public enum TTError: Error, Equatable {
    case commandFailed(command: [String], exitCode: Int32, stderr: String)
    case binaryNotFound(triedPaths: [String])
    case decodeFailed(command: [String], detail: String)
}
```

`macos/TTStation/Sources/TTStationKit/ProcessRunner.swift`:
```swift
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
```

`macos/TTStation/Tests/TTStationKitTests/Support/FakeProcessRunner.swift`:
```swift
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter ProcessRunnerTests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/TTError.swift macos/TTStation/Sources/TTStationKit/ProcessRunner.swift macos/TTStation/Tests/TTStationKitTests/Support/FakeProcessRunner.swift macos/TTStation/Tests/TTStationKitTests/ProcessRunnerTests.swift
git commit -m "feat(macos): TTError, process-runner protocol, fake runner"
```

---

## Task 5: TTBinaryLocator

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/BinaryLocator.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/BinaryLocatorTests.swift`

**Interfaces:**
- Consumes: `TTError`.
- Produces: `struct TTBinaryLocator { init(override:String?, candidates:[String], fileExists:(String)->Bool); func locate() throws -> String }`. Returns first existing path in order [override?] + candidates; throws `TTError.binaryNotFound(triedPaths:)` listing all tried. `fileExists` is injected for testability; a `default` static builds real candidates (`~/.local/bin/tt`, `/opt/homebrew/bin/tt`, `/usr/local/bin/tt`) using `FileManager`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/BinaryLocatorTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class BinaryLocatorTests: XCTestCase {
    func testPrefersOverrideWhenItExists() throws {
        let loc = TTBinaryLocator(override: "/custom/tt", candidates: ["/a/tt"]) { $0 == "/custom/tt" }
        XCTAssertEqual(try loc.locate(), "/custom/tt")
    }
    func testFallsBackToFirstExistingCandidate() throws {
        let loc = TTBinaryLocator(override: nil, candidates: ["/a/tt", "/b/tt"]) { $0 == "/b/tt" }
        XCTAssertEqual(try loc.locate(), "/b/tt")
    }
    func testSkipsMissingOverride() throws {
        let loc = TTBinaryLocator(override: "/missing/tt", candidates: ["/b/tt"]) { $0 == "/b/tt" }
        XCTAssertEqual(try loc.locate(), "/b/tt")
    }
    func testThrowsListingAllTriedWhenNoneExist() {
        let loc = TTBinaryLocator(override: "/x/tt", candidates: ["/a/tt", "/b/tt"]) { _ in false }
        XCTAssertThrowsError(try loc.locate()) { error in
            XCTAssertEqual(error as? TTError, .binaryNotFound(triedPaths: ["/x/tt", "/a/tt", "/b/tt"]))
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter BinaryLocatorTests`
Expected: FAIL — `TTBinaryLocator` undefined.

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Sources/TTStationKit/BinaryLocator.swift`:
```swift
import Foundation

/// Resolves the `tt` binary. GUI apps do NOT inherit the shell PATH, so we
/// probe explicit locations in order and report every one we tried on failure.
public struct TTBinaryLocator {
    private let override: String?
    private let candidates: [String]
    private let fileExists: (String) -> Bool

    public init(override: String?, candidates: [String], fileExists: @escaping (String) -> Bool) {
        self.override = override
        self.candidates = candidates
        self.fileExists = fileExists
    }

    public func locate() throws -> String {
        var tried: [String] = []
        for path in ([override].compactMap { $0 } + candidates) {
            tried.append(path)
            if fileExists(path) { return path }
        }
        throw TTError.binaryNotFound(triedPaths: tried)
    }

    /// Real-world locator: user override (UserDefaults key `tt.binaryPath`)
    /// then the standard install locations.
    public static func standard(override: String? = UserDefaults.standard.string(forKey: "tt.binaryPath")) -> TTBinaryLocator {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return TTBinaryLocator(
            override: override,
            candidates: ["\(home)/.local/bin/tt", "/opt/homebrew/bin/tt", "/usr/local/bin/tt"],
            fileExists: { FileManager.default.isExecutableFile(atPath: $0) }
        )
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter BinaryLocatorTests`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/BinaryLocator.swift macos/TTStation/Tests/TTStationKitTests/BinaryLocatorTests.swift
git commit -m "feat(macos): TTBinaryLocator with injectable existence check"
```

---

## Task 6: RealProcessRunner (spawns `Process`)

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/ProcessRunner.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/ProcessRunnerTests.swift` (add integration cases)

**Interfaces:**
- Consumes: `TTProcessRunner`, `ProcessResult`, `TTBinaryLocator`, `TTError`.
- Produces: `final class RealProcessRunner: TTProcessRunner { init(locator: TTBinaryLocator) }`. Resolves the binary via the locator, spawns it with `args`, captures stdout (Data) + stderr (String) + exit code. Throws `TTError.binaryNotFound` if the locator fails.

- [ ] **Step 1: Write the failing test**

Append to `macos/TTStation/Tests/TTStationKitTests/ProcessRunnerTests.swift`:
```swift
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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter ProcessRunnerTests`
Expected: FAIL — `RealProcessRunner` undefined.

- [ ] **Step 3: Write minimal implementation**

Append to `macos/TTStation/Sources/TTStationKit/ProcessRunner.swift`:
```swift
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter ProcessRunnerTests`
Expected: PASS (4 tests total in this file).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/ProcessRunner.swift macos/TTStation/Tests/TTStationKitTests/ProcessRunnerTests.swift
git commit -m "feat(macos): RealProcessRunner spawns tt and captures output"
```

---

## Task 7: TTClient — read commands (discover, models, status, endpoint)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/TTClient.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/TTClientTests.swift`

**Interfaces:**
- Consumes: `TTProcessRunner`, `ProcessResult`, domain models, `ServingStatus`, `TTError`.
- Produces: `final class TTClient { init(runner: TTProcessRunner) }` with:
  - `func discover(manualHosts:[String], noMdns:Bool) async throws -> [BoxRecord]`
  - `func models(host:String) async throws -> [ModelInfo]`
  - `func status(host:String) async throws -> ServingStatus`
  - `func endpoint(host:String) async throws -> Endpoint`
  - private `decode<T:Decodable>` + `guardExit` helpers mapping non-zero exit → `TTError.commandFailed`, decode failure → `TTError.decodeFailed`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/TTClientTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class TTClientTests: XCTestCase {
    func testDiscoverBuildsArgsAndDecodes() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(
            stdout: Data(#"[{"name":"b","host":"h","ctrl_port":8080,"chips":"4xBH","status":"idle","apiver":1}]"#.utf8),
            stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let boxes = try await client.discover(manualHosts: ["h:8080"], noMdns: true)
        XCTAssertEqual(fake.lastArgs, ["--json", "discover", "--host", "h:8080", "--no-mdns"])
        XCTAssertEqual(boxes.first?.status, .idle)
    }

    func testStatusDecodesWrappedString() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"status":"serving:Qwen3-8B"}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let status = try await client.status(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "status", "--host", "h:8080"])
        XCTAssertEqual(status, .serving(model: "Qwen3-8B"))
    }

    func testModelsDecodes() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"release_version":null,"models":[{"name":"Qwen3-8B","devices":["P300X2"]}]}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let models = try await client.models(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "models", "--host", "h:8080"])
        XCTAssertEqual(models.map(\.name), ["Qwen3-8B"])
    }

    func testNonZeroExitThrowsWithStderr() async {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(), stderr: "no token stored for h:8080", exitCode: 1)
        let client = TTClient(runner: fake)
        do {
            _ = try await client.endpoint(host: "h:8080")
            XCTFail("expected throw")
        } catch {
            XCTAssertEqual(error as? TTError,
                .commandFailed(command: ["--json", "endpoint", "--host", "h:8080"], exitCode: 1, stderr: "no token stored for h:8080"))
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter TTClientTests`
Expected: FAIL — `TTClient` undefined.

- [ ] **Step 3: Write minimal implementation**

`macos/TTStation/Sources/TTStationKit/TTClient.swift`:
```swift
import Foundation

/// Typed façade over `tt --json`. One method per subcommand; the only place
/// argv is assembled and stdout is decoded.
public final class TTClient {
    private let runner: TTProcessRunner
    public init(runner: TTProcessRunner) { self.runner = runner }

    // MARK: Read commands

    public func discover(manualHosts: [String] = [], noMdns: Bool = false) async throws -> [BoxRecord] {
        var args = ["--json", "discover"]
        for h in manualHosts { args += ["--host", h] }
        if noMdns { args.append("--no-mdns") }
        return try await call(args, decode: [BoxRecord].self)
    }

    public func models(host: String) async throws -> [ModelInfo] {
        let resp = try await call(["--json", "models", "--host", host], decode: ModelsResponse.self)
        return resp.models
    }

    public func status(host: String) async throws -> ServingStatus {
        let resp = try await call(["--json", "status", "--host", host], decode: StatusResponse.self)
        do { return try ServingStatus(raw: resp.status) }
        catch { throw TTError.decodeFailed(command: ["--json", "status", "--host", host], detail: "bad status: \(resp.status)") }
    }

    public func endpoint(host: String) async throws -> Endpoint {
        try await call(["--json", "endpoint", "--host", host], decode: Endpoint.self)
    }

    // MARK: Helpers

    func call<T: Decodable>(_ args: [String], decode type: T.Type) async throws -> T {
        let result = try await runner.run(args)
        guard result.exitCode == 0 else {
            throw TTError.commandFailed(command: args, exitCode: result.exitCode, stderr: result.stderr.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        do { return try JSONDecoder().decode(T.self, from: result.stdout) }
        catch { throw TTError.decodeFailed(command: args, detail: String(describing: error)) }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter TTClientTests`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/TTClient.swift macos/TTStation/Tests/TTStationKitTests/TTClientTests.swift
git commit -m "feat(macos): TTClient read commands (discover/models/status/endpoint)"
```

---

## Task 8: TTClient — action commands (pair, run, stop)

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/TTClient.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/TTClientTests.swift` (add cases)

**Interfaces:**
- Consumes: same as Task 7.
- Produces, on `TTClient`:
  - `func pair(host:String, code:String) async throws -> PairResult`
  - `func run(host:String, model:String) async throws -> Endpoint`
  - `func stop(host:String) async throws`
  - `func isAuthError(_ error: TTError) -> Bool` — true when `commandFailed` stderr indicates a missing/invalid token (used by the view-model to flip back to unpaired).

- [ ] **Step 1: Write the failing test**

Append to `macos/TTStation/Tests/TTStationKitTests/TTClientTests.swift`:
```swift
extension TTClientTests {
    func testPairBuildsArgs() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"host":"h:8080","paired":true,"token":"deadbeef"}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let result = try await client.pair(host: "h:8080", code: "042817")
        XCTAssertEqual(fake.lastArgs, ["--json", "pair", "h:8080", "--code", "042817"])
        XCTAssertTrue(result.paired)
    }

    func testRunBuildsArgsModelFirst() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data(#"{"base_url":"http://h:8000/v1","model":"Qwen3-8B","requires_key":false}"#.utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        let ep = try await client.run(host: "h:8080", model: "Qwen3-8B")
        XCTAssertEqual(fake.lastArgs, ["--json", "run", "Qwen3-8B", "--host", "h:8080"])
        XCTAssertEqual(ep.model, "Qwen3-8B")
    }

    func testStopBuildsArgsAndIgnoresBody() async throws {
        let fake = FakeProcessRunner()
        fake.nextResult = ProcessResult(stdout: Data("{}".utf8), stderr: "", exitCode: 0)
        let client = TTClient(runner: fake)
        try await client.stop(host: "h:8080")
        XCTAssertEqual(fake.lastArgs, ["--json", "stop", "--host", "h:8080"])
    }

    func testIsAuthError() {
        let client = TTClient(runner: FakeProcessRunner())
        XCTAssertTrue(client.isAuthError(.commandFailed(command: [], exitCode: 1, stderr: "no token stored for h:8080; run `tt pair`")))
        XCTAssertFalse(client.isAuthError(.commandFailed(command: [], exitCode: 1, stderr: "connection refused")))
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter TTClientTests`
Expected: FAIL — `pair`/`run`/`stop`/`isAuthError` undefined.

- [ ] **Step 3: Write minimal implementation**

Append to `TTClient` in `macos/TTStation/Sources/TTStationKit/TTClient.swift`:
```swift
extension TTClient {
    // MARK: Action commands

    public func pair(host: String, code: String) async throws -> PairResult {
        try await call(["--json", "pair", host, "--code", code], decode: PairResult.self)
    }

    public func run(host: String, model: String) async throws -> Endpoint {
        try await call(["--json", "run", model, "--host", host], decode: Endpoint.self)
    }

    public func stop(host: String) async throws {
        let args = ["--json", "stop", "--host", host]
        let result = try await runner.run(args)
        guard result.exitCode == 0 else {
            throw TTError.commandFailed(command: args, exitCode: result.exitCode, stderr: result.stderr.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    public func isAuthError(_ error: TTError) -> Bool {
        if case let .commandFailed(_, _, stderr) = error {
            let s = stderr.lowercased()
            return s.contains("no token") || s.contains("unauthorized") || s.contains("401")
        }
        return false
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter TTClientTests`
Expected: PASS (8 tests total in this file).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/TTClient.swift macos/TTStation/Tests/TTStationKitTests/TTClientTests.swift
git commit -m "feat(macos): TTClient action commands (pair/run/stop) + auth-error detection"
```

---

## Task 9: HostRegistry + DiscoveryService (merge + dedupe)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/HostRegistry.swift`
- Create: `macos/TTStation/Sources/TTStationKit/DiscoveryService.swift`
- Create: `macos/TTStation/Tests/TTStationKitTests/Support/InMemoryStore.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/DiscoveryServiceTests.swift`

**Interfaces:**
- Consumes: `TTClient`, `BoxRecord`.
- Produces:
  - `protocol KeyValueStore { func stringArray(_ key:String)->[String]; func setStringArray(_ v:[String], _ key:String) }`; `UserDefaults: KeyValueStore` conformance; test `InMemoryStore`.
  - `final class HostRegistry { init(store:KeyValueStore); var manualHosts:[String]; func addManualHost(_:); func removeManualHost(_:); var pairedHosts:Set<String>; func markPaired(_:); func markUnpaired(_:) }` (keys `tt.manualHosts`, `tt.pairedHosts`).
  - `protocol DiscoveryService { func scan() async -> [BoxRecord] }`.
  - `final class MDNSDiscoveryService: DiscoveryService { init(client:TTClient, registry:HostRegistry) }` — runs `client.discover(manualHosts: registry.manualHosts, noMdns:false)`; on throw, falls back to probing manual hosts individually; dedupes by `hostPort` (mDNS wins over a bare manual entry). For the merge test we expose `static func merge(discovered:[BoxRecord], manualHosts:[String]) -> [BoxRecord]` that appends synthetic idle records for manual hosts not already discovered.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/Support/InMemoryStore.swift`:
```swift
@testable import TTStationKit

final class InMemoryStore: KeyValueStore {
    private var storage: [String: [String]] = [:]
    func stringArray(_ key: String) -> [String] { storage[key] ?? [] }
    func setStringArray(_ value: [String], _ key: String) { storage[key] = value }
}
```

`macos/TTStation/Tests/TTStationKitTests/DiscoveryServiceTests.swift`:
```swift
import XCTest
@testable import TTStationKit

final class DiscoveryServiceTests: XCTestCase {
    func testRegistryPersistsManualAndPaired() {
        let reg = HostRegistry(store: InMemoryStore())
        reg.addManualHost("h:8080")
        reg.addManualHost("h:8080") // dedupe
        reg.markPaired("h:8080")
        XCTAssertEqual(reg.manualHosts, ["h:8080"])
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        reg.markUnpaired("h:8080")
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
    }

    func testMergeAddsManualHostsNotDiscovered() {
        let discovered = [BoxRecord(name: "b", host: "1.2.3.4", ctrlPort: 8080, chips: "4xBH", statusRaw: "idle", apiver: 1)]
        let merged = MDNSDiscoveryService.merge(discovered: discovered, manualHosts: ["1.2.3.4:8080", "9.9.9.9:8080"])
        XCTAssertEqual(merged.count, 2) // discovered 1.2.3.4 kept once; 9.9.9.9 added
        XCTAssertTrue(merged.contains { $0.hostPort == "9.9.9.9:8080" })
        XCTAssertEqual(merged.filter { $0.hostPort == "1.2.3.4:8080" }.count, 1)
    }
}
```

Note: `BoxRecord`'s memberwise init must be `public`. In Task 3 the properties are `let` with no explicit init — add a `public init(...)` to `BoxRecord` now (memberwise init is internal by default for public structs).

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter DiscoveryServiceTests`
Expected: FAIL — `KeyValueStore`/`HostRegistry`/`MDNSDiscoveryService` undefined (and/or `BoxRecord.init` not public).

- [ ] **Step 3: Write minimal implementation**

Add to `macos/TTStation/Sources/TTStationKit/Models.swift` (public init on `BoxRecord`):
```swift
extension BoxRecord {
    public init(name: String, host: String, ctrlPort: Int, chips: String, statusRaw: String, apiver: Int) {
        self.name = name; self.host = host; self.ctrlPort = ctrlPort
        self.chips = chips; self.statusRaw = statusRaw; self.apiver = apiver
    }
}
```

`macos/TTStation/Sources/TTStationKit/HostRegistry.swift`:
```swift
import Foundation

public protocol KeyValueStore {
    func stringArray(_ key: String) -> [String]
    func setStringArray(_ value: [String], _ key: String)
}

extension UserDefaults: KeyValueStore {
    public func stringArray(_ key: String) -> [String] { stringArray(forKey: key) ?? [] }
    public func setStringArray(_ value: [String], _ key: String) { set(value, forKey: key) }
}

/// Persists manually-added hosts and which hosts the CLI has paired. The app
/// never reads the Keychain; "paired" is tracked here after a successful pair
/// and cleared on an auth error.
public final class HostRegistry {
    private let store: KeyValueStore
    private let manualKey = "tt.manualHosts"
    private let pairedKey = "tt.pairedHosts"

    public init(store: KeyValueStore) { self.store = store }

    public var manualHosts: [String] { store.stringArray(manualKey) }
    public func addManualHost(_ host: String) {
        var hosts = manualHosts
        guard !hosts.contains(host) else { return }
        hosts.append(host)
        store.setStringArray(hosts, manualKey)
    }
    public func removeManualHost(_ host: String) {
        store.setStringArray(manualHosts.filter { $0 != host }, manualKey)
    }

    public var pairedHosts: Set<String> { Set(store.stringArray(pairedKey)) }
    public func markPaired(_ host: String) {
        store.setStringArray(Array(pairedHosts.union([host])), pairedKey)
    }
    public func markUnpaired(_ host: String) {
        store.setStringArray(Array(pairedHosts.subtracting([host])), pairedKey)
    }
}
```

`macos/TTStation/Sources/TTStationKit/DiscoveryService.swift`:
```swift
import Foundation

public protocol DiscoveryService {
    func scan() async -> [BoxRecord]
}

public final class MDNSDiscoveryService: DiscoveryService {
    private let client: TTClient
    private let registry: HostRegistry
    public init(client: TTClient, registry: HostRegistry) {
        self.client = client; self.registry = registry
    }

    public func scan() async -> [BoxRecord] {
        let manual = registry.manualHosts
        let discovered = (try? await client.discover(manualHosts: manual, noMdns: false)) ?? []
        return Self.merge(discovered: discovered, manualHosts: manual)
    }

    /// Dedupe by `host:port`; append a synthetic idle record for any manual
    /// host the discovery pass didn't already return.
    public static func merge(discovered: [BoxRecord], manualHosts: [String]) -> [BoxRecord] {
        var byHostPort: [String: BoxRecord] = [:]
        for box in discovered { byHostPort[box.hostPort] = box }
        var result = discovered
        for host in manualHosts where byHostPort[host] == nil {
            let parts = host.split(separator: ":")
            guard parts.count == 2, let port = Int(parts[1]) else { continue }
            result.append(BoxRecord(name: host, host: String(parts[0]), ctrlPort: port, chips: "?", statusRaw: "idle", apiver: 1))
        }
        return result
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter DiscoveryServiceTests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/HostRegistry.swift macos/TTStation/Sources/TTStationKit/DiscoveryService.swift macos/TTStation/Sources/TTStationKit/Models.swift macos/TTStation/Tests/TTStationKitTests/Support/InMemoryStore.swift macos/TTStation/Tests/TTStationKitTests/DiscoveryServiceTests.swift
git commit -m "feat(macos): HostRegistry persistence + DiscoveryService merge/dedupe"
```

---

## Task 10: BoxViewModel + AppModel (state machine)

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/BoxViewModel.swift`
- Create: `macos/TTStation/Sources/TTStationKit/AppModel.swift`
- Create: `macos/TTStation/Tests/TTStationKitTests/Support/FakeTTClient.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/BoxViewModelTests.swift`

**Interfaces:**
- Consumes: `TTClient` (via a protocol so it can be faked), `DiscoveryService`, `HostRegistry`, models.
- Produces:
  - `protocol TTCommands` with the seven `TTClient` methods + `isAuthError`; `TTClient: TTCommands` conformance; test `FakeTTClient`.
  - `@Observable @MainActor final class BoxViewModel` with `let record: BoxRecord`, `var status: ServingStatus?`, `var endpoint: Endpoint?`, `var models:[ModelInfo]`, `var selectedModel:String?`, `var isPaired:Bool`, `var inFlight:Bool`, `var errorText:String?`, and `func refresh()`, `func loadModels()`, `func pair(code:)`, `func run()`, `func stop()`. On auth error during `run/stop`, sets `isPaired=false` and records unpaired in the registry.
  - `@Observable @MainActor final class AppModel` with `var boxes:[BoxViewModel]`, `var selectedHostPort:String?`, `var scanState: ScanState` (`enum { case idle, scanning, failed(String) }`), `func scan()`, `func addManualHost(_:)`, `init(commands:TTCommands, discovery:DiscoveryService, registry:HostRegistry)`.

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/Support/FakeTTClient.swift`:
```swift
import Foundation
@testable import TTStationKit

final class FakeTTClient: TTCommands {
    var models_ = [ModelInfo(name: "Qwen3-8B", devices: ["P300X2"])]
    var statusResult: ServingStatus = .idle
    var pairShouldSucceed = true
    var runEndpoint = Endpoint(baseURL: "http://h:8000/v1", model: "Qwen3-8B", requiresKey: false)
    var runError: TTError?

    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord] { [] }
    func models(host: String) async throws -> [ModelInfo] { models_ }
    func status(host: String) async throws -> ServingStatus { statusResult }
    func endpoint(host: String) async throws -> Endpoint { runEndpoint }
    func pair(host: String, code: String) async throws -> PairResult {
        if pairShouldSucceed { return PairResult(host: host, paired: true) }
        throw TTError.commandFailed(command: [], exitCode: 1, stderr: "invalid code")
    }
    func run(host: String, model: String) async throws -> Endpoint {
        if let runError { throw runError }
        return runEndpoint
    }
    func stop(host: String) async throws {}
    func isAuthError(_ error: TTError) -> Bool {
        if case let .commandFailed(_, _, s) = error { return s.lowercased().contains("no token") }
        return false
    }
}
```

Also add memberwise `public init`s to `ModelInfo`, `Endpoint`, `PairResult`, `ModelsResponse` if not already public (public structs need explicit public inits to be constructed from the test module).

`macos/TTStation/Tests/TTStationKitTests/BoxViewModelTests.swift`:
```swift
import XCTest
@testable import TTStationKit

@MainActor
final class BoxViewModelTests: XCTestCase {
    private func makeVM(paired: Bool = true, client: FakeTTClient = FakeTTClient()) -> (BoxViewModel, HostRegistry) {
        let reg = HostRegistry(store: InMemoryStore())
        let rec = BoxRecord(name: "b", host: "h", ctrlPort: 8080, chips: "4xBH", statusRaw: "idle", apiver: 1)
        if paired { reg.markPaired(rec.hostPort) }
        return (BoxViewModel(record: rec, commands: client, registry: reg), reg)
    }

    func testRefreshLoadsStatus() async {
        let client = FakeTTClient(); client.statusResult = .serving(model: "Qwen3-8B")
        let (vm, _) = makeVM(client: client)
        await vm.refresh()
        XCTAssertEqual(vm.status, .serving(model: "Qwen3-8B"))
    }

    func testPairSuccessMarksPairedAndLoadsModels() async {
        let (vm, reg) = makeVM(paired: false)
        await vm.pair(code: "042817")
        XCTAssertTrue(vm.isPaired)
        XCTAssertTrue(reg.pairedHosts.contains("h:8080"))
        XCTAssertEqual(vm.models.map(\.name), ["Qwen3-8B"])
    }

    func testRunSetsEndpoint() async {
        let (vm, _) = makeVM()
        vm.selectedModel = "Qwen3-8B"
        await vm.run()
        XCTAssertEqual(vm.endpoint?.baseURL, "http://h:8000/v1")
        XCTAssertFalse(vm.inFlight)
    }

    func testAuthErrorOnRunFlipsToUnpaired() async {
        let client = FakeTTClient()
        client.runError = .commandFailed(command: [], exitCode: 1, stderr: "no token stored")
        let (vm, reg) = makeVM(client: client)
        vm.selectedModel = "Qwen3-8B"
        await vm.run()
        XCTAssertFalse(vm.isPaired)
        XCTAssertFalse(reg.pairedHosts.contains("h:8080"))
        XCTAssertNotNil(vm.errorText)
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter BoxViewModelTests`
Expected: FAIL — `TTCommands`/`BoxViewModel` undefined.

- [ ] **Step 3: Write minimal implementation**

Add to `macos/TTStation/Sources/TTStationKit/TTClient.swift`:
```swift
/// Protocol so view-models can be tested against a fake.
public protocol TTCommands {
    func discover(manualHosts: [String], noMdns: Bool) async throws -> [BoxRecord]
    func models(host: String) async throws -> [ModelInfo]
    func status(host: String) async throws -> ServingStatus
    func endpoint(host: String) async throws -> Endpoint
    func pair(host: String, code: String) async throws -> PairResult
    func run(host: String, model: String) async throws -> Endpoint
    func stop(host: String) async throws
    func isAuthError(_ error: TTError) -> Bool
}

extension TTClient: TTCommands {}
```

Add public inits in `Models.swift` for `ModelInfo`, `Endpoint`, `PairResult`, `ModelsResponse`:
```swift
extension ModelInfo { public init(name: String, devices: [String]) { self.name = name; self.devices = devices } }
extension Endpoint { public init(baseURL: String, model: String, requiresKey: Bool) { self.baseURL = baseURL; self.model = model; self.requiresKey = requiresKey } }
extension PairResult { public init(host: String, paired: Bool) { self.host = host; self.paired = paired } }
extension ModelsResponse { public init(releaseVersion: String?, models: [ModelInfo]) { self.releaseVersion = releaseVersion; self.models = models } }
```

`macos/TTStation/Sources/TTStationKit/BoxViewModel.swift`:
```swift
import Foundation
import Observation

@Observable @MainActor
public final class BoxViewModel: Identifiable {
    public let record: BoxRecord
    public var id: String { record.hostPort }

    public var status: ServingStatus?
    public var endpoint: Endpoint?
    public var models: [ModelInfo] = []
    public var selectedModel: String?
    public var isPaired: Bool
    public var inFlight = false
    public var errorText: String?

    private let commands: TTCommands
    private let registry: HostRegistry

    public init(record: BoxRecord, commands: TTCommands, registry: HostRegistry) {
        self.record = record
        self.commands = commands
        self.registry = registry
        self.isPaired = registry.pairedHosts.contains(record.hostPort)
    }

    public func refresh() async {
        do {
            status = try await commands.status(host: record.hostPort)
            if isPaired { await loadModels() }
        } catch { record(error) }
    }

    public func loadModels() async {
        do {
            models = try await commands.models(host: record.hostPort)
            if selectedModel == nil { selectedModel = models.first?.name }
        } catch { record(error) }
    }

    public func pair(code: String) async {
        inFlight = true; defer { inFlight = false }
        do {
            _ = try await commands.pair(host: record.hostPort, code: code)
            isPaired = true
            registry.markPaired(record.hostPort)
            errorText = nil
            await loadModels()
        } catch { record(error) }
    }

    public func run() async {
        guard let model = selectedModel else { errorText = "Pick a model first."; return }
        inFlight = true; defer { inFlight = false }
        do {
            endpoint = try await commands.run(host: record.hostPort, model: model)
            status = .serving(model: model)
            errorText = nil
        } catch { record(error) }
    }

    public func stop() async {
        inFlight = true; defer { inFlight = false }
        do {
            try await commands.stop(host: record.hostPort)
            endpoint = nil
            status = .idle
            errorText = nil
        } catch { record(error) }
    }

    private func record(_ error: Error) {
        if let tt = error as? TTError {
            if commands.isAuthError(tt) {
                isPaired = false
                registry.markUnpaired(record.hostPort)
            }
            if case let .commandFailed(_, _, stderr) = tt { errorText = stderr.isEmpty ? "Command failed." : stderr }
            else { errorText = String(describing: tt) }
        } else {
            errorText = error.localizedDescription
        }
    }
}
```

`macos/TTStation/Sources/TTStationKit/AppModel.swift`:
```swift
import Foundation
import Observation

@Observable @MainActor
public final class AppModel {
    public enum ScanState: Equatable { case idle, scanning, failed(String) }

    public var boxes: [BoxViewModel] = []
    public var selectedHostPort: String?
    public var scanState: ScanState = .idle

    private let commands: TTCommands
    private let discovery: DiscoveryService
    private let registry: HostRegistry

    public init(commands: TTCommands, discovery: DiscoveryService, registry: HostRegistry) {
        self.commands = commands
        self.discovery = discovery
        self.registry = registry
    }

    public var selectedBox: BoxViewModel? {
        boxes.first { $0.id == selectedHostPort }
    }

    public func scan() async {
        scanState = .scanning
        let records = await discovery.scan()
        boxes = records.map { BoxViewModel(record: $0, commands: commands, registry: registry) }
        if selectedHostPort == nil { selectedHostPort = boxes.first?.id }
        for box in boxes { await box.refresh() }
        scanState = .idle
    }

    public func addManualHost(_ host: String) {
        registry.addManualHost(host)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test`
Expected: PASS (entire suite, all tasks).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/BoxViewModel.swift macos/TTStation/Sources/TTStationKit/AppModel.swift macos/TTStation/Sources/TTStationKit/TTClient.swift macos/TTStation/Sources/TTStationKit/Models.swift macos/TTStation/Tests/TTStationKitTests/Support/FakeTTClient.swift macos/TTStation/Tests/TTStationKitTests/BoxViewModelTests.swift
git commit -m "feat(macos): BoxViewModel + AppModel state machine, TTCommands protocol"
```

---

## Task 11: App target skeleton (XcodeGen + empty MenuBarExtra)

**Files:**
- Create: `macos/TTStation/AppShell/project.yml`
- Create: `macos/TTStation/AppShell/Info.plist`
- Create: `macos/TTStation/AppShell/Sources/TTStationApp.swift`
- Modify: `macos/TTStation/CLAUDE.md` or `macos/README.md` (note the build commands) — see step 5

**Interfaces:**
- Consumes: `TTStationKit` (as a local Swift package dependency).
- Produces: a buildable `TTStation.app` with an empty `MenuBarExtra`. No app logic yet. Establishes `xcodegen generate` + `xcodebuild build` as the app build loop.

- [ ] **Step 1: Ensure XcodeGen is available**

Run: `which xcodegen || brew install xcodegen`
Expected: a path to `xcodegen`.

- [ ] **Step 2: Write the project + app files**

`macos/TTStation/AppShell/project.yml`:
```yaml
name: TTStation
options:
  bundleIdPrefix: com.tenstorrent
  deploymentTarget:
    macOS: "14.0"
packages:
  TTStationKit:
    path: ..
targets:
  TTStation:
    type: application
    platform: macOS
    sources:
      - Sources
    dependencies:
      - package: TTStationKit
        product: TTStationKit
    settings:
      base:
        PRODUCT_BUNDLE_IDENTIFIER: com.tenstorrent.ttstation
        SWIFT_VERSION: "5.0"
        INFOPLIST_FILE: Info.plist
        GENERATE_INFOPLIST_FILE: NO
        MARKETING_VERSION: "0.1.0"
        CURRENT_PROJECT_VERSION: "1"
```

`macos/TTStation/AppShell/Info.plist`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>TTStation</string>
    <key>CFBundleIdentifier</key><string>com.tenstorrent.ttstation</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>LSUIElement</key><true/>
</dict>
</plist>
```

`macos/TTStation/AppShell/Sources/TTStationApp.swift`:
```swift
import SwiftUI
import TTStationKit

@main
struct TTStationApp: App {
    var body: some Scene {
        MenuBarExtra("TTStation", systemImage: "cpu") {
            Text("TTStation").padding()
        }
        .menuBarExtraStyle(.window)
    }
}
```

- [ ] **Step 3: Generate and build**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate && \
xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 4: Add the generated project to .gitignore (it's derived)**

Append to repo `.gitignore`:
```
macos/TTStation/AppShell/TTStation.xcodeproj/
macos/TTStation/.build/
```

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/project.yml macos/TTStation/AppShell/Info.plist macos/TTStation/AppShell/Sources/TTStationApp.swift .gitignore
git commit -m "feat(macos): XcodeGen app target with empty MenuBarExtra shell"
```

---

## Task 12: Views — wire the full loop

**Files:**
- Create: `macos/TTStation/AppShell/Sources/MenuContentView.swift`
- Create: `macos/TTStation/AppShell/Sources/BoxRowView.swift`
- Create: `macos/TTStation/AppShell/Sources/BoxDetailView.swift`
- Create: `macos/TTStation/AppShell/Sources/ManualHostSheet.swift`
- Modify: `macos/TTStation/AppShell/Sources/TTStationApp.swift`

**Interfaces:**
- Consumes: `AppModel`, `BoxViewModel`, `TTClient`, `RealProcessRunner`, `TTBinaryLocator`, `MDNSDiscoveryService`, `HostRegistry` from `TTStationKit`.
- Produces: the SwiftUI popover UI wiring the state layer to controls. No new testable logic (views); verified by build + the Task 13 mock-box run.

- [ ] **Step 1: Compose the app entry with a real AppModel**

`macos/TTStation/AppShell/Sources/TTStationApp.swift` (replace body):
```swift
import SwiftUI
import TTStationKit

@main
struct TTStationApp: App {
    @State private var model: AppModel

    init() {
        let registry = HostRegistry(store: UserDefaults.standard)
        let client = TTClient(runner: RealProcessRunner(locator: .standard()))
        let discovery = MDNSDiscoveryService(client: client, registry: registry)
        _model = State(initialValue: AppModel(commands: client, discovery: discovery, registry: registry))
    }

    var body: some Scene {
        MenuBarExtra("TTStation", systemImage: "cpu") {
            MenuContentView(model: model)
                .frame(width: 340)
        }
        .menuBarExtraStyle(.window)
    }
}
```

- [ ] **Step 2: Root menu content**

`macos/TTStation/AppShell/Sources/MenuContentView.swift`:
```swift
import SwiftUI
import TTStationKit

struct MenuContentView: View {
    @Bindable var model: AppModel
    @State private var showAddHost = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Tenstorrent Boxes").font(.headline)
                Spacer()
                if model.scanState == .scanning { ProgressView().scaleEffect(0.6) }
                Button { Task { await model.scan() } } label: { Image(systemName: "arrow.clockwise") }
                    .buttonStyle(.borderless)
            }
            if case let .failed(msg) = model.scanState {
                Text(msg).font(.caption).foregroundStyle(.red)
            }
            if model.boxes.isEmpty {
                Text("No boxes found — add one manually.").font(.caption).foregroundStyle(.secondary)
            } else {
                ForEach(model.boxes) { box in
                    BoxRowView(box: box, isSelected: box.id == model.selectedHostPort)
                        .onTapGesture { model.selectedHostPort = box.id }
                }
            }
            if let selected = model.selectedBox {
                Divider()
                BoxDetailView(box: selected)
            }
            Divider()
            Button("Add host…") { showAddHost = true }
            Button("Quit") { NSApplication.shared.terminate(nil) }
        }
        .padding(12)
        .task { await model.scan() }
        .sheet(isPresented: $showAddHost) {
            ManualHostSheet { host in
                model.addManualHost(host)
                Task { await model.scan() }
            }
        }
    }
}
```

- [ ] **Step 3: Box row (status dot + chips)**

`macos/TTStation/AppShell/Sources/BoxRowView.swift`:
```swift
import SwiftUI
import TTStationKit

struct BoxRowView: View {
    let box: BoxViewModel
    let isSelected: Bool

    private var isServing: Bool { box.status?.isServing ?? false }

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(isServing ? .green : .gray).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(box.record.name).fontWeight(isSelected ? .semibold : .regular)
                Text(box.record.chips).font(.caption2).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
    }
}
```

- [ ] **Step 4: Box detail (pair / model picker / run / stop / copy)**

`macos/TTStation/AppShell/Sources/BoxDetailView.swift`:
```swift
import SwiftUI
import TTStationKit

struct BoxDetailView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !box.isPaired {
                Text("Enter the 6-digit code shown on the box:").font(.caption)
                HStack {
                    TextField("000000", text: $code)
                        .textFieldStyle(.roundedBorder).frame(width: 100)
                    Button("Pair") { Task { await box.pair(code: code) } }
                        .disabled(code.count != 6 || box.inFlight)
                }
            } else {
                Picker("Model", selection: Binding(
                    get: { box.selectedModel ?? "" },
                    set: { box.selectedModel = $0 }
                )) {
                    ForEach(box.models, id: \.name) { Text($0.name).tag($0.name) }
                }
                .task { if box.models.isEmpty { await box.loadModels() } }

                HStack {
                    Button("Run") { Task { await box.run() } }.disabled(box.inFlight)
                    Button("Stop") { Task { await box.stop() } }.disabled(box.inFlight)
                    if box.inFlight { ProgressView().scaleEffect(0.6) }
                }

                if let ep = box.endpoint {
                    HStack {
                        Text(ep.baseURL).font(.system(.caption, design: .monospaced)).lineLimit(1).truncationMode(.middle)
                        Button { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(ep.baseURL, forType: .string) }
                            label: { Image(systemName: "doc.on.doc") }.buttonStyle(.borderless)
                    }
                }
            }
            if let err = box.errorText {
                Text(err).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
        }
    }
}
```

- [ ] **Step 5: Manual-host sheet**

`macos/TTStation/AppShell/Sources/ManualHostSheet.swift`:
```swift
import SwiftUI

struct ManualHostSheet: View {
    var onAdd: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var host = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add a box by address").font(.headline)
            TextField("host:port  (e.g. 192.168.5.119:8080)", text: $host)
                .textFieldStyle(.roundedBorder).frame(width: 280)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Add") { onAdd(host.trimmingCharacters(in: .whitespaces)); dismiss() }
                    .disabled(!host.contains(":"))
            }
        }
        .padding(16)
    }
}
```

- [ ] **Step 6: Build**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate && \
xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 7: Commit**

```bash
git add macos/TTStation/AppShell/Sources
git commit -m "feat(macos): wire full MenuBarExtra UI (discover/pair/run/stop/copy)"
```

---

## Task 13: End-to-end against `mock-box`

**Files:**
- Create: `macos/TTStation/AppShell/Sources/` (no new files — this is a verification task)
- Modify: `macos/README.md` (replace the "not built yet" status with build/run instructions)

**Interfaces:**
- Consumes: the built `TTStation.app`, the `tt` binary, and `mock-box` (`crates/mock-box`).
- Produces: a verified end-to-end run and updated docs. No code.

- [ ] **Step 1: Build the CLI and mock-box**

Run:
```bash
cd /Users/tsingletary/code/tt-station && cargo build --release -p tt -p mock-box && \
cp target/release/tt ~/.local/bin/tt
```
Expected: both build; `~/.local/bin/tt` refreshed.

- [ ] **Step 2: Start mock-box advertising over mDNS**

Run (leave running in a separate shell): `./target/release/mock-box advertise --name quietbox-mock --port 18899`
Expected: it prints that it is advertising + serving the control API. (Check `mock-box --help` for exact flag names and adjust; the intent is: mDNS advertise + control API + canned `/v1`.)

- [ ] **Step 3: Sanity-check the CLI sees it**

Run: `tt --json discover`
Expected: JSON array containing the mock box (name `quietbox-mock`). If mDNS is blocked, use `tt --json discover --host 127.0.0.1:18899`.

- [ ] **Step 4: Launch the app and drive the loop**

Run:
```bash
open macos/TTStation/AppShell/build/Release/TTStation.app 2>/dev/null || \
open "$(xcodebuild -project macos/TTStation/AppShell/TTStation.xcodeproj -scheme TTStation -showBuildSettings 2>/dev/null | awk '/ BUILT_PRODUCTS_DIR /{d=$3} /FULL_PRODUCT_NAME/{p=$3} END{print d"/"p}')"
```
Then, from the menu bar icon: confirm the mock box appears with a status dot → select it → Pair (read the 6-digit code from the mock-box console) → pick a model → Run → confirm an endpoint appears → Copy → Stop.
Expected: the whole loop works with no hardware. Capture any stderr shown in the UI and fix in the relevant `TTClient`/view-model task if the shape differs from fixtures.

- [ ] **Step 5: Update README and commit**

Replace the "Status: not built yet" section of `macos/README.md` with:
```markdown
**Status:** v1 built (`macos/TTStation/`). Logic in the `TTStationKit` Swift package
(`swift test`); SwiftUI app target generated with XcodeGen.

## Build & run

    cd macos/TTStation && swift test                     # unit tests (layers 1–4)
    cd macos/TTStation/AppShell && xcodegen generate \
      && xcodebuild -scheme TTStation -destination 'platform=macOS' build

End-to-end with no hardware: run `mock-box advertise …`, then launch the app and
drive discover → pair → run → copy endpoint.
```
Then:
```bash
git add macos/README.md
git commit -m "docs(macos): TTStation v1 build/run instructions; e2e verified against mock-box"
```

---

## Self-Review Notes

- **Spec coverage:** app shape/target (Task 11) ✓; five layers — runner (4/6), locator (5), client (7/8), models (2/3), discovery+registry (9), view-models (10) ✓; views (12) ✓; data-flow loop (12/13) ✓; error surfacing incl. stderr + binary-not-found (4,5,10,12) ✓; testing incl. fixtures + fakes + mock-box e2e (2–10,13) ✓; deferred items (notifications, console link, live poll, bundling, icon art) correctly absent ✓.
- **Type consistency:** `hostPort`, `ServingStatus`, `TTError.commandFailed/.binaryNotFound/.decodeFailed`, `TTCommands`, `KeyValueStore`, `ScanState` used identically across tasks. `TTClient.call` helper defined in Task 7 and reused in Task 8. Public memberwise inits added where the test module constructs public structs (Tasks 9, 10).
- **Known execution-time check:** `mock-box`'s exact CLI flags (Task 13 Step 2) must be read from `mock-box --help` at run time; the plan states intent rather than guessing flag spellings.

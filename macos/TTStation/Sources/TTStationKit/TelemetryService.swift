import Foundation

/// Read-only mirror of the agent's `GET /telemetry` WebSocket (unauthed route).
///
/// This is the one I/O path the app's redesign permits directly in Swift: a
/// long-lived, read-only socket that decodes verbatim `tt-smi -s` frames via
/// `TelemetrySnapshot.decode` (Task 8/8.5) and republishes them as
/// `@Observable` state for `DeviceStripView` (Task 13) to draw. All *control*
/// (pair/run/stop/reset/status) still goes through `tt --json` — this class
/// never sends anything, it only opens the socket and reads.
@Observable
@MainActor
public final class TelemetryService {

    /// Connection lifecycle as observed by the UI.
    ///
    /// `.stalled` is part of the required interface (Task 13's view is
    /// expected to switch over it) but this task does not implement a
    /// no-frame-timeout, so nothing in this file ever assigns it — the
    /// state machine here only ever visits `.idle -> .connecting -> .live`,
    /// with `.failed` on socket errors. Wiring a real "no frame in N
    /// seconds while nominally connected" timeout is a reasonable follow-up
    /// but is out of scope here; the case is kept (not deleted) so it
    /// doesn't have to be re-added later, and its absence from the
    /// assignments below is intentional, not an oversight.
    public enum ConnectionState: Equatable {
        case idle
        case connecting
        case live
        case stalled
        case failed(String)
    }

    public private(set) var snapshot: TelemetrySnapshot?
    public private(set) var state: ConnectionState = .idle

    private let session: URLSession
    private var webSocketTask: URLSessionWebSocketTask?

    /// The single task driving connect -> receive-loop -> backoff-and-reconnect
    /// for the current run. `stop()` (and a superseding `start()`) cancels it.
    private var runTask: Task<Void, Never>?

    /// Bumped on every `start()`/`stop()`. Each run captures its own value by
    /// copy and re-checks it before every state mutation. This is what makes
    /// "start while already running cancels + restarts" (and stop() itself)
    /// safe even though Swift Task cancellation is cooperative: a superseded
    /// run may still be mid-`await` for a beat after `stopCurrentRun()`
    /// returns, and this guard stops it from clobbering the *new* run's
    /// `state`/`snapshot` (or resurrecting them after a `stop()`) once it
    /// finally notices.
    private var generation = 0

    /// Reconnect backoff ladder in seconds: 1s, 2s, then capped at 5s.
    private static let backoffStepsSeconds: [UInt64] = [1, 2, 5]

    /// Test-only hook: called at the very top of `start(host:ctrlPort:lite:)`
    /// with the exact arguments passed in, before any URL-building or
    /// connection work happens. `nil` in production — exists purely so
    /// `BoxViewModelTests` can observe ref-counted subscribe/unsubscribe
    /// behavior (start-once, stop-at-zero) without opening a real socket.
    /// Never assigned outside tests; leaving it nil is a no-op.
    public var onStart: ((String, Int, Bool) -> Void)?

    /// Test-only hook: called at the very top of `stop()`, before the
    /// existing cancel/teardown logic. `nil` in production — see `onStart`.
    public var onStop: (() -> Void)?

    public init(session: URLSession = .shared) {
        self.session = session
    }

    /// Opens `ws://<host>:<ctrlPort>/telemetry` and starts receiving frames.
    ///
    /// Double-start policy: calling `start` while a run is already active
    /// cancels that run and begins a fresh one against the new
    /// `host`/`ctrlPort`, rather than no-op'ing. This matters for the app's
    /// case where the box being observed changes (e.g. the user switches
    /// boxes) — the caller shouldn't have to remember to `stop()` first.
    ///
    /// `lite` (default `true`) requests the thin `?view=lite` telemetry
    /// stream from the agent instead of the full verbatim `tt-smi -s`
    /// mirror — this is what lets `BoxViewModel`'s single shared
    /// subscription (see `subscribeTelemetry()`) stay cheap even when both
    /// the window's device strip and the popover are open at once.
    public func start(host: String, ctrlPort: Int, lite: Bool = true) {
        onStart?(host, ctrlPort, lite)

        stopCurrentRun()

        generation += 1
        let myGeneration = generation

        // mDNS hands back FQDNs with a trailing dot (`qb2-lab.local.`); strip
        // it so the URL we build matches the canonical `host:port` identity
        // the rest of the app already uses (mirrors `BoxRecord.hostPort` /
        // `WorkbenchLaunchers`'s `canonicalHost`).
        let canonicalHost = host.hasSuffix(".") ? String(host.dropLast()) : host
        let querySuffix = lite ? "?view=lite" : ""
        guard let url = URL(string: "ws://\(canonicalHost):\(ctrlPort)/telemetry\(querySuffix)") else {
            state = .failed("invalid telemetry URL for \(canonicalHost):\(ctrlPort)")
            return
        }

        state = .connecting

        runTask = Task { [weak self] in
            await self?.runLoop(url: url, generation: myGeneration)
        }
    }

    /// Cancels the socket and any pending reconnect (including a sleeping
    /// backoff wait) and sets `state = .idle`. Because cancellation is
    /// cooperative, the current run's `generation` check is what actually
    /// guarantees it stops touching `state`/`snapshot` — see `generation`.
    public func stop() {
        onStop?()
        stopCurrentRun()
        state = .idle
    }

    /// Invalidates the current run and tears down its socket, but does not
    /// itself touch `state` — callers decide what state follows (`.idle` for
    /// `stop()`, `.connecting` for a superseding `start()`).
    private func stopCurrentRun() {
        generation += 1
        webSocketTask?.cancel(with: .goingAway, reason: nil)
        webSocketTask = nil
        runTask?.cancel()
        runTask = nil
    }

    /// One run: open a socket, receive until it errors, back off, reconnect
    /// — repeating until cancelled or superseded by a newer `generation`.
    private func runLoop(url: URL, generation myGeneration: Int) async {
        var backoffIndex = 0

        while !Task.isCancelled, myGeneration == generation {
            let task = session.webSocketTask(with: url)
            webSocketTask = task
            task.resume()

            do {
                let receivedAnyFrame = try await receiveLoop(task: task, generation: myGeneration)
                // A successful stretch of frames means the socket was healthy;
                // don't let one hiccup after hours of uptime pay the full 5s
                // penalty next time.
                if receivedAnyFrame { backoffIndex = 0 }
            } catch {
                guard myGeneration == generation, !Task.isCancelled else { return }
                state = .failed(error.localizedDescription)
            }

            // Either receiveLoop threw (handled above) or returned normally,
            // which only happens once cancelled/superseded — recheck before
            // scheduling a reconnect so we don't spin one up needlessly.
            guard myGeneration == generation, !Task.isCancelled else { return }

            let delaySeconds = Self.backoffStepsSeconds[min(backoffIndex, Self.backoffStepsSeconds.count - 1)]
            backoffIndex += 1
            try? await Task.sleep(nanoseconds: delaySeconds * 1_000_000_000)

            guard myGeneration == generation, !Task.isCancelled else { return }
            state = .connecting
        }
    }

    /// Receives frames on `task` until it errors (thrown) or this run is
    /// cancelled/superseded (returns normally). Decodes every `.string`
    /// frame through `TelemetrySnapshot.decode` and publishes it, marking
    /// `state = .live` on each one.
    private func receiveLoop(task: URLSessionWebSocketTask, generation myGeneration: Int) async throws -> Bool {
        var receivedAnyFrame = false

        while !Task.isCancelled, myGeneration == generation {
            let message = try await task.receive()
            guard !Task.isCancelled, myGeneration == generation else { return receivedAnyFrame }

            switch message {
            case .string(let text):
                snapshot = TelemetrySnapshot.decode(text)
                state = .live
                receivedAnyFrame = true
            case .data(let data):
                // The agent's `/telemetry` route always sends `.string` JSON
                // text frames today; treat a `.data` frame as a protocol
                // surprise rather than silently dropping it — best-effort
                // decode it as UTF-8 text too.
                if let text = String(data: data, encoding: .utf8) {
                    snapshot = TelemetrySnapshot.decode(text)
                    state = .live
                    receivedAnyFrame = true
                }
            @unknown default:
                break
            }
        }

        return receivedAnyFrame
    }
}

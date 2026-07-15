import Foundation
import Observation

@Observable @MainActor
public final class BoxViewModel: Identifiable {
    // `record` and `id` are `nonisolated`: `Identifiable.id` is a nonisolated
    // protocol requirement, and `BoxRecord` is an immutable, Sendable-safe
    // value type, so reading it off the main actor is sound. Without this,
    // the compiler treats the whole `Identifiable` conformance as crossing
    // into main-actor-isolated code — a warning today, a hard error in the
    // Swift 6 language mode.
    nonisolated public let record: BoxRecord
    nonisolated public var id: String { record.hostPort }

    public var status: ServingStatus?
    public var endpoint: Endpoint?
    public var models: [ModelInfo] = []
    /// Every `/v1` endpoint currently serving on this box, from the unauthed
    /// `tt serving` read — including containers this box's agent did not
    /// launch (e.g. tt-studio), which carry `source == "external"`. Empty
    /// when nothing is serving or the read fails (never fatal).
    public var serving: [ServingEntry] = []
    /// The box's resolved serving configuration, from the unauthed `tt config`
    /// read — works regardless of pairing. `nil` when the read fails (never
    /// fatal) or hasn't completed yet.
    public var config: BoxConfig?
    /// The box's curated model catalog (runs-here / experimental / other-
    /// hardware tiers), from the unauthed `tt catalog` read — works regardless
    /// of pairing. `nil` when the read fails (never fatal) or hasn't completed
    /// yet.
    public var catalog: BoxCatalog?
    public var selectedModel: String?
    public var isPaired: Bool
    public var inFlight = false
    /// True from the moment `run()` is invoked until its endpoint returns (or
    /// it fails) — i.e. the model is spinning up but not yet serving. Drives
    /// the amber "starting" status dot and the "Starting <model>…" message,
    /// which are additive to (and never mutate) the existing `status` logic.
    public var starting = false
    /// Historically tracked an in-progress cancel while the UI waited for the
    /// abandoned `run()` to unwind. Cancel is now instant (see
    /// `cancelStart()`), so this is only ever transiently set — it stays
    /// around because `RunStopBar`/`BoxDetailView` still reference it for the
    /// (now effectively unreachable) "Canceling…" line, and dropping it would
    /// be a bigger view diff than this fix calls for.
    public var cancelling = false
    public var errorText: String?
    /// Non-nil once `startPairing()` has minted a pairing session and the
    /// box's console is showing a code, and cleared again once
    /// `completePairing`/`cancelPairing` end that session. Its presence is
    /// what the view uses to decide between the "Start pairing" button and
    /// the code-entry form.
    public var pairId: String?
    /// Opt-in toggle shown at the code-entry step of the pair flow: when on
    /// (the default), a successful `completePairing` follows up with
    /// `tt ssh-authorize` to install this Mac's key on the box as `ttuser`,
    /// so Terminal/tt-toplike/VS Code work keylessly right after pairing.
    public var enableSSH: Bool = true
    /// Result of the post-pair SSH-authorize step, success or non-fatal
    /// failure — surfaced as a one-line note in the pair UI. `nil` until a
    /// pairing (with `enableSSH` on) has actually run it. Cleared at the
    /// start of every `completePairing` call so a stale note from a previous
    /// pairing attempt never lingers on screen.
    public var sshMessage: String?
    /// Transient state after a power op is issued (Task 6/7): `.suspending`/
    /// `.rebooting`/`.poweredOff` set by `issuePower`, `.waking` by
    /// `wakeBox()`. Non-nil tells the UI (`BoxHeaderView`) the ensuing
    /// connection drop is expected, not an error. Cleared by `refresh()`
    /// once the box is reachable again — see `PowerTransition`.
    public var powerState: PowerState?

    /// The row-level "what's this box doing right now" summary, derived from
    /// `serving`/`status`/`starting` — see `RunningState.runningState` for
    /// the precedence rules. A computed property (not cached) so it always
    /// reflects the latest of those three fields; the fields themselves are
    /// tiny arrays/enums, so recomputing on read is cheap.
    public var runningState: RunningState {
        RunningState.runningState(serving: serving, status: status, starting: starting)
    }

    /// Single shared `/telemetry` socket for this box, ref-counted across
    /// however many views currently want it (the window's `DeviceStripView`
    /// and the popover's `BoxDetailView` — see `subscribeTelemetry()`) so
    /// two open views never open two sockets. Owns the connection; views
    /// only ever subscribe/unsubscribe, never call `telemetry.start`/`stop`
    /// directly.
    public let telemetry = TelemetryService()
    /// Count of views currently subscribed to `telemetry`. Starts the
    /// underlying socket on the 0->1 transition and stops it on 1->0; never
    /// goes negative (`unsubscribeTelemetry()` floors at zero) so a stray
    /// extra unsubscribe (e.g. a view tearing down twice) can't double-stop
    /// or underflow the count.
    private var telemetrySubscribers = 0

    private let commands: TTCommands
    private let registry: HostRegistry
    /// Bumped by every `run()` call and by `cancelStart()`. `run()` captures
    /// its own value at the start and checks it again after `commands.run`
    /// returns/throws — a mismatch means a cancel (or a newer `run()`)
    /// superseded this call while it was in flight, so its completion must
    /// be a no-op rather than clobbering state a cancel (or the newer run)
    /// already landed. See `run()` and `cancelStart()`.
    private var runGeneration = 0

    public init(record: BoxRecord, commands: TTCommands, registry: HostRegistry) {
        self.record = record
        self.commands = commands
        self.registry = registry
        self.isPaired = registry.pairedHosts.contains(record.hostPort)
        // Seed from the discover record so the status dot reflects reality
        // immediately, before any network round-trip. Unpaired boxes never
        // get an authed `status()` call (see refresh()), so without this
        // seed they'd show no status at all until paired.
        self.status = record.status
    }

    /// Registers a view's interest in this box's live telemetry. Only the
    /// 0->1 transition actually opens the socket (requesting the thin
    /// `?view=lite` stream — see `TelemetryService.start`); a second (or
    /// third, …) concurrent subscriber just bumps the count and rides the
    /// existing connection. Pair every call with a matching
    /// `unsubscribeTelemetry()` (e.g. in a view's `.onDisappear`).
    public func subscribeTelemetry() {
        telemetrySubscribers += 1
        if telemetrySubscribers == 1 {
            telemetry.start(host: record.host, ctrlPort: record.ctrlPort, lite: true)
        }
    }

    /// Releases a view's interest in this box's live telemetry. Only the
    /// last unsubscribe (count reaching zero) actually stops the socket;
    /// the guard against `telemetrySubscribers > 0` makes an extra/unmatched
    /// unsubscribe a no-op instead of underflowing the count or re-firing
    /// `telemetry.stop()`.
    public func unsubscribeTelemetry() {
        guard telemetrySubscribers > 0 else { return }
        telemetrySubscribers -= 1
        if telemetrySubscribers == 0 {
            telemetry.stop()
        }
    }

    public func refresh() async {
        // `GET /status`, `/serving`, `/config`, `/catalog` are all UNAUTHED
        // on the agent — every one of them answers 200 for any reachable box
        // regardless of pairing, by design (so `tt status`/discovery work
        // pre-pair). That means a successful call to any of them proves
        // *nothing* about whether this Mac holds a valid bearer token for
        // this box — they're display-only reads, fetched below with `try?`
        // and never treated as a pairing signal.
        //
        // `isPaired` is instead derived from the one bearer-guarded read this
        // view-model touches: `GET /endpoint`. Its outcomes map cleanly:
        //   - `401` (auth error)      → UNPAIRED — no valid token for this box.
        //   - `409` (Conflict, idle)  → PAIRED, nothing currently serving.
        //   - `200`                   → PAIRED, serving — carries the Endpoint.
        // A network/timeout/other failure is none of the above, so it leaves
        // `isPaired`/the registry untouched rather than bouncing a genuinely
        // paired (but slow or momentarily wedged) box to "unpaired".
        errorText = nil
        // `serving` surfaces models this agent didn't launch (e.g.
        // tt-studio), so fetch it independently of the pairing probe below.
        // Failure is never fatal — fall back to an empty list.
        serving = (try? await commands.serving(host: record.hostPort)) ?? []
        // `config` and `catalog` are likewise unauthed, display-only, and
        // never fatal — fall back to `nil` on failure.
        config = try? await commands.config(host: record.hostPort)
        catalog = try? await commands.catalog(host: record.hostPort)
        // `status` is unauthed too — purely a display read. Fall back to the
        // discovery-seeded status on failure; this is not a pairing signal.
        status = (try? await commands.status(host: record.hostPort)) ?? record.status

        // Whether this refresh actually got an HTTP response out of the box
        // at all (regardless of what it said) — the signal `PowerTransition`
        // needs to know the box is back, distinct from `isPaired` (which is
        // about token validity, not reachability). Starts optimistic;
        // flipped to `false` only in the generic-failure branches below,
        // which is where a rebooted/suspended/off box's dropped connection
        // actually lands.
        var reachable = true
        do {
            let ep = try await commands.endpoint(host: record.hostPort)
            isPaired = true
            registry.markPaired(record.hostPort)
            endpoint = ep
            await loadModels()
        } catch let e as TTError where commands.isAuthError(e) {
            // 401: no valid token for this box — the normal unpaired signal,
            // not an error to surface. Still proves the box answered HTTP.
            isPaired = false
            registry.markUnpaired(record.hostPort)
            endpoint = nil
        } catch let e as TTError where commands.isIdleConflict(e) {
            // 409: authed fine, just nothing serving right now. Also proves
            // the box answered HTTP.
            isPaired = true
            registry.markPaired(record.hostPort)
            endpoint = nil
            await loadModels()
        } catch let e as TTError {
            // Network/timeout/other — not an auth signal. Leave
            // `isPaired`/the registry untouched. This is also the branch a
            // suspended/rebooting/powered-off box's refresh lands in, so it
            // marks the box unreachable for `powerState` purposes.
            reachable = false
            if case let .timedOut(_, seconds) = e {
                errorText = Self.timeoutMessage(seconds: seconds)
            } else if case let .commandFailed(_, _, stderr) = e {
                errorText = stderr.isEmpty ? "Command failed." : stderr
            } else {
                errorText = String(describing: e)
            }
        } catch {
            reachable = false
            errorText = error.localizedDescription
        }
        // Recompute the power-op transient state now that we know whether
        // the box just answered: clears once it's reachable again (came
        // back from reboot / was woken), persists while it's still down.
        powerState = PowerTransition.onReachabilityChange(powerState, reachable: reachable)
    }

    private static func timeoutMessage(seconds: Double) -> String {
        "Timed out after \(Int(seconds))s — the box may be busy or unreachable."
    }

    public func loadModels() async {
        do {
            models = try await commands.models(host: record.hostPort)
            // Smart default: honour the user's last choice on this box, else
            // pick the best-scoring model so a freshly-paired box "just works"
            // without any interaction. Only seed when nothing is selected yet.
            if selectedModel == nil {
                selectedModel = ModelDefaults.pickDefaultModel(
                    from: models,
                    lastUsed: registry.lastModel(forHost: record.hostPort),
                    boxMesh: record.deviceMesh
                )
            }
        } catch { record(error) }
    }

    /// Step 1 of pairing: ask the agent to mint a pairing session. This is
    /// what makes the box print a fresh 6-digit code on its own console.
    public func startPairing() async {
        inFlight = true; defer { inFlight = false }
        errorText = nil
        do {
            pairId = try await commands.pairInit(host: record.hostPort).pairId
        } catch { record(error) }
    }

    /// Step 2 of pairing: submit the code the user read off the box's
    /// console against the session `startPairing()` opened.
    public func completePairing(code: String) async {
        guard let id = pairId else { return }
        inFlight = true; defer { inFlight = false }
        sshMessage = nil
        do {
            _ = try await commands.pairComplete(host: record.hostPort, pairId: id, code: code)
            isPaired = true
            registry.markPaired(record.hostPort)
            pairId = nil
            errorText = nil
            await loadModels()
            // Opt-in follow-up, never allowed to undo the pairing above: a
            // failed `ssh-authorize` call surfaces as a note, not an error,
            // and does not touch `isPaired`/`errorText`.
            if enableSSH { await authorizeSSH() }
        } catch {
            // Clear pairId on failure rather than letting the user retry the
            // same session: the agent expires pairing sessions and caps
            // attempts, so retrying a stale pair_id risks tripping the
            // lockout. Clearing sends them back to "Start pairing" for a
            // fresh code instead.
            record(error)
            pairId = nil
        }
    }

    /// Installs this Mac's SSH public key on the box as `ttuser` via
    /// `tt ssh-authorize`. Only ever called right after a successful pair
    /// (see `completePairing`) and deliberately non-fatal to it: any failure
    /// here is captured as a one-line note in `sshMessage`, not thrown, so a
    /// box that paired fine but couldn't set up SSH (e.g. no local key, box
    /// unreachable for the extra round-trip) still ends the flow paired.
    private func authorizeSSH() async {
        do {
            let info = try await commands.sshAuthorize(host: record.hostPort)
            if info.alreadyPresent {
                sshMessage = "SSH already enabled — connect as \(info.sshUser)."
            } else {
                sshMessage = "SSH enabled — connect as \(info.sshUser)."
            }
        } catch let e as TTError {
            switch e {
            case let .commandFailed(_, _, stderr):
                sshMessage = "SSH setup failed: \(stderr.isEmpty ? "command failed." : stderr)"
            case let .timedOut(_, seconds):
                sshMessage = "SSH setup timed out after \(Int(seconds))s."
            default:
                sshMessage = "SSH setup failed: \(String(describing: e))"
            }
        } catch {
            sshMessage = "SSH setup failed: \(error.localizedDescription)"
        }
    }

    /// Abandon an in-progress pairing session (e.g. the user wants to back
    /// out before entering a code). Mints nothing new — just returns to the
    /// "Start pairing" state.
    public func cancelPairing() {
        pairId = nil
        errorText = nil
    }

    public func run() async {
        guard let model = selectedModel else { errorText = "Pick a model first."; return }
        // Claim a new generation for this call. `cancelStart()` (or a newer
        // `run()`) bumping `runGeneration` past `myGen` is how a stale call
        // gets abandoned -- see the guard below.
        runGeneration += 1
        let myGen = runGeneration
        inFlight = true; starting = true; cancelling = false
        // Deliberately NO `defer` here: `commands.run` wraps `tt run`, which
        // can block for up to its 600s timeout (the agent's `/run` health-
        // poll does not abort promptly just because `stop` killed the
        // container underneath it). A `defer` would unconditionally clobber
        // `inFlight`/`starting`/`status` whenever this call eventually
        // returns -- including long after a cancel (or a newer `run()`) has
        // already moved the VM on. The generation guard below makes that
        // stale completion a no-op instead.
        let result: Result<Endpoint, Error>
        do { result = .success(try await commands.run(host: record.hostPort, model: model)) }
        catch { result = .failure(error) }
        guard myGen == runGeneration else { return } // superseded — ignore.
        inFlight = false; starting = false; cancelling = false
        switch result {
        case .success(let ep):
            endpoint = ep
            status = .serving(model: model)
            // Persist the choice so this box defaults to it next time.
            registry.setLastModel(model, forHost: record.hostPort)
            errorText = nil
        case .failure(let error):
            record(error)
        }
    }

    public func stop() async {
        // Bump the generation so an in-flight `run()` (if `stop` is ever
        // invoked mid-load) is abandoned and its late completion can't clobber
        // the idle state we set here — same guard `cancelStart()` relies on.
        // (Today the UI gates Stop and Cancel into mutually exclusive branches,
        // so this is belt-and-suspenders against a future call site.)
        runGeneration += 1
        inFlight = true; defer { inFlight = false }
        do {
            try await commands.stop(host: record.hostPort)
            endpoint = nil
            status = .idle
            errorText = nil
        } catch { record(error) }
    }

    /// Cancel an in-progress model load. Returns the UI to a usable idle
    /// state IMMEDIATELY — it does NOT wait for the (un-cancellable, up-to-
    /// 600s) `tt run` to return. Bumping `runGeneration` abandons that
    /// in-flight `run()` so its late completion is ignored (see `run()`'s
    /// guard). The agent-side `stop` (which actually tears the load down) is
    /// fired best-effort in the background so the UI is never coupled to it
    /// -- `tt stop` can itself block behind the agent's in-flight `/run`.
    /// No-op unless a load is actually starting (there's nothing to cancel
    /// otherwise).
    public func cancelStart() async {
        guard starting else { return }
        runGeneration += 1 // abandon the in-flight run()
        inFlight = false; starting = false; cancelling = false
        status = .idle; endpoint = nil; errorText = nil
        let commands = self.commands
        let host = record.hostPort
        Task { try? await commands.stop(host: host) } // best-effort, backgrounded
    }

    /// The primary stop/cancel control is available while a load is starting
    /// (to cancel it) or while a model is serving (to stop it) -- but not
    /// once a plain stop/cancel is already under way.
    public var canStopOrCancel: Bool {
        if cancelling { return false }
        if starting { return true }
        return status?.isServing ?? (endpoint != nil)
    }

    /// Fire a power action (`PowerMenuView`) and set the expected transient
    /// state so the following connection drop isn't rendered as an error.
    /// `resetChips` is instantaneous (`PowerTransition.next` returns `nil`
    /// for it) — it never disconnects the box/agent, so there's nothing to
    /// mask. The three machine ops (suspend/reboot/shutdown) do disconnect
    /// almost immediately after the agent accepts them, so `commands.power`
    /// failing here is the *expected* outcome, not a real error — see the
    /// swallow below. Always passes `record.hostPort`: `TTClient.power`
    /// requires a real `--host` and fails outright given `nil` (Task 6
    /// Minor).
    public func issuePower(_ action: PowerAction) async {
        powerState = PowerTransition.next(issued: action, reachable: true)
        do {
            try await commands.power(action, host: record.hostPort)
        } catch {
            // Machine ops routinely drop the connection right after the
            // agent accepts them — that's success, not failure. The
            // `powerState` set above already communicates what's happening,
            // so this is a deliberate swallow, not an oversight.
        }
    }

    /// Send a Wake-on-LAN magic packet at this box's last-known MAC. Fires
    /// directly (no confirmation) since waking a box is never destructive.
    /// `record.mac` is `nil` for an agent/discovery record that predates
    /// Task 3's MAC detection — `TTClient.wake` requires a real MAC and
    /// fails outright given `nil`, same reasoning as `issuePower`'s host.
    public func wakeBox() async {
        powerState = .waking
        try? await commands.wake(mac: record.mac, host: record.hostPort)
    }

    private func record(_ error: Error) {
        if let tt = error as? TTError {
            // `isAuthError` only matches `.commandFailed`, so `.timedOut`
            // (and any other non-auth case) never flips `isPaired` here —
            // a hang isn't evidence the token is bad.
            if commands.isAuthError(tt) {
                isPaired = false
                registry.markUnpaired(record.hostPort)
            }
            switch tt {
            case let .commandFailed(_, _, stderr): errorText = stderr.isEmpty ? "Command failed." : stderr
            case let .timedOut(_, seconds): errorText = Self.timeoutMessage(seconds: seconds)
            default: errorText = String(describing: tt)
            }
        } else {
            errorText = error.localizedDescription
        }
    }
}

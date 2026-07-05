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
    public var selectedModel: String?
    public var isPaired: Bool
    public var inFlight = false
    /// True from the moment `run()` is invoked until its endpoint returns (or
    /// it fails) — i.e. the model is spinning up but not yet serving. Drives
    /// the amber "starting" status dot and the "Starting <model>…" message,
    /// which are additive to (and never mutate) the existing `status` logic.
    public var starting = false
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

    private let commands: TTCommands
    private let registry: HostRegistry

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

    public func refresh() async {
        // Always probe `status()`, regardless of our locally-remembered
        // `isPaired` flag. UserDefaults-backed pairing state can go stale —
        // e.g. a pairing done via the CLI directly never touches this app's
        // registry — so it isn't a source of truth. The CLI's own token
        // store is: a successful authed `status` call means the CLI holds a
        // valid bearer token for this box (paired), and a "no token"/auth
        // failure means it doesn't (unpaired). `tt status` for an unpaired
        // box fails locally with no network round-trip, so probing it on
        // every refresh is cheap.
        errorText = nil
        // `serving` is an unauthed read that works regardless of pairing and
        // surfaces models this agent didn't launch (e.g. tt-studio), so fetch
        // it independently of the status/pairing flow below. Failure is never
        // fatal — fall back to an empty list rather than surfacing an error.
        serving = (try? await commands.serving(host: record.hostPort)) ?? []
        // `config` is likewise an unauthed read that works regardless of
        // pairing. Failure is never fatal — fall back to `nil` rather than
        // surfacing an error.
        config = try? await commands.config(host: record.hostPort)
        do {
            let s = try await commands.status(host: record.hostPort)
            isPaired = true
            registry.markPaired(record.hostPort)
            status = s
            if s.isServing { endpoint = try? await commands.endpoint(host: record.hostPort) }
            await loadModels()
        } catch let e as TTError where commands.isAuthError(e) {
            // Normal unpaired signal, not an error to surface — keep
            // whatever status the discovery record seeded us with.
            isPaired = false
            registry.markUnpaired(record.hostPort)
            status = record.status
        } catch {
            if let tt = error as? TTError, case let .commandFailed(_, _, stderr) = tt {
                errorText = stderr.isEmpty ? "Command failed." : stderr
            } else if let tt = error as? TTError, case let .timedOut(_, seconds) = tt {
                // A hang (e.g. the box's serving backend is down) is not an
                // auth signal — leave `isPaired`/`registry` untouched so a
                // genuinely paired box doesn't get bounced to "unpaired" just
                // because it's slow or wedged right now.
                errorText = Self.timeoutMessage(seconds: seconds)
            } else {
                errorText = error.localizedDescription
            }
        }
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
        // `starting` reflects the spin-up window (amber dot); `inFlight`
        // continues to gate the buttons. Both clear together on completion.
        inFlight = true; starting = true
        defer { inFlight = false; starting = false }
        do {
            endpoint = try await commands.run(host: record.hostPort, model: model)
            status = .serving(model: model)
            // Persist the choice so this box defaults to it next time.
            registry.setLastModel(model, forHost: record.hostPort)
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

import SwiftUI
import TTStationKit

/// The window's detail pane: composes the Task 13 cards into the full box
/// workspace, given the window's uncapped room. The popover (`BoxDetailView`)
/// keeps only a trimmed subset of this for quick actions.
///
/// **Card chrome:** `ConnectCardView`/`WorkbenchCardView`/`ServingCardView`/
/// `ConfigCardView` already wrap themselves in `CardContainer` (Task 13 for
/// the first three; `ConfigCardView` follows the same idiom). `BoxHeaderView`,
/// `DeviceStripView`, and `ModelBrowserView` (+ the inline run/stop/endpoint
/// block that goes with it) do not — Task 13 deliberately left them bare, so
/// this view wraps those three in `CardContainer` itself. The result: every
/// section in the pane reads as one consistent stack of titled cards, with
/// no section double-wrapped and none left bare.
///
/// **Box-switch state reset:** `@Bindable var box` changes identity when the
/// sidebar selection changes, but without an explicit `.id`, SwiftUI treats
/// this as the *same* view at the same tree position and keeps this view's
/// (and its subviews', notably `DeviceStripView`'s own `@State
/// TelemetryService`) `@State` alive across the switch — so the new box's UI
/// would render on top of the previous box's live telemetry socket/model
/// search text. `.id(box.id)` on the root `VStack` forces SwiftUI to treat
/// each box as a distinct view identity, tearing down and rebuilding all
/// `@State` in this subtree (including `DeviceStripView`'s socket, per its
/// own doc comment) whenever the selected box changes.
struct BoxWorkspaceView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""
    // Owns the one-click front-end (Connect) + workbench launchers shared by
    // every card below that shells out (Connect, Workbench, and DeviceStrip's
    // "Open tt-toplike ↗" button).
    @State private var launcher = LaunchController()

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            CardContainer(title: box.record.name) {
                BoxHeaderView(box: box)
            }

            if !box.isPaired {
                CardContainer(title: "Pairing") {
                    pairingBody
                }
            } else {
                CardContainer(title: "Devices") {
                    DeviceStripView(box: box, launcher: launcher)
                }

                // Read-only "what will this box serve with" summary. Guarded
                // on `box.config != nil` so an older agent (or a `/config`
                // read that failed — never fatal, see
                // `BoxViewModel.refresh()`) just omits the card instead of
                // this view having to render an empty/broken state.
                if let config = box.config {
                    ConfigCardView(config: config)
                }

                CardContainer(title: "Model") {
                    modelBody
                }

                if let ep = box.endpoint {
                    ConnectCardView(endpoint: ep, launcher: launcher)
                }

                WorkbenchCardView(box: box, launcher: launcher)

                ServingCardView(entries: box.serving)
            }

            // Shown at top level (not inside the pairing branch) because
            // `authorizeSSH()` populates this *after* `completePairing` flips
            // `isPaired = true` — if this lived inside `if !box.isPaired`,
            // SwiftUI would unmount that branch (and this Text with it) the
            // instant pairing succeeds, before the async SSH authorize call
            // resolves. Rendering it here means it survives the pairing ->
            // paired transition and the user actually sees the SSH result.
            if let sshMessage = box.sshMessage {
                Text(sshMessage).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
            }

            if let err = box.errorText {
                Text(err).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
        }
        // See the box-switch note above: this is what actually resets
        // per-box @State (including DeviceStripView's telemetry socket) when
        // the sidebar selection changes.
        .id(box.id)
    }

    /// Pairing UI, unchanged in behavior from the pre-composition monolith:
    /// "Start pairing" while no code has been requested yet, then a 6-digit
    /// code entry once one has.
    @ViewBuilder
    private var pairingBody: some View {
        if box.pairId == nil {
            Text("Pair to control this box.").font(.caption)
            HStack {
                Button("Start pairing") { Task { await box.startPairing() } }
                    .disabled(box.inFlight)
                if box.inFlight { ProgressView().scaleEffect(0.6) }
            }
        } else {
            Text("Enter the 6-digit code shown on the box:").font(.caption)
            HStack {
                TextField("000000", text: $code)
                    .textFieldStyle(.roundedBorder).frame(width: 100)
                Button("Pair") { Task { await box.completePairing(code: code) } }
                    .disabled(code.count != 6 || box.inFlight)
                Button("Start over") { box.cancelPairing() }
                    .disabled(box.inFlight)
                if box.inFlight { ProgressView().scaleEffect(0.6) }
            }
            Toggle("Also enable Terminal / SSH access (installs this Mac's key as ttuser)", isOn: $box.enableSSH)
                .toggleStyle(.checkbox)
                .font(.caption)
        }
    }

    /// Searchable model browser + Run/Stop + spin-up/serving feedback.
    /// Kept inline here (rather than folded into `ModelBrowserView`) because
    /// it reaches directly into `box.run()`/`box.stop()`/`box.endpoint` —
    /// exactly the carve-out the brief calls out as acceptable.
    @ViewBuilder
    private var modelBody: some View {
        ModelBrowserView(box: box, maxListHeight: nil)
            .task { if box.models.isEmpty { await box.loadModels() } }

        // Run is the primary action; Stop is a secondary, destructive one.
        // Both gated by `inFlight`; Run additionally needs a model.
        HStack(spacing: 8) {
            Button { Task { await box.run() } } label: {
                Label("Run", systemImage: "play.fill")
            }
            .buttonStyle(.borderedProminent)
            .disabled(box.selectedModel == nil || box.inFlight)
            .help("Start serving the selected model on this box.")

            Button(role: .destructive) { Task { await box.stop() } } label: {
                Label("Stop", systemImage: "stop.fill")
            }
            .buttonStyle(.bordered)
            .disabled(box.inFlight)
            .help("Stop the model currently serving on this box.")

            if box.inFlight { ProgressView().scaleEffect(0.6) }
        }
        .controlSize(.small)

        // Spin-up feedback: shown while `run()` is in flight, before the
        // endpoint returns. First run pulls the model image, which can take
        // minutes — say so, so the wait doesn't read as a hang.
        if box.starting {
            HStack(spacing: 6) {
                ProgressView().scaleEffect(0.6)
                Text("Starting \(box.selectedModel ?? "model")… (first run can take a few minutes)")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }

        if let ep = box.endpoint {
            // Prominent "Serving <model>" line so the running state is
            // unmistakable at a glance. Connecting a front-end to it lives
            // in the separate Connect card below.
            HStack(spacing: 4) {
                Image(systemName: "circle.fill").font(.system(size: 7)).foregroundStyle(TTTheme.statusServing)
                Text("Serving \(ep.model)").font(.caption.weight(.semibold))
            }
            HStack {
                Text(ep.baseURL).font(TTTheme.mono).lineLimit(1).truncationMode(.middle)
                Button { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(ep.baseURL, forType: .string) }
                    label: { Image(systemName: "doc.on.doc") }.buttonStyle(.borderless)
                    .help("Copy endpoint URL")
            }
        }
    }
}

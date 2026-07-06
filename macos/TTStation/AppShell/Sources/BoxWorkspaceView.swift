import SwiftUI
import TTStationKit

/// The window's detail pane: composes the Task 13 cards into the full box
/// workspace, given the window's uncapped room. The popover (`BoxDetailView`)
/// keeps only a trimmed subset of this for quick actions.
///
/// **Card chrome:** `ConnectCardView`/`WorkbenchCardView`/`ServingCardView`/
/// `ConfigCardView` already wrap themselves in `CardContainer` (Task 13 for
/// the first three; `ConfigCardView` follows the same idiom). `BoxHeaderView`,
/// `DeviceStripView`, and `ModelBrowserView` do not — Task 13 deliberately
/// left them bare, so this view wraps those three in `CardContainer` itself.
/// The result: every section in the pane reads as one consistent stack of
/// titled cards, with no section double-wrapped and none left bare.
///
/// **Run/Stop/serving:** used to live inline in `modelBody` alongside the
/// browser; Task 3 of the window redesign pulled it out into `RunStopBar`,
/// pinned below the scroll in `WindowRootView` so it's always visible
/// regardless of scroll position — `modelBody` here is now just the browser.
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

    /// Searchable model browser. Run/Stop/Cancel + the serving/endpoint line
    /// that used to live here have moved to `RunStopBar`, pinned below the
    /// scroll in `WindowRootView` — this card is now just the browser.
    @ViewBuilder
    private var modelBody: some View {
        // "Set up in Workbench →" (catalog mode's Experimental header) opens
        // the exact same VS Code launch the Workbench card's own button
        // below invokes — one launcher, one behavior, wherever the CTA
        // appears in the pane.
        ModelBrowserView(box: box, maxListHeight: nil, onOpenWorkbench: {
            Task { await launcher.openVSCode(host: box.record.host) }
        })
        .task { if box.models.isEmpty { await box.loadModels() } }
    }
}

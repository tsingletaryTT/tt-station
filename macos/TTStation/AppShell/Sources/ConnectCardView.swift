import SwiftUI
import TTStationKit

/// Front-end launchers (Open WebUI / opencode) for a box's live endpoint.
///
/// Serving-only by construction: this view takes an `Endpoint` directly
/// (not the box), so the composing view only reaches for it inside an
/// `if let ep = box.endpoint { ConnectCardView(endpoint: ep, launcher:
/// launcher) }` — there is no "no endpoint" state to render here at all.
/// Extracted from the "Connect:" row in the pre-card `BoxWorkspaceView`,
/// adding the phase-text (`webUIPhase`/`openCodePhase`) `LaunchController`
/// exposes so the card can say *what* is happening during an install
/// (e.g. "Installing opencode…"), not just that something is in flight.
struct ConnectCardView: View {
    let endpoint: Endpoint
    var launcher: LaunchController

    var body: some View {
        CardContainer(title: "Connect") {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Button {
                        Task { await launcher.openWebUI(endpoint: endpoint) }
                    } label: {
                        Label("Open Web UI", systemImage: "globe")
                    }
                    .disabled(launcher.isLaunchingWebUI)
                    .help("Launch Open WebUI locally (uvx) wired to this model and open it in your browser.")

                    Button {
                        Task { await launcher.openInOpenCode(endpoint: endpoint) }
                    } label: {
                        Label("Open in opencode", systemImage: "terminal")
                    }
                    .disabled(launcher.isLaunchingOpenCode)
                    .help("Open a Terminal running opencode with this model preselected.")

                    if launcher.isLaunchingWebUI || launcher.isLaunchingOpenCode {
                        ProgressView().scaleEffect(0.6)
                    }
                }
                .controlSize(.small)

                if let phase = launcher.webUIPhase ?? launcher.openCodePhase {
                    Text(phase).font(.caption).foregroundStyle(.secondary)
                }
                if let e = launcher.webUIError ?? launcher.openCodeError {
                    Text(e).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                }
            }
        }
    }
}

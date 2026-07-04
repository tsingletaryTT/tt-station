import SwiftUI
import TTStationKit

/// Compact popover detail: pairing + quick actions for the selected box, plus
/// an "Open window" affordance. Model browsing and the full `/serving` list
/// moved to the resizable window (`BoxWorkspaceView`); this view Runs the
/// current/smart-default `selectedModel`.
struct BoxDetailView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""
    @State private var launcher = LaunchController()
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !box.isPaired {
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
                }
            } else {
                HStack(spacing: 8) {
                    Button { Task { await box.run() } } label: {
                        Label("Run", systemImage: "play.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(box.selectedModel == nil || box.inFlight)
                    .help("Run the selected model. Browse/choose models in the window.")

                    Button(role: .destructive) { Task { await box.stop() } } label: {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .buttonStyle(.bordered)
                    .disabled(box.inFlight)
                    .help("Stop the model currently serving on this box.")

                    if box.inFlight { ProgressView().scaleEffect(0.6) }
                }
                .controlSize(.small)
                // Keep the smart-default `selectedModel` populated even though
                // the browser now lives in the window, so Run is enabled here.
                .task { if box.models.isEmpty { await box.loadModels() } }

                if box.starting {
                    HStack(spacing: 6) {
                        ProgressView().scaleEffect(0.6)
                        Text("Starting \(box.selectedModel ?? "model")… (first run can take a few minutes)")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }

                if let ep = box.endpoint {
                    HStack(spacing: 4) {
                        Image(systemName: "circle.fill").font(.system(size: 7)).foregroundStyle(.green)
                        Text("Serving \(ep.model)").font(.caption.weight(.semibold))
                            .lineLimit(1).truncationMode(.middle)
                    }
                    HStack {
                        Text(ep.baseURL).font(.system(.caption, design: .monospaced))
                            .lineLimit(1).truncationMode(.middle)
                        Button {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(ep.baseURL, forType: .string)
                        } label: { Image(systemName: "doc.on.doc") }
                        .buttonStyle(.borderless).help("Copy endpoint URL")
                    }
                    HStack(spacing: 8) {
                        Text("Connect:").font(.caption).foregroundStyle(.secondary)
                        Button { Task { await launcher.openWebUI(endpoint: ep) } } label: {
                            Label("Open Web UI", systemImage: "globe")
                        }
                        .disabled(launcher.isLaunchingWebUI)
                        Button { Task { await launcher.openInOpenCode(endpoint: ep) } } label: {
                            Label("opencode", systemImage: "terminal")
                        }
                        .disabled(launcher.isLaunchingOpenCode)
                        if launcher.isLaunchingWebUI || launcher.isLaunchingOpenCode {
                            ProgressView().scaleEffect(0.6)
                        }
                    }
                    if let e = launcher.webUIError ?? launcher.openCodeError {
                        Text(e).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                    }
                }

                Button { openWindow(id: "main") } label: {
                    Label("Open TTStation window", systemImage: "macwindow")
                }
                .controlSize(.small)
            }
            if let err = box.errorText {
                Text(err).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
        }
    }
}

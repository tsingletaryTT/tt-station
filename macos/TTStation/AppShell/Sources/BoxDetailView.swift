import SwiftUI
import TTStationKit

struct BoxDetailView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""
    // Owns the one-click front-end launchers (Open Web UI / opencode). Only
    // used in the serving branch, where `box.endpoint != nil`.
    @State private var launcher = LaunchController()

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
                // Searchable, family-grouped browser (sets box.selectedModel).
                ModelPickerView(box: box)
                    .task { if box.models.isEmpty { await box.loadModels() } }

                // Run is the primary action; Stop is a secondary, destructive
                // one. Both gated by `inFlight`; Run additionally needs a model.
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

                // Spin-up feedback: shown while `run()` is in flight, before
                // the endpoint returns. First run pulls the model image, which
                // can take minutes — say so, so the wait doesn't read as a hang.
                if box.starting {
                    HStack(spacing: 6) {
                        ProgressView().scaleEffect(0.6)
                        Text("Starting \(box.selectedModel ?? "model")… (first run can take a few minutes)")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }

                if let ep = box.endpoint {
                    // Prominent "Serving <model>" line so the running state is
                    // unmistakable at a glance.
                    HStack(spacing: 4) {
                        Image(systemName: "circle.fill").font(.system(size: 7)).foregroundStyle(.green)
                        Text("Serving \(ep.model)").font(.caption.weight(.semibold))
                    }
                    HStack {
                        Text(ep.baseURL).font(.system(.caption, design: .monospaced)).lineLimit(1).truncationMode(.middle)
                        Button { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(ep.baseURL, forType: .string) }
                            label: { Image(systemName: "doc.on.doc") }.buttonStyle(.borderless)
                            .help("Copy endpoint URL")
                    }

                    // Connect a local front-end to the running model. Shown only
                    // while serving (this `if let ep` branch); each button spins
                    // and disables independently and surfaces its own error.
                    HStack(spacing: 8) {
                        Text("Connect:").font(.caption).foregroundStyle(.secondary)
                        Button {
                            Task { await launcher.openWebUI(endpoint: ep) }
                        } label: {
                            Label("Open Web UI", systemImage: "globe")
                        }
                        .disabled(launcher.isLaunchingWebUI)
                        .help("Launch Open WebUI locally (uvx) wired to this model and open it in your browser.")
                        Button {
                            Task { await launcher.openInOpenCode(endpoint: ep) }
                        } label: {
                            Label("Open in opencode", systemImage: "terminal")
                        }
                        .disabled(launcher.isLaunchingOpenCode)
                        .help("Open a Terminal running opencode with this model preselected.")
                        if launcher.isLaunchingWebUI || launcher.isLaunchingOpenCode {
                            ProgressView().scaleEffect(0.6)
                        }
                    }
                    if let e = launcher.webUIError ?? launcher.openCodeError {
                        Text(e).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                    }
                }
            }
            // Currently-serving endpoints from the unauthed `tt serving` read —
            // shown regardless of pairing so externally-launched models (e.g.
            // tt-studio) are visible too. Each row mirrors the endpoint copy
            // affordance above; a small badge marks `source == "external"`.
            if !box.serving.isEmpty {
                Divider()
                Text("Serving").font(.caption).foregroundStyle(.secondary)
                ForEach(box.serving, id: \.hostPort) { entry in
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 4) {
                            Text(entry.model).font(.caption).lineLimit(1).truncationMode(.middle)
                            if entry.source == "external" {
                                Text("external")
                                    .font(.system(size: 9, weight: .semibold))
                                    .padding(.horizontal, 4).padding(.vertical, 1)
                                    .background(Color.secondary.opacity(0.2))
                                    .clipShape(Capsule())
                            }
                        }
                        HStack {
                            Text(entry.baseURL).font(.system(.caption, design: .monospaced)).lineLimit(1).truncationMode(.middle)
                            Button { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(entry.baseURL, forType: .string) }
                                label: { Image(systemName: "doc.on.doc") }.buttonStyle(.borderless)
                                .help("Copy endpoint URL")
                        }
                    }
                }
            }
            if let err = box.errorText {
                Text(err).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
        }
    }
}

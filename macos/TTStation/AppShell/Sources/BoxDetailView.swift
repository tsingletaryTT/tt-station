import SwiftUI
import TTStationKit

struct BoxDetailView: View {
    @Bindable var box: BoxViewModel
    @State private var code = ""

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

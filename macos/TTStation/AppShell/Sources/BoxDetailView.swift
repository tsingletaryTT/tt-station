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

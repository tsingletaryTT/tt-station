import SwiftUI
import TTStationKit

struct MenuContentView: View {
    @Bindable var model: AppModel
    @State private var showAddHost = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Tenstorrent Boxes").font(.headline)
                Spacer()
                if model.scanState == .scanning { ProgressView().scaleEffect(0.6) }
                Button { Task { await model.scan() } } label: { Image(systemName: "arrow.clockwise") }
                    .buttonStyle(.borderless)
            }
            if case let .failed(msg) = model.scanState {
                Text(msg).font(.caption).foregroundStyle(.red)
            }
            if model.boxes.isEmpty {
                Text("No boxes found — add one manually.").font(.caption).foregroundStyle(.secondary)
            } else {
                ForEach(model.boxes) { box in
                    BoxRowView(box: box, isSelected: box.id == model.selectedHostPort)
                        .onTapGesture { model.selectedHostPort = box.id }
                }
            }
            if let selected = model.selectedBox {
                Divider()
                BoxDetailView(box: selected)
            }
            Divider()
            Button("Add host…") { showAddHost = true }
            Button("Quit") { NSApplication.shared.terminate(nil) }
        }
        .padding(12)
        .task { await model.scan() }
        .sheet(isPresented: $showAddHost) {
            ManualHostSheet { host in
                model.addManualHost(host)
                Task { await model.scan() }
            }
        }
    }
}

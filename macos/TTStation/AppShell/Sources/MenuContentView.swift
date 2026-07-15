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
                // Mirrors `BoxHeaderView`'s power menu (Task 7) so power
                // control is reachable from the popover too, not just the
                // resizable window — same `PowerMenuView`, same `isPaired`
                // gate, just embedded next to the selected box's name here
                // since the popover has no header row of its own.
                HStack {
                    Text(selected.record.name).font(.subheadline.weight(.semibold))
                    Spacer()
                    if selected.isPaired {
                        PowerMenuView(box: selected)
                    }
                }
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

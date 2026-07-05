import SwiftUI
import TTStationKit

/// The window's sidebar: the discovered boxes as a selectable List, kept in
/// sync with the popover via `AppModel.selectedHostPort`. Refresh + Add host
/// live in a bottom bar.
struct BoxSidebarView: View {
    @Bindable var model: AppModel
    @State private var showAddHost = false

    var body: some View {
        List(selection: Binding(
            get: { model.selectedHostPort },
            set: { model.selectedHostPort = $0 }
        )) {
            Section("Boxes") {
                ForEach(model.boxes) { box in
                    HStack(spacing: 6) {
                        Image(systemName: "circle.fill")
                            .font(.system(size: 7))
                            .foregroundStyle(TTTheme.statusColor(
                                isServing: box.status?.isServing ?? false,
                                isStarting: box.starting
                            ))
                        VStack(alignment: .leading, spacing: 1) {
                            Text(box.record.name)
                            Text(box.record.chips).font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    .tag(box.id as String?)
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Button { Task { await model.scan() } } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                Spacer()
                Button("Add host…") { showAddHost = true }
            }
            .controlSize(.small)
            .padding(8)
        }
        .sheet(isPresented: $showAddHost) {
            ManualHostSheet { host in
                model.addManualHost(host)
                Task { await model.scan() }
            }
        }
    }
}

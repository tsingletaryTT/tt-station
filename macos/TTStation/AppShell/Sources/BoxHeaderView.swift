import SwiftUI
import TTStationKit

/// The workspace's identity strip: box name, its raw `chips` summary (e.g.
/// `"4xBH"` — already short/readable as returned by the agent, so this just
/// displays it rather than reformatting), a device-mesh badge, and a
/// reachability dot.
///
/// The status-dot palette mirrors `BoxRowView`'s sidebar semantics (amber
/// while `starting` takes precedence, green once serving, gray idle) plus one
/// state the sidebar row doesn't need: red when `box.errorText` is set, since
/// this header is the one place in the detail pane an error is always
/// in view.
struct BoxHeaderView: View {
    let box: BoxViewModel

    private var statusColor: Color {
        if box.errorText != nil { return .red }
        if box.starting { return .orange }
        if box.status?.isServing == true { return .green }
        return .gray
    }

    private var statusHelp: String {
        if box.errorText != nil { return "Error — see details below." }
        if box.starting { return "Starting a model…" }
        if box.status?.isServing == true { return "Serving a model" }
        return "Idle"
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Circle().fill(statusColor).frame(width: 9, height: 9).help(statusHelp)
            VStack(alignment: .leading, spacing: 2) {
                Text(box.record.name).font(.title3.weight(.semibold))
                Text(box.record.chips).font(.caption).foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
            if let mesh = box.record.deviceMesh, !mesh.isEmpty {
                Text(mesh.uppercased())
                    .font(TTTheme.mono)
                    .padding(.horizontal, 8).padding(.vertical, 3)
                    .background(TTTheme.teal.opacity(0.18))
                    .foregroundStyle(TTTheme.teal)
                    .clipShape(Capsule())
                    .help("Device mesh reported by this box")
            }
        }
    }
}

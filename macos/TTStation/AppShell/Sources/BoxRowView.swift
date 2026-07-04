import SwiftUI
import TTStationKit

struct BoxRowView: View {
    let box: BoxViewModel
    let isSelected: Bool

    private var isServing: Bool { box.status?.isServing ?? false }

    /// Status dot colour: amber while a model is spinning up (`starting`),
    /// green once serving, grey when idle. `starting` takes precedence so the
    /// transient spin-up state is visible even before `status` flips.
    private var statusColor: Color {
        if box.starting { return .orange }
        return isServing ? .green : .gray
    }

    private var statusHelp: String {
        if box.starting { return "Starting a model…" }
        return isServing ? "Serving a model" : "Idle"
    }

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(statusColor).frame(width: 8, height: 8)
                .help(statusHelp)
            VStack(alignment: .leading, spacing: 1) {
                Text(box.record.name).fontWeight(isSelected ? .semibold : .regular)
                Text(box.record.chips).font(.caption2).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
    }
}

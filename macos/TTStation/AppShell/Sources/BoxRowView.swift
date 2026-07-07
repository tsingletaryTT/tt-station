import SwiftUI
import TTStationKit

struct BoxRowView: View {
    let box: BoxViewModel
    let isSelected: Bool

    /// Status dot colour: amber while a model is spinning up (`starting`),
    /// green once serving, grey when idle. Derived from `box.runningState` —
    /// the same broad signal (agent + external `/serving` entries) that
    /// drives the waveform glyph and model line below — so the dot can never
    /// disagree with the rest of the row (e.g. a box serving only an
    /// external/tt-studio model must still show green, not grey).
    private var statusColor: Color {
        switch box.runningState {
        case .idle:
            return TTTheme.statusColor(isServing: false, isStarting: false)
        case .starting:
            return TTTheme.statusColor(isServing: false, isStarting: true)
        case .serving:
            return TTTheme.statusColor(isServing: true, isStarting: false)
        }
    }

    private var statusHelp: String {
        switch box.runningState {
        case .idle: return "Idle"
        case .starting: return "Starting a model…"
        case .serving: return "Serving a model"
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            // Small product thumbnail for a recognized chassis (QuietBox 2 →
            // p300x2), so the box is identifiable at a glance in the popover.
            if let art = DeviceArtwork.assetName(forMesh: box.record.deviceMesh) {
                Image(art)
                    .resizable()
                    .scaledToFit()
                    .frame(width: 28, height: 28)
                    .clipShape(RoundedRectangle(cornerRadius: 5))
                    .accessibilityLabel("Tenstorrent QuietBox 2")
            }
            Circle().fill(statusColor).frame(width: 8, height: 8)
                .help(statusHelp)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    if case .serving = box.runningState {
                        // Small serving glyph ahead of the name — a second,
                        // always-visible cue beyond the status dot (which
                        // can be easy to miss at 8pt) that this box is
                        // actively running a model.
                        Image(systemName: "waveform")
                            .font(.caption2)
                            .foregroundStyle(TTTheme.teal)
                    }
                    Text(box.record.name).fontWeight(isSelected ? .semibold : .regular)
                }
                secondaryLine
            }
            Spacer()
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
    }

    /// The row's second line: idle shows just the chips summary (unchanged
    /// look); starting shows an amber "Starting…" note; serving headlines
    /// the running model (in the teal accent) ahead of the chips summary,
    /// with a "+N" suffix when more than one distinct model is serving on
    /// this box (own model, or from `/serving`'s external entries too).
    @ViewBuilder
    private var secondaryLine: some View {
        switch box.runningState {
        case .idle:
            Text(box.record.chips).font(.caption2).foregroundStyle(.secondary)
        case .starting:
            Text("Starting…").font(.caption2).foregroundStyle(TTTheme.statusStarting)
        case let .serving(primary, others):
            HStack(spacing: 3) {
                Text(primary)
                    .font(.caption)
                    .fontWeight(.medium)
                    .foregroundStyle(TTTheme.teal)
                    .lineLimit(1)
                if others > 0 {
                    Text("+\(others)")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                Text("· \(box.record.chips)")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
    }
}

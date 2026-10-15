import SwiftUI

/// tt-toplike's views, on the phone: the real tt-toplike, run on the box
/// against its own telemetry and streamed here (`ToplikeTerminal`). The names
/// and order follow tt-toplike's own `v` cycle.
enum ToplikeMode: String, CaseIterable, Identifiable {
    case arcade, starfield, castle, flow, defrag, inference, insights, hivemind

    var id: String { rawValue }

    var title: String {
        switch self {
        case .castle: return "memory castle"
        case .flow: return "memory flow"
        default: return rawValue
        }
    }

    /// What each view shows, in tt-toplike's own words (its README / `l` legend).
    var legend: String {
        switch self {
        case .arcade:
            return "Everything at once: starfield, memory castle + defrag, memory flow. The @ hero walks the box's real readings: across = current, up = power, colour = temperature."
        case .starfield:
            return "Each chip's Tensix cores as stars, responding to the chip's power, current and temperature."
        case .castle:
            return "The memory hierarchy, DDR → L2 → L1 → Tensix, per chip."
        case .flow:
            return "The GDDR channels in motion, full screen."
        case .defrag:
            return "One row per GDDR channel: blocks fill as a model's weights load, glow while it runs, and drain when it stops."
        case .inference:
            return "The inference server monitor: what's serving, its throughput, queue and cache, straight from the server's metrics."
        case .insights:
            return "Each chip in plain words, with the processes holding it."
        case .hivemind:
            return "Who's touching which chip: a source × device board and a live feed."
        }
    }
}

/// The black, scanlined panel every canvas mode sits in.
struct ToplikePanel<Content: View>: View {
    var height: CGFloat = 330
    @ViewBuilder var content: Content

    var body: some View {
        content
            .frame(height: height)
            .frame(maxWidth: .infinity)
            .background(Color.black)
            .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
            .overlay(Scanlines().clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous)))
            .overlay(RoundedRectangle(cornerRadius: 18, style: .continuous)
                .strokeBorder(Toplike.magenta.opacity(0.35), lineWidth: 1))
            .shadow(color: Toplike.magenta.opacity(0.25), radius: 18)
    }
}

extension ByteFormat {
    /// "8.1G", "512M".
    static func short(_ bytes: Double) -> String {
        bytes >= 1e9 ? String(format: "%.1fG", bytes / 1e9) : String(format: "%.0fM", bytes / 1e6)
    }
}

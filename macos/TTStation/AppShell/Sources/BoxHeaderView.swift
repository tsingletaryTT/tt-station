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
        TTTheme.statusColor(
            isServing: box.status?.isServing == true,
            isStarting: box.starting,
            hasError: box.errorText != nil
        )
    }

    private var statusHelp: String {
        if let powerText = powerStatusText { return powerText }
        if box.errorText != nil { return "Error — see details below." }
        if box.starting { return "Starting a model…" }
        if box.status?.isServing == true { return "Serving a model" }
        return "Idle"
    }

    /// While a power op is in flight (Task 7), this takes over the header's
    /// status line in place of the normal reachable/serving text — the
    /// ensuing connection drop (or, for `.waking`, the box still being down
    /// until it answers again) is expected, not an error, and the copy here
    /// says so instead of `refresh()`'s generic timeout/network message.
    private var powerStatusText: String? {
        switch box.powerState {
        case .suspending: return "Suspending…"
        case .rebooting: return "Rebooting…"
        case .poweredOff: return "Powered off — Wake to bring it back"
        case .waking: return "Waking…"
        case nil: return nil
        }
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            // Product artwork for a recognized chassis (QuietBox 2 → p300x2).
            // Baseline-aligned row, so pin the image to the top rather than
            // letting it ride the text baseline.
            if let art = DeviceArtwork.assetName(forMesh: box.record.deviceMesh) {
                Image(art)
                    .resizable()
                    .scaledToFit()
                    .frame(width: 48, height: 48)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .alignmentGuide(.firstTextBaseline) { $0[.top] }
                    .help("Tenstorrent QuietBox 2")
                    .accessibilityLabel("Tenstorrent QuietBox 2")
            }
            Circle().fill(statusColor).frame(width: 9, height: 9).help(statusHelp)
            VStack(alignment: .leading, spacing: 2) {
                Text(box.record.name).font(.title3.weight(.semibold))
                // The power-op line (e.g. "Rebooting…") takes over from the
                // normal chips summary while `powerState` is set — see
                // `powerStatusText`.
                Text(powerStatusText ?? box.record.chips)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
            // Secondary chip: the box's active config profile, if the
            // unauthed `/config` read (`box.config`, see `BoxViewModel`)
            // returned one. Deliberately just a label — no picker — the box
            // panel owns switching profiles, this app only ever shows the
            // resolved result. Lower-contrast than the mesh badge below so
            // the mesh (hardware identity) stays the visually primary chip.
            if let profile = box.config?.activeProfile {
                Text(profile)
                    .font(.caption2)
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(Color.secondary.opacity(0.15))
                    .foregroundStyle(.secondary)
                    .clipShape(Capsule())
                    .help("Active config profile (switch it from the box panel)")
            }
            if let mesh = box.record.deviceMesh, !mesh.isEmpty {
                Text(mesh.uppercased())
                    .font(TTTheme.mono)
                    .padding(.horizontal, 8).padding(.vertical, 3)
                    .background(TTTheme.teal.opacity(0.18))
                    .foregroundStyle(TTTheme.teal)
                    .clipShape(Capsule())
                    .help("Device mesh reported by this box")
            }
            // Power controls only make sense once paired (the underlying
            // `/power`/`/wake` calls are authed / need a known MAC) — an
            // unpaired box just doesn't show the menu, same gating
            // `BoxDetailView` uses for Run/Stop.
            if box.isPaired {
                PowerMenuView(box: box)
            }
        }
    }
}

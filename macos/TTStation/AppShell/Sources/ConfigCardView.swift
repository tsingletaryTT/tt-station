import SwiftUI
import TTStationKit

/// Read-only display of the box's resolved serving configuration — profile,
/// backend, `serving_host:serving_port`, image, device — sourced from the
/// unauthed `tt --json config` read (`BoxViewModel.config`, refreshed
/// alongside `serving`/`status` in `BoxViewModel.refresh()`).
///
/// **Deliberately read-only:** per the agentd-config-profiles spec, the box
/// panel (physically at the box) owns profile switching — this card exists
/// only so the Mac can show *what will this box serve with* at a glance.
/// There is no picker, toggle, or write path here, and there should never be
/// one; adding profile-switching controls to this view would contradict the
/// spec's division of ownership.
///
/// The composing view (`BoxWorkspaceView`) is responsible for guarding this
/// behind `box.config != nil` — an older agent, or a `/config` read that
/// failed (never fatal, see `BoxViewModel.refresh()`'s `try?`), simply omits
/// the card rather than this view having to render an empty/broken state.
struct ConfigCardView: View {
    let config: BoxConfig

    var body: some View {
        CardContainer(title: "Config") {
            VStack(alignment: .leading, spacing: 4) {
                profileLine
                labeled("Backend", config.backend)
                HStack(spacing: 4) {
                    Text("Serving:").font(.caption).foregroundStyle(.secondary)
                    Text("\(config.servingHost):\(config.servingPort)").font(TTTheme.mono)
                }
                if let image = config.servingImage {
                    HStack(spacing: 4) {
                        Text("Image:").font(.caption).foregroundStyle(.secondary)
                        Text(image)
                            .font(TTTheme.mono)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                HStack(spacing: 4) {
                    Text("Device:").font(.caption).foregroundStyle(.secondary)
                    Text(config.ttDevice ?? "auto-detected").font(TTTheme.mono)
                }
            }
        }
    }

    /// "Profile: <active>" plus a secondary "· others: <comma-joined>" when
    /// more than one profile is available on the box — or "Profile: none"
    /// when the box has no config file / no active profile at all (still
    /// worth a line: it tells the owner this box is running whatever
    /// hardcoded defaults the agent falls back to, not a named profile).
    @ViewBuilder
    private var profileLine: some View {
        if let active = config.activeProfile {
            HStack(spacing: 4) {
                Text("Profile:").font(.caption).foregroundStyle(.secondary)
                Text(active).font(.caption).foregroundStyle(TTTheme.teal)
                let others = config.availableProfiles.filter { $0 != active }
                if !others.isEmpty {
                    Text("· others: \(others.joined(separator: ", "))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
            }
        } else {
            labeled("Profile", "none")
        }
    }

    private func labeled(_ label: String, _ value: String) -> some View {
        HStack(spacing: 4) {
            Text("\(label):").font(.caption).foregroundStyle(.secondary)
            Text(value).font(.caption)
        }
    }
}

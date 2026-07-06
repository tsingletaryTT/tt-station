import SwiftUI
import TTStationKit

/// Per-device telemetry tiles (temp / power / aiclk) for the box currently
/// shown in the workspace, backed by the agent's live `/telemetry` WebSocket.
///
/// **Ownership:** reads from the box's own `box.telemetry` (a ref-counted
/// subscription on `BoxViewModel`, added in Task 3) rather than owning a
/// `TelemetryService` locally. `subscribeTelemetry()`/`unsubscribeTelemetry()`
/// increment/decrement a shared counter that starts exactly one lite socket
/// per box regardless of how many views (this strip + the popover's temp
/// chip) are observing it — so the window and the popover no longer open two
/// separate sockets to the same agent.
struct DeviceStripView: View {
    let box: BoxViewModel
    var launcher: LaunchController

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            content
            Spacer(minLength: 0)
            Button {
                Task { await launcher.openTTToplike(host: box.record.host, ctrlPort: box.record.ctrlPort) }
            } label: {
                Label("Open tt-toplike ↗", systemImage: "waveform.path.ecg")
            }
            .controlSize(.small)
            .disabled(launcher.isLaunchingToplike)
            .help("Open tt-toplike showing this box's live telemetry.")
        }
        .onAppear { box.subscribeTelemetry() }
        .onDisappear { box.unsubscribeTelemetry() }
    }

    /// Device tiles when a live snapshot has readings, else a quiet
    /// "unavailable" note — covers both `.failed` (socket error) and the
    /// pre-first-frame `.connecting`/`.idle` states with one condition,
    /// since there's nothing more specific to say to the user in either case.
    @ViewBuilder
    private var content: some View {
        if let devices = box.telemetry.snapshot?.devices, !devices.isEmpty {
            HStack(spacing: 8) {
                ForEach(devices, id: \.index) { deviceTile($0) }
            }
        } else {
            Text("telemetry unavailable").font(.caption).foregroundStyle(.secondary)
        }
    }

    private func deviceTile(_ device: DeviceReading) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("dev\(device.index)").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
            if let temp = device.tempC {
                Text(String(format: "%.0f°C", temp))
                    .font(TTTheme.mono)
                    .foregroundStyle(TTTheme.tempColor(temp))
                // Compact horizontal meter, 0-100°C scale, clamped so an
                // out-of-range reading never over/under-draws the bar.
                GeometryReader { geo in
                    RoundedRectangle(cornerRadius: 2)
                        .fill(Color.secondary.opacity(0.15))
                        .overlay(alignment: .leading) {
                            RoundedRectangle(cornerRadius: 2)
                                .fill(TTTheme.tempColor(temp))
                                .frame(width: geo.size.width * min(max(temp / 100, 0), 1))
                        }
                }
                .frame(height: 4)
            }
            HStack(spacing: 6) {
                if let power = device.powerW {
                    Text(String(format: "%.0fW", power)).font(.caption2).foregroundStyle(.secondary)
                }
                if let aiclk = device.aiclkMHz {
                    Text(String(format: "%.0fMHz", aiclk)).font(.caption2).foregroundStyle(.secondary)
                }
            }
        }
        .padding(6)
        .frame(minWidth: 64, alignment: .leading)
        .background(Color.secondary.opacity(0.08))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }
}

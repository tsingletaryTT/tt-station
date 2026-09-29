import SwiftUI
import TTStationKit

/// Detail pane for "This Mac": a Tenstorrent card attached to this machine (a Thunderbolt
/// enclosure), shown with exactly what can be known about it TODAY, with no driver loaded.
///
/// Everything comes from one `tt-station --json local` read (`AppModel.localReport`):
/// * **identity + link + memory**, from the macOS IORegistry;
/// * **right-sized models**, delegated to the official `tt` CLI (`tt model list --hw <config>`);
/// * an explicit **"not yet"** list (`LocalCapability`) so the pane never implies telemetry or
///   serving work when they don't. Those need the DriverKit extension in `macos/TTStationDriver/`.
///
/// Read-only by design: there is nothing to Run here yet, so there's no RunStopBar either.
struct LocalWorkspaceView: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let report = model.localReport, report.hasCards {
                ForEach(report.cards) { card in
                    CardContainer(title: "This Mac") { header(card, mesh: report.deviceMesh) }
                    CardContainer(title: "What macOS can see (no driver)") { facts(card) }
                }
                CardContainer(title: "Right-sized for this device") { rightSized(report) }
                CardContainer(title: "Status") { capabilities(report) }
            } else {
                ContentUnavailableView("No Tenstorrent card attached", systemImage: "cpu",
                                       description: Text(model.localError ?? "Connect an enclosure and refresh."))
            }
        }
    }

    // MARK: Cards

    private func header(_ card: LocalCard, mesh: String?) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "cpu").foregroundStyle(TTTheme.teal)
            Text(card.boardLabel).font(.title2.weight(.semibold))
            Text(card.chipLabel).foregroundStyle(.secondary)
            if card.tunnelled {
                Label("Thunderbolt", systemImage: "bolt.horizontal").font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if let mesh {
                // Same badge idea as a box's device-mesh badge: the official device-config name.
                Text(mesh.uppercased())
                    .font(.caption.monospaced().weight(.semibold))
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(TTTheme.teal.opacity(0.18), in: Capsule())
                    .help("Device config used for right-sizing (tt model list --hw \(mesh))")
            }
        }
    }

    private func facts(_ card: LocalCard) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            row("PCI ID", card.pciID, mono: true)
            row("Board", "\(card.boardLabel) (subsystem \(card.subsystemLabel))")
            row("Link", card.linkLabel)
            row("Location", card.location ?? "?", mono: true)
            row("Memory", card.memorySummary, mono: true)
            if !card.hasLargeWindows {
                Text("No 4 GiB windows over this link (BAR4 wasn't assigned through the Thunderbolt bridge). The card still works through its 2 MiB windows.")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .textSelection(.enabled)
    }

    @ViewBuilder
    private func rightSized(_ report: LocalReport) -> some View {
        if let models = report.models {
            VStack(alignment: .leading, spacing: 6) {
                if models.isEmpty {
                    Text("The official catalog lists no models for \(report.deviceMesh ?? "this device").")
                        .font(.caption).foregroundStyle(.secondary)
                }
                ForEach(models) { m in
                    HStack(spacing: 8) {
                        Text(m.name).font(.body.weight(.medium))
                        if let t = m.modelType { Text(t).font(.caption).foregroundStyle(.secondary) }
                        Spacer()
                        if let ctx = m.contextLabel { Text(ctx).font(.caption).foregroundStyle(.secondary) }
                        if let status = m.status { statusBadge(status) }
                    }
                }
                if let tt = report.officialTT {
                    Text("From the official tt \(tt.version): `tt model list --hw \(report.deviceMesh ?? "?")`")
                        .font(.caption2).foregroundStyle(.secondary)
                }
            }
        } else {
            VStack(alignment: .leading, spacing: 4) {
                Text(LocalizedStringKey(report.modelsError ?? "Right-sizing didn't run."))
                    .font(.caption).foregroundStyle(.orange)
                    .textSelection(.enabled)
                Text("tt-station asks Tenstorrent's official `tt` CLI. Install it with `uv tool install tenstorrent`.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func capabilities(_ report: LocalReport) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(LocalCapability.list(for: report)) { cap in
                switch cap {
                case let .detected(text):
                    capabilityRow("checkmark.circle.fill", .green, "Detected", text)
                case let .rightSized(text):
                    capabilityRow("checkmark.circle.fill", .green, "Right-sized", text)
                case let .rightSizingUnavailable(text):
                    capabilityRow("exclamationmark.triangle.fill", .orange, "Right-sizing unavailable", text)
                case let .notYet(title, reason):
                    capabilityRow("clock", .secondary, "\(title): not yet", reason)
                }
            }
        }
    }

    // MARK: Pieces

    private func row(_ label: String, _ value: String, mono: Bool = false) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            Text("\(label):").font(.caption).foregroundStyle(.secondary).frame(width: 70, alignment: .leading)
            Text(value).font(mono ? TTTheme.mono : .caption)
        }
    }

    private func statusBadge(_ status: String) -> some View {
        let color: Color = status == "COMPLETE" ? .green : status == "EXPERIMENTAL" ? .orange : .secondary
        return Text(status.capitalized)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 6).padding(.vertical, 2)
            .foregroundStyle(color)
            .background(color.opacity(0.15), in: Capsule())
    }

    private func capabilityRow(_ symbol: String, _ color: Color, _ title: String, _ detail: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: symbol).foregroundStyle(color)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.caption.weight(.semibold))
                // `LocalizedStringKey` so inline `code` in the reasons renders as code: a plain
                // `String` isn't parsed as Markdown (first look showed literal backticks).
                Text(LocalizedStringKey(detail)).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

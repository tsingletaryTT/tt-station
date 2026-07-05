import SwiftUI
import TTStationKit

/// Terminal / tt-toplike / VS Code as first-class buttons with SF Symbol
/// icons, one-line subtitles, and their own per-action spinner + error —
/// extracted from the "Workbench" row in the pre-card `BoxWorkspaceView`
/// (Task 12's `LaunchController` workbench methods; VS Code now also
/// installs the tt-vscode-toolkit extension as part of that launch).
struct WorkbenchCardView: View {
    let box: BoxViewModel
    var launcher: LaunchController

    var body: some View {
        CardContainer(title: "Workbench") {
            VStack(alignment: .leading, spacing: 8) {
                workbenchRow(
                    title: "Terminal",
                    subtitle: "SSH into this box.",
                    systemImage: "terminal",
                    isLaunching: launcher.isLaunchingTerminal,
                    error: launcher.terminalError
                ) {
                    Task { await launcher.openTerminalSSH(host: box.record.host) }
                }
                workbenchRow(
                    title: "tt-toplike",
                    subtitle: "Live telemetry for this box, in a terminal.",
                    systemImage: "waveform.path.ecg",
                    isLaunching: launcher.isLaunchingToplike,
                    error: launcher.toplikeError
                ) {
                    Task { await launcher.openTTToplike(host: box.record.host, ctrlPort: box.record.ctrlPort) }
                }
                workbenchRow(
                    title: "VS Code",
                    subtitle: "Remote-SSH window (installs tt-vscode-toolkit).",
                    systemImage: "chevron.left.forwardslash.chevron.right",
                    isLaunching: launcher.isLaunchingVSCode,
                    error: launcher.vscodeError
                ) {
                    Task { await launcher.openVSCode(host: box.record.host) }
                }
            }
        }
    }

    @ViewBuilder
    private func workbenchRow(
        title: String,
        subtitle: String,
        systemImage: String,
        isLaunching: Bool,
        error: String?,
        action: @escaping () -> Void
    ) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Button(action: action) {
                    Label(title, systemImage: systemImage)
                }
                .disabled(isLaunching)
                Text(subtitle).font(.caption).foregroundStyle(.secondary)
                Spacer(minLength: 0)
                if isLaunching { ProgressView().scaleEffect(0.6) }
            }
            .controlSize(.small)
            if let error {
                Text(error).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
        }
    }
}

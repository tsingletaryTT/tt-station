import SwiftUI
import TTStationKit

/// Pinned action bar owning the serving/endpoint display and Run/Stop/Cancel.
///
/// Task 2 of the window redesign: this reproduces the Run/Stop/Cancel +
/// "Serving <model>"/endpoint block that lives in `BoxWorkspaceView.modelBody`
/// today, verbatim in behavior, so it can be relocated outside the scrolling
/// card stack (Task 3) and stay visible regardless of scroll position. This
/// view is purely read-only over `BoxViewModel` — no new `@State`, no new
/// `BoxViewModel` methods; it calls the exact same `run()`/`stop()`/
/// `cancelStart()`/`canStopOrCancel` the model card uses today.
///
/// Not yet wired into the window (Task 3) — this task only creates the view
/// and confirms it compiles standalone against `BoxViewModel`/`TTTheme`.
struct RunStopBar: View {
    @Bindable var box: BoxViewModel

    /// Serving state for the status dot + model name, mirroring the
    /// precedence `BoxHeaderView`/`BoxSidebarView` already use: an active
    /// `endpoint` (this app's own `run()`) or the agent-reported
    /// `status.isServing` (something else serving on the box) both count.
    private var isServing: Bool {
        box.endpoint != nil || (box.status?.isServing ?? false)
    }

    /// The model name to show next to the status dot: the actively-serving
    /// model if there is one, else whatever's selected in the browser, else a
    /// placeholder so the bar never renders an empty label.
    private var displayModel: String {
        box.endpoint?.model ?? box.selectedModel ?? "No model selected"
    }

    var body: some View {
        VStack(spacing: 0) {
            Divider()

            content
                .padding(10)
        }
        .background(.regularMaterial)
    }

    @ViewBuilder
    private var content: some View {
        VStack(alignment: .leading, spacing: 4) {
            // Row 1: status dot + model name, Run/Stop/Cancel trailing.
            HStack {
                Image(systemName: "circle.fill")
                    .font(.system(size: 7))
                    .foregroundStyle(TTTheme.statusColor(isServing: isServing, isStarting: box.starting))
                Text(displayModel)
                    .font(.callout.weight(.medium))
                    .lineLimit(1)
                    .truncationMode(.middle)

                Spacer()

                if box.inFlight { ProgressView().scaleEffect(0.6) }

                // Run is the primary action; Stop/Cancel is a secondary,
                // destructive one. Run is gated by `inFlight` (stays disabled
                // through a load); Stop/Cancel is gated by `canStopOrCancel`
                // so it stays live during a load (to cancel it) — the only
                // real way to abort a load is to tell the agent to `stop`,
                // which makes the in-flight `run()` fail fast.
                Button { Task { await box.run() } } label: {
                    Label("Run", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .disabled(box.selectedModel == nil || box.inFlight)
                .help("Start serving the selected model on this box.")

                if box.starting {
                    Button(role: .destructive) { Task { await box.cancelStart() } } label: {
                        Label("Cancel", systemImage: "xmark.circle.fill")
                    }
                    .buttonStyle(.bordered)
                    .disabled(!box.canStopOrCancel)
                    .help("Cancel the in-progress model load.")
                } else {
                    Button(role: .destructive) { Task { await box.stop() } } label: {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .buttonStyle(.bordered)
                    .disabled(!box.canStopOrCancel)
                    .help("Stop the model currently serving on this box.")
                }
            }

            // Row 2: a single context line, whichever is relevant right now —
            // canceling takes precedence over starting (an in-flight cancel
            // means a load was already underway), which takes precedence over
            // the endpoint line (no endpoint exists yet while starting).
            if box.cancelling {
                HStack(spacing: 6) {
                    ProgressView().scaleEffect(0.6)
                    Text("Canceling…").font(.caption).foregroundStyle(.secondary)
                }
            } else if box.starting {
                HStack(spacing: 6) {
                    ProgressView().scaleEffect(0.6)
                    Text("Starting \(box.selectedModel ?? "model")… (first run can take a few minutes)")
                        .font(.caption).foregroundStyle(.secondary)
                }
            } else if let ep = box.endpoint {
                HStack(spacing: 4) {
                    Text(ep.baseURL).font(TTTheme.mono).lineLimit(1).truncationMode(.middle)
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(ep.baseURL, forType: .string)
                    } label: {
                        Image(systemName: "doc.on.doc")
                    }
                    .buttonStyle(.borderless)
                    .help("Copy endpoint URL")
                }
            }
        }
        .controlSize(.small)
    }
}

import SwiftUI
import TTStationKit

/// The `box.serving` list — every `/v1` endpoint currently live on this box,
/// including containers this box's agent didn't launch itself (marked with
/// the `external` badge, e.g. tt-studio) — extracted from the inline
/// "Serving" block in the pre-card `BoxWorkspaceView`.
///
/// Takes the entries directly (not the box) so it stays a pure display
/// card: the composing view decides when to show it.
///
/// **Empty-guard (window redesign Task 5):** `BoxWorkspaceView` now passes
/// `box.serving` filtered to exclude whatever endpoint the pinned
/// `RunStopBar` already shows, so this card only ever adds entries the bar
/// doesn't cover (an external tt-studio container, etc). Once that agent
/// endpoint is filtered out, an idle/solo-agent box would otherwise show an
/// empty "Nothing is currently serving..." card directly under a bar that's
/// already saying exactly that — pure noise. So this view now renders
/// nothing at all when `entries` is empty, instead of an empty-state
/// message; the call site no longer needs its own `if !entries.isEmpty`
/// wrapper because `EmptyView` costs nothing to lay out.
struct ServingCardView: View {
    let entries: [ServingEntry]

    var body: some View {
        if entries.isEmpty {
            EmptyView()
        } else {
            CardContainer(title: "Serving") {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(entries, id: \.hostPort) { entry in
                        VStack(alignment: .leading, spacing: 2) {
                            HStack(spacing: 4) {
                                Text(entry.model).font(.caption).lineLimit(1).truncationMode(.middle)
                                if entry.source == "external" {
                                    Text("external")
                                        .font(.system(size: 9, weight: .semibold))
                                        .padding(.horizontal, 4).padding(.vertical, 1)
                                        .background(Color.secondary.opacity(0.2))
                                        .clipShape(Capsule())
                                }
                            }
                            HStack {
                                Text(entry.baseURL)
                                    .font(.system(.caption, design: .monospaced))
                                    .lineLimit(1).truncationMode(.middle)
                                Button {
                                    NSPasteboard.general.clearContents()
                                    NSPasteboard.general.setString(entry.baseURL, forType: .string)
                                } label: {
                                    Image(systemName: "doc.on.doc")
                                }
                                .buttonStyle(.borderless)
                                .help("Copy endpoint URL")
                            }
                        }
                    }
                }
            }
        }
    }
}

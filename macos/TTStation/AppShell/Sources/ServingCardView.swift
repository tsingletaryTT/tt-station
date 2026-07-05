import SwiftUI
import TTStationKit

/// The `box.serving` list — every `/v1` endpoint currently live on this box,
/// including containers this box's agent didn't launch itself (marked with
/// the `external` badge, e.g. tt-studio) — extracted from the inline
/// "Serving" block in the pre-card `BoxWorkspaceView`.
///
/// Takes the entries directly (not the box) so it stays a pure display
/// card: the composing view decides when to show it (Task 14 will likely
/// gate on `!box.serving.isEmpty`, matching the extracted block's own
/// `if !box.serving.isEmpty` guard) and this view doesn't have to duplicate
/// that policy.
struct ServingCardView: View {
    let entries: [ServingEntry]

    var body: some View {
        CardContainer(title: "Serving") {
            if entries.isEmpty {
                Text("Nothing is currently serving on this box.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
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

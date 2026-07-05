import SwiftUI

/// Reusable titled card chrome shared by every detail-pane card in the
/// control-room window (`BoxHeaderView`, `DeviceStripView`,
/// `ModelBrowserView`, `ConnectCardView`, `WorkbenchCardView`,
/// `ServingCardView`) so they read as one visual system instead of each
/// reinventing background/border/title styling.
///
/// `@ViewBuilder let content: () -> Content` (rather than a plain stored
/// closure) is what lets callers write `CardContainer(title: "Connect") {
/// ... }` with multiple/optional statements in the trailing closure, exactly
/// like `VStack` or `Group` — Swift synthesizes the memberwise init and
/// propagates the `@ViewBuilder` attribute onto the `content:` parameter.
struct CardContainer<Content: View>: View {
    let title: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(TTTheme.teal)
            content()
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(Color.secondary.opacity(0.15), lineWidth: 1)
        )
    }
}

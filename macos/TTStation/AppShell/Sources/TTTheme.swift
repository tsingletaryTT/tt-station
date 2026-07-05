import SwiftUI

/// Tenstorrent brand theme primitives for the macOS app.
///
/// Plain value namespace — no state, no side effects. Card views (Task 13) and the
/// app root (Task 14) read these constants/helpers directly; this file must not grow
/// view modifiers or components (those belong alongside the views that need them).
///
/// Palette source: the "editor/IDE surface" variant documented in the user's global
/// CLAUDE.md brand-colors section — teal `#4FD1C5` on deep blue-gray `#0F2A35` — which
/// is the variant `tt-vscode-toolkit` uses, distinct from the docs-site theme's
/// forest-teal identity. Chosen here because this is an editor/tool-adjacent developer
/// surface, not the public docs site.
enum TTTheme {
    /// Editor-variant accent: bright teal, used for links/highlights/serving state.
    static let teal = Color(red: 0x4F / 255, green: 0xD1 / 255, blue: 0xC5 / 255)

    /// Editor-variant deep base: near-black blue-gray, used for inverse/background fills.
    static let ground = Color(red: 0x0F / 255, green: 0x2A / 255, blue: 0x35 / 255)

    /// Machine-string font (endpoints, hostnames, token fragments, telemetry values).
    ///
    /// Uses the system monospaced font rather than Berkeley Mono: Berkeley Mono is a
    /// paid, manually-installed font with no guarantee it's present on any given Mac,
    /// and `Font.custom` has no built-in "does this font exist" fallback — you'd have
    /// to probe `NSFontManager` yourself to avoid silently falling back to the system
    /// UI font at the wrong size/weight. `Font.system(.caption, design: .monospaced)`
    /// is always available, scales with Dynamic Type, and is the safer default for a
    /// tool that has to look right on every box owner's machine out of the box.
    static let mono = Font.system(.caption, design: .monospaced)

    /// Temperature ramp for per-device readings (device strip, Task 13).
    ///
    /// Bucketed with linear interpolation across three anchor stops rather than a
    /// continuous HSB sweep, so the semantics stay legible at a glance:
    ///   - `< 55°C`  → `teal`   (cool / nominal)
    ///   - `~70°C`   → yellow  (warm)
    ///   - `>= 85°C` → red     (hot)
    /// Values between anchors interpolate linearly in RGB space so the ramp has no
    /// visible banding; values outside `[55, 85]` clamp to the nearest anchor color
    /// (no extrapolation past teal or past red).
    static func tempColor(_ c: Double) -> Color {
        let cool = teal
        let warm = Color.yellow
        let hot = Color.red

        let coolAnchor = 55.0
        let warmAnchor = 70.0
        let hotAnchor = 85.0

        if c <= coolAnchor { return cool }
        if c >= hotAnchor { return hot }
        if c <= warmAnchor {
            let t = (c - coolAnchor) / (warmAnchor - coolAnchor)
            return interpolate(cool, warm, t)
        }
        let t = (c - warmAnchor) / (hotAnchor - warmAnchor)
        return interpolate(warm, hot, t)
    }

    // MARK: - Status-dot colors

    /// Serving (green), starting (amber), idle (gray), error (red) — the four
    /// states a box's status dot can show, shared by `BoxHeaderView`,
    /// `BoxRowView`, `BoxSidebarView`, and the popover so the same state
    /// always reads as the same color everywhere in the app. Plain semaphore
    /// colors rather than the teal/ground editor accent above: these signal
    /// live device state, not brand identity, and system green/orange/gray/red
    /// already carry the right meaning in both light and dark mode.
    static let statusServing = Color.green
    static let statusStarting = Color.orange
    static let statusIdle = Color.gray
    static let statusError = Color.red

    /// Priority-ordered status-dot color: `hasError` > `isStarting` >
    /// `isServing` > idle. Centralizes the precedence every call site
    /// (header/row/sidebar/popover) was otherwise duplicating ad hoc.
    static func statusColor(isServing: Bool, isStarting: Bool, hasError: Bool = false) -> Color {
        if hasError { return statusError }
        if isStarting { return statusStarting }
        return isServing ? statusServing : statusIdle
    }

    /// Linear RGB interpolation between two colors via `NSColor`'s sRGB components.
    /// Alpha is left at the destination's (both anchors are opaque, so this is 1.0
    /// throughout the ramp).
    private static func interpolate(_ from: Color, _ to: Color, _ t: Double) -> Color {
        let clampedT = min(max(t, 0), 1)
        let fromComponents = NSColor(from).usingColorSpace(.deviceRGB) ?? NSColor(from)
        let toComponents = NSColor(to).usingColorSpace(.deviceRGB) ?? NSColor(to)

        let r = fromComponents.redComponent + (toComponents.redComponent - fromComponents.redComponent) * clampedT
        let g = fromComponents.greenComponent + (toComponents.greenComponent - fromComponents.greenComponent) * clampedT
        let b = fromComponents.blueComponent + (toComponents.blueComponent - fromComponents.blueComponent) * clampedT

        return Color(red: r, green: g, blue: b)
    }
}

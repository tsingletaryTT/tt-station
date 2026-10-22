// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// Vesper, Tenstorrent's design system (`@tenstorrent/vesper`), dark theme:
/// forest-teal stone surfaces, a teal accent, the brand mint/pink/purple/yellow,
/// Inter Tight for text and IBM Plex Mono for labels and numbers, and the
/// 24 pt dot grid. Values are Vesper's dark-theme primitives.
enum Vesper {
    static func hex(_ v: UInt32, _ a: Double = 1) -> Color {
        Color(red: Double((v >> 16) & 0xFF) / 255, green: Double((v >> 8) & 0xFF) / 255,
              blue: Double(v & 0xFF) / 255, opacity: a)
    }
    // Stone: surfaces, borders, text.
    static let stone0 = hex(0x050F0E), stone50 = hex(0x0B1817), stone100 = hex(0x13201E)
    static let stone200 = hex(0x1B2826), stone300 = hex(0x233431), stone400 = hex(0x2D4340)
    static let stone500 = hex(0x395651), stone600 = hex(0x5E7D78), stone700 = hex(0x7D9B97)
    static let stone800 = hex(0xB3CCC8), stone900 = hex(0xE7F0EE)
    // Accent and brand.
    static let teal400 = hex(0x0F546B), teal500 = hex(0x136B86), teal600 = hex(0x1A8FB2), teal700 = hex(0x46AFD2)
    static let mint = hex(0x39C6AC), pink = hex(0xE34FAA), purple = hex(0x9C91E3), yellow = hex(0xFFB71B)
    // Status.
    static let success = hex(0x39C6AC), warning = hex(0xFFB35C), error = hex(0xF04B28), info = hex(0x0091FF)
    static let amber = hex(0xFFA333)
    static let staticWhite = hex(0xF1F8F8), staticBlack = hex(0x071614)

    static let textPrimary = stone900, textSecondary = stone800, textTertiary = stone700
    static let borderPrimary = stone400, borderSecondary = stone300

    /// Vesper's radius scale (4, 8, 12, 16, 24, 28).
    enum Radius {
        static let r2: CGFloat = 8, r3: CGFloat = 12, r4: CGFloat = 16, r6: CGFloat = 24, r7: CGFloat = 28
    }
}

/// The app's names for Vesper's roles.
enum Brand {
    /// Text and icon accent (`--vesper-text-accent`, teal-700).
    static let accent = Vesper.teal700
    /// Filled controls: the app tint (`--vesper-border-accent-base`, teal-600).
    static let accentFill = Vesper.teal600
    /// What sits on a filled accent.
    static let onAccent = Vesper.staticWhite
    static let ground = Vesper.stone0
    static let card = Vesper.stone100.opacity(0.55)
    static let stroke = Vesper.borderSecondary

    /// The app-wide backdrop: Vesper's primary surface with its dot grid, and a
    /// faint teal light at the top so Liquid Glass has something to refract.
    static var backdrop: some View {
        ZStack {
            Vesper.stone0
            DotGrid()
            RadialGradient(colors: [Vesper.teal500.opacity(0.28), .clear],
                           center: .top, startRadius: 10, endRadius: 520)
            RadialGradient(colors: [Vesper.mint.opacity(0.06), .clear],
                           center: .bottomTrailing, startRadius: 10, endRadius: 420)
        }
        .ignoresSafeArea()
    }

    /// Chip temperature (°C) → color. Blackhole throttles at 90 °C, so the
    /// ramp turns amber within about 10 °C of that and red just under it.
    static func tempColor(_ celsius: Double) -> Color {
        switch celsius {
        case ..<65: return Vesper.mint
        case ..<78: return Vesper.yellow
        case ..<88: return Vesper.amber
        default: return Vesper.error
        }
    }

    /// `meta-llama/Llama-3.1-8B-Instruct` → `Llama-3.1-8B-Instruct`.
    static func shortModel(_ name: String) -> String {
        name.split(separator: "/").last.map(String.init) ?? name
    }
}

/// Vesper's background dot pattern: a 2 pt square every 24 pt.
struct DotGrid: View {
    var color: Color = Vesper.hex(0x678583, 0.25)

    var body: some View {
        Canvas { gc, size in
            var path = Path()
            var y: CGFloat = 0
            while y < size.height {
                var x: CGFloat = 0
                while x < size.width {
                    path.addRect(CGRect(x: x, y: y, width: 2, height: 2))
                    x += 24
                }
                y += 24
            }
            gc.fill(path, with: .color(color))
        }
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// Vesper's typefaces. Text styles keep Dynamic Type (`relativeTo:`).
extension Font {
    /// Inter Tight at a text style's size; headings in Vesper's 500 weight.
    static func tt(_ style: Font.TextStyle) -> Font {
        let (size, weight): (CGFloat, Font.Weight) = switch style {
        case .largeTitle: (34, .medium)
        case .title: (28, .medium)
        case .title2: (22, .medium)
        case .title3: (20, .medium)
        case .headline: (17, .semibold)
        case .subheadline: (15, .regular)
        case .callout: (16, .regular)
        case .footnote: (13, .regular)
        case .caption: (12, .regular)
        case .caption2: (11, .regular)
        default: (17, .regular)
        }
        return .custom("Inter Tight", size: size, relativeTo: style).weight(weight)
    }

    /// IBM Plex Mono: labels, eyebrows, numbers and code.
    static func ttMono(_ size: CGFloat, _ weight: Font.Weight = .regular, relativeTo style: Font.TextStyle = .caption) -> Font {
        .custom("IBM Plex Mono", size: size, relativeTo: style).weight(weight)
    }
}

/// The Tenstorrent mark, drawn from the three polygons of the brand SVG so it stays
/// crisp at any size and can be filled with any style.
struct TTMark: Shape {
    private static let viewBox = CGRect(x: 8.3, y: 8.8, width: 344, height: 347.6)
    private static let polygons: [[CGPoint]] = {
        let raw: [[(CGFloat, CGFloat)]] = [
            [(266.3, 207.5), (180.3, 257.1), (94.3, 207.5), (94.3, 306.8), (180.3, 356.4), (266.3, 306.8)],
            [(266.3, 108.2), (180.3, 157.8), (94.3, 108.2), (94.3, 207.5), (8.3, 157.8), (8.3, 58.5), (94.3, 8.8)],
            [(266.3, 207.5), (352.3, 157.8), (352.3, 58.5), (266.3, 8.8)],
        ]
        return raw.map { polygon in polygon.map { CGPoint(x: $0.0, y: $0.1) } }
    }()

    func path(in rect: CGRect) -> Path {
        let box = Self.viewBox
        let scale = min(rect.width / box.width, rect.height / box.height)
        let dx = rect.midX - box.width * scale / 2
        let dy = rect.midY - box.height * scale / 2
        var path = Path()
        for polygon in Self.polygons {
            path.addLines(polygon.map {
                CGPoint(x: dx + ($0.x - box.minX) * scale, y: dy + ($0.y - box.minY) * scale)
            })
            path.closeSubpath()
        }
        return path
    }
}

/// Mark + "tenstorrent" wordmark, as in the brand logo.
struct TTLogo: View {
    var height: CGFloat = 16

    var body: some View {
        HStack(spacing: height * 0.5) {
            TTMark()
                .fill(Vesper.textPrimary)
                .frame(width: height * 1.5, height: height * 1.5)
            Image("Wordmark")
                .resizable()
                .scaledToFit()
                .frame(height: height)
                .foregroundStyle(.primary)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Tenstorrent")
    }
}

/// Every card is Liquid Glass over Vesper's stone, with Vesper's hairline
/// border; optionally tinted (red for errors, accent for decisions).
struct GlassCard: ViewModifier {
    var tint: Color?
    var radius: CGFloat = Vesper.Radius.r4
    var interactive = false

    func body(content: Content) -> some View {
        content
            .glassEffect(glass, in: .rect(cornerRadius: radius))
            .overlay(RoundedRectangle(cornerRadius: radius, style: .continuous)
                .strokeBorder(Vesper.borderSecondary.opacity(0.9), lineWidth: 1))
    }

    private var glass: Glass {
        var glass = Glass.regular.tint(tint ?? Vesper.stone100.opacity(0.5))
        if interactive { glass = glass.interactive() }
        return glass
    }
}

extension View {
    /// Keep content to a comfortable reading width on iPad, centered.
    func readableWidth(_ max: CGFloat = 760) -> some View {
        frame(maxWidth: max).frame(maxWidth: .infinity)
    }

    func card(tint: Color? = nil, radius: CGFloat = Vesper.Radius.r4, interactive: Bool = false) -> some View {
        modifier(GlassCard(tint: tint, radius: radius, interactive: interactive))
    }
}

/// A section eyebrow, Vesper style: IBM Plex Mono, upper case, wide tracking.
struct SectionTitle: View {
    let text: String
    var body: some View {
        Text(text.uppercased())
            .font(.ttMono(11, .medium))
            .tracking(1.1)
            .foregroundStyle(Vesper.textTertiary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 4)
    }
}

/// Vesper's Status Indicator: a colored dot (pulsing while in progress) and a label.
struct StatusPill: View {
    enum Kind { case serving, starting, idle, offline, checking }
    let state: Kind
    let text: String

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(color).frame(width: 7, height: 7)
                .phaseAnimator([1.0, 0.35]) { dot, o in dot.opacity(state == .starting ? o : 1) }
                    animation: { _ in .easeInOut(duration: 0.8) }
            Text(text).lineLimit(1)
        }
        .font(.ttMono(11, .medium))
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        .glassEffect(.regular.tint(color.opacity(0.25)), in: .capsule)
        .foregroundStyle(state == .idle || state == .checking ? Vesper.textTertiary : Vesper.textPrimary)
    }

    private var color: Color {
        switch state {
        case .serving: return Vesper.success
        case .starting: return Vesper.amber
        case .idle, .checking: return Vesper.stone600
        case .offline: return Vesper.error
        }
    }
}

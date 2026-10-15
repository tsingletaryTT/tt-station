import SwiftUI

/// The Box tab's tt-toplike mode: the real tt-toplike, streamed from the box
/// (`ToplikeTerminal`), with the box's live readouts underneath.
struct ToplikeDashboard: View {
    let session: BoxSession
    let telemetry: TelemetryFeed
    let logLines: [String]
    @AppStorage("tt.mobile.toplikeMode") private var mode: ToplikeMode = .arcade
    @AppStorage(Temp.key) private var tempUnit = "c"

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            TerminalHeader(session: session, telemetry: telemetry)
            ModeBar(mode: $mode)

            ToplikePanel(height: mode == .arcade ? 460 : 360) {
                ToplikeTerminal(session: session, mode: mode.rawValue)
                    .padding(6)
            }
            Text(mode.legend)
                .font(Toplike.mono(9.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if !telemetry.chips.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(telemetry.chips) { chip in
                        ChipReadout(chip: chip, power: telemetry.powerHistory(chip: chip.index))
                    }
                }
                .padding(14)
                .background(Color.black.opacity(0.75), in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                .overlay(Scanlines().clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous)))
            }

            TerminalLog(lines: logLines)
        }
    }

}

/// `[insights] [starfield] …`: tt-toplike's `v` cycle as tabs.
private struct ModeBar: View {
    @Binding var mode: ToplikeMode

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 6) {
                ForEach(ToplikeMode.allCases) { m in
                    Button {
                        withAnimation(.easeInOut(duration: 0.2)) { mode = m }
                    } label: {
                        Text("[\(m.title)]")
                            .font(Toplike.mono(11, m == mode ? .bold : .regular))
                            .foregroundStyle(m == mode ? Color.black : Toplike.cyan)
                            .padding(.horizontal, 6).padding(.vertical, 4)
                            .background(m == mode ? Toplike.cyan : Color.clear, in: RoundedRectangle(cornerRadius: 4))
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .scrollIndicators(.hidden)
    }
}

// MARK: - Palette & helpers

enum Toplike {
    static let cyan = Color(red: 0.25, green: 0.95, blue: 1.0)
    static let magenta = Color(red: 1.0, green: 0.3, blue: 0.85)
    static let lime = Color(red: 0.6, green: 1.0, blue: 0.35)
    static let amber = Color(red: 1.0, green: 0.72, blue: 0.2)
    /// One color per chip in multi-series charts.
    static let chipPalette: [Color] = [cyan, magenta, lime, amber]

    static func mono(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .ttMono(size, weight)
    }

    /// 0...1: how far the chip is from its own idle (lowest power seen) to its TDP.
    static func activity(_ chip: ChipReading) -> Double {
        guard let p = chip.powerW else { return 0 }
        let idle = chip.idleW ?? 15, top = chip.tdpW ?? 125
        return min(max((p - idle) / max(10, top - idle), 0), 1)
    }

    static func boxActivity(_ chips: [ChipReading]) -> Double {
        chips.isEmpty ? 0 : chips.map(activity).reduce(0, +) / Double(chips.count)
    }

    /// Temperature → neon: cyan when cool, through brand purple and magenta, to amber when hot.
    static func heat(_ celsius: Double?) -> Color {
        let t = min(max(((celsius ?? 40) - 35) / 50, 0), 1)
        switch t {
        case ..<0.33: return cyan
        case ..<0.55: return Brand.accent
        case ..<0.8: return magenta
        default: return amber
        }
    }

    /// `▰▰▰▰▱▱▱▱` for a 0...1 fraction.
    static func blocks(_ fraction: Double, width: Int = 10) -> String {
        let filled = Int((min(max(fraction, 0), 1) * Double(width)).rounded())
        return String(repeating: "▰", count: filled) + String(repeating: "▱", count: width - filled)
    }
}

// MARK: - Header

private struct TerminalHeader: View {
    let session: BoxSession
    let telemetry: TelemetryFeed

    var body: some View {
        TimelineView(.periodic(from: .now, by: 0.5)) { context in
            let cursorOn = Int(context.date.timeIntervalSinceReferenceDate * 2) % 2 == 0
            VStack(alignment: .leading, spacing: 4) {
                (Text("tt-toplike").foregroundStyle(Toplike.magenta)
                 + Text(" ▸ ").foregroundStyle(.secondary)
                 + Text(session.box.name).foregroundStyle(Toplike.cyan)
                 + Text(" ▸ ").foregroundStyle(.secondary)
                 + Text((session.status?.deviceMesh ?? "?").uppercased()).foregroundStyle(Toplike.lime)
                 + Text(cursorOn ? " █" : "  ").foregroundStyle(Toplike.cyan))
                    .font(Toplike.mono(13, .semibold))
                Text(statusLine)
                    .font(Toplike.mono(11))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
    }

    private var statusLine: String {
        let model = session.startingModel.map { "loading \(Brand.shortModel($0))…" }
            ?? session.status?.servingModel.map { "serving \(Brand.shortModel($0))" }
            ?? "idle"
        let watts = telemetry.totalPower.map { "\(Int($0.rounded()))W" } ?? "—W"
        let hot = telemetry.hottest.map { Temp.string($0) + " max" } ?? "—"
        return "\(model) · \(watts) · \(hot)"
    }
}

// MARK: - Readouts

private struct ChipReadout: View {
    let chip: ChipReading
    let power: [Double]

    var body: some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                (Text("CHIP\(chip.index) ").foregroundStyle(.secondary)
                 + Text(chip.tempC.map { String(format: "%5.1f", Temp.value($0)) + Temp.symbol + " " } ?? "  —\(Temp.symbol) ").foregroundStyle(Toplike.heat(chip.tempC))
                 + Text(Toplike.blocks(((chip.tempC ?? 25) - 25) / max((chip.throttleC ?? 90) - 25, 1), width: 8)).foregroundStyle(Toplike.heat(chip.tempC)))
                (Text("  PWR ").foregroundStyle(.secondary)
                 + Text(chip.powerW.map { String(format: "%4.0fW ", $0) } ?? "   —W ").foregroundStyle(Toplike.lime)
                 + Text(Toplike.blocks(Toplike.activity(chip), width: 8)).foregroundStyle(Toplike.lime)
                 + Text(chip.aiclkMHz.map { String(format: " %4.0fMHz", $0) } ?? "").foregroundStyle(.secondary))
            }
            .font(Toplike.mono(11))
            Spacer(minLength: 4)
            Sparkline(values: power, color: Toplike.heat(chip.tempC))
                .frame(width: 84, height: 30)
        }
    }
}

/// A neon line of recent values, with a soft glow.
struct Sparkline: View {
    let values: [Double]
    let color: Color

    var body: some View {
        Canvas { gc, size in
            guard values.count > 1, let lo = values.min(), let hi = values.max() else { return }
            let span = max(hi - lo, 1)
            var path = Path()
            for (i, v) in values.enumerated() {
                let p = CGPoint(x: size.width * CGFloat(i) / CGFloat(values.count - 1),
                                y: size.height * (1 - CGFloat((v - lo) / span)) * 0.85 + size.height * 0.075)
                if i == 0 { path.move(to: p) } else { path.addLine(to: p) }
            }
            gc.addFilter(.shadow(color: color, radius: 3))
            gc.stroke(path, with: .color(color), lineWidth: 1.5)
        }
    }
}

private struct TerminalLog: View {
    let lines: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("$ tail -f serving.log").font(Toplike.mono(11, .semibold)).foregroundStyle(Toplike.magenta)
            if lines.isEmpty {
                Text("(no output yet)").font(Toplike.mono(10)).foregroundStyle(.secondary)
            }
            ForEach(Array(lines.suffix(12).enumerated()), id: \.offset) { _, line in
                Text(line)
                    .font(Toplike.mono(9.5))
                    .foregroundStyle(line.contains("ERROR") || line.contains("Traceback") ? Color.red
                                     : line.contains("WARN") ? Toplike.amber : Toplike.lime.opacity(0.85))
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(14)
        .background(Color.black.opacity(0.8), in: RoundedRectangle(cornerRadius: 18, style: .continuous))
        .overlay(Scanlines().clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous)))
        .textSelection(.enabled)
    }
}

/// CRT scanlines.
struct Scanlines: View {
    var body: some View {
        Canvas { gc, size in
            var y: CGFloat = 0
            while y < size.height {
                gc.fill(Path(CGRect(x: 0, y: y, width: size.width, height: 1)), with: .color(.black.opacity(0.22)))
                y += 3
            }
        }
        .allowsHitTesting(false)
    }
}

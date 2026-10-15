import Charts
import SwiftUI

/// Live numbers and recorded history for the Box tab, from the agent's
/// `/metrics/*` (sampled every 5 s on the box, whether or not anyone watches).
@MainActor
@Observable
final class MetricsModel {
    var range = "1h"
    private(set) var history: MetricsHistory?
    private(set) var now: MetricsNow?
    private(set) var error: String?

    /// Poll while the dashboard is on screen: the live view every 3 s, the
    /// history every 15 s (and whenever the range changes, via `reload`).
    func run(_ session: BoxSession) async {
        var tick = 0
        while !Task.isCancelled {
            do {
                now = try await session.metricsNow()
                if tick % 5 == 0 || history == nil {
                    history = try await session.metricsHistory(range: range)
                }
                error = nil
            } catch {
                self.error = error.localizedDescription
            }
            tick += 1
            try? await Task.sleep(for: .seconds(3))
        }
    }

    func reload(_ session: BoxSession) async {
        history = try? await session.metricsHistory(range: range)
    }
}

// MARK: - Live inference

/// What the served model is doing right now: throughput and latency for chat,
/// generation rate for image/video.
struct InferenceCard: View {
    let now: MetricsNow?
    let kind: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                SectionTitle(text: "Inference")
                if let phase = now?.inference?.phase {
                    Text(phase.capitalized)
                        .font(.tt(.caption2).weight(.semibold))
                        .padding(.horizontal, 8).padding(.vertical, 3)
                        .background((phase == "ready" ? Color.green : .orange).opacity(0.2), in: Capsule())
                }
            }
            if let s = now?.inference?.serving {
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 14) {
                    Stat(value: String(format: "%.1f", s.generationTps), unit: "tok/s", label: "Generating")
                    Stat(value: s.ttftAvgS > 0 ? String(format: "%.0f", s.ttftAvgS * 1000) : "–", unit: "ms",
                         label: "Time to first token")
                    Stat(value: "\(s.requestsRunning)", unit: s.requestsWaiting > 0 ? "+\(s.requestsWaiting) queued" : "",
                         label: "Requests")
                    Stat(value: s.tpotAvgS > 0 ? String(format: "%.0f", s.tpotAvgS * 1000) : "–", unit: "ms/token",
                         label: "Per token")
                }
                Gauge(value: min(max(s.kvCacheUsage, 0), 1)) {
                    Text("KV cache")
                } currentValueLabel: {
                    Text("\(Int(s.kvCacheUsage * 100))%")
                }
                .gaugeStyle(.accessoryLinear)
                .tint(Brand.accent)
            } else if let m = now?.inference?.media {
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 14) {
                    Stat(value: String(format: "%.1f", m.generationsPerMin), unit: "/min",
                         label: kind == "video" ? "Videos" : "Generations")
                    Stat(value: m.durationAvgS > 0 ? String(format: "%.1f", m.durationAvgS) : "–", unit: "s",
                         label: "Average time")
                    Stat(value: "\(m.jobsInProgress)", unit: "", label: "In progress")
                    Stat(value: "\(m.completedTotal)", unit: m.erroredTotal > 0 ? "\(m.erroredTotal) failed" : "",
                         label: "Completed")
                }
            } else {
                Text(now?.inference == nil ? "Nothing is serving." : "Waiting for the first requests…")
                    .font(.tt(.subheadline))
                    .foregroundStyle(.secondary)
            }
        }
        .padding(16)
        .card()
    }
}

private struct Stat: View {
    let value: String
    let unit: String
    let label: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Text(value).font(.system(size: 26, weight: .semibold, design: .rounded).monospacedDigit())
                    .contentTransition(.numericText())
                Text(unit).font(.tt(.caption)).foregroundStyle(.secondary)
            }
            Text(label).font(.tt(.caption)).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .animation(.default, value: value)
    }
}

// MARK: - Charts

/// The recorded history as charts, with a range picker and touch-to-inspect.
struct HistoryCharts: View {
    @Bindable var metrics: MetricsModel
    let session: BoxSession
    @State private var selected: Date?

    private var points: [MetricPoint] { metrics.history?.points ?? [] }

    var body: some View {
        VStack(spacing: 14) {
            HStack {
                SectionTitle(text: "History")
                Picker("Range", selection: $metrics.range) {
                    Text("5m").tag("5m")
                    Text("1h").tag("1h")
                    Text("24h").tag("24h")
                }
                .pickerStyle(.segmented)
                .frame(width: 180)
            }
            .onChange(of: metrics.range) { Task { await metrics.reload(session) } }

            if points.count < 2 {
                Text("History builds up as the box runs (a point every 5 seconds).")
                    .font(.tt(.footnote))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(16)
                    .card()
            } else {
                ChartCard(title: "Temperature", unit: Temp.symbol, selected: selected,
                          readout: readout { p in p.chips.compactMap(\.tempC).max().map { Temp.string($0) + " max" } }) {
                    Chart {
                        ForEach(points) { p in
                            ForEach(Array(p.chips.enumerated()), id: \.offset) { i, chip in
                                if let temp = chip.tempC {
                                    LineMark(x: .value("Time", p.date), y: .value(Temp.symbol, Temp.value(temp)))
                                        .foregroundStyle(by: .value("Chip", "Chip \(i)"))
                                        .interpolationMethod(.monotone)
                                }
                            }
                        }
                        selectionRule
                    }
                    .chartForegroundStyleScale(range: Toplike.chipPalette)
                    .chartYScale(domain: .automatic(includesZero: false))
                }
                ChartCard(title: "Power", unit: "W", selected: selected,
                          readout: readout { $0.totalPower.map { String(format: "%.0f W", $0) } }) {
                    Chart {
                        ForEach(points) { p in
                            if let watts = p.totalPower {
                                AreaMark(x: .value("Time", p.date), y: .value("W", watts))
                                    .foregroundStyle(LinearGradient(colors: [Brand.accent.opacity(0.5), Brand.accent.opacity(0.05)],
                                                                    startPoint: .top, endPoint: .bottom))
                                    .interpolationMethod(.monotone)
                                LineMark(x: .value("Time", p.date), y: .value("W", watts))
                                    .foregroundStyle(Brand.accent)
                                    .interpolationMethod(.monotone)
                            }
                        }
                        selectionRule
                    }
                }
                if points.contains(where: { ($0.genTps ?? 0) > 0 }) {
                    ChartCard(title: "Throughput", unit: "tok/s", selected: selected,
                              readout: readout { $0.genTps.map { String(format: "%.1f tok/s", $0) } }) {
                        Chart {
                            ForEach(points) { p in
                                BarMark(x: .value("Time", p.date), y: .value("tok/s", p.genTps ?? 0))
                                    .foregroundStyle(Color.green.gradient)
                            }
                            selectionRule
                        }
                    }
                }
                ChartCard(title: "AI clock", unit: "MHz", selected: selected,
                          readout: readout { p in
                              let c = p.chips.compactMap(\.aiclkMHz)
                              return c.isEmpty ? nil : String(format: "%.0f MHz", c.reduce(0, +) / Double(c.count))
                          }) {
                    Chart {
                        ForEach(points) { p in
                            let clocks = p.chips.compactMap(\.aiclkMHz)
                            if !clocks.isEmpty {
                                LineMark(x: .value("Time", p.date), y: .value("MHz", clocks.reduce(0, +) / Double(clocks.count)))
                                    .foregroundStyle(Color.cyan)
                                    .interpolationMethod(.stepCenter)
                            }
                        }
                        selectionRule
                    }
                    .chartYScale(domain: .automatic(includesZero: false))
                }
            }
        }
        .environment(\.chartSelection, $selected)
    }

    @ChartContentBuilder private var selectionRule: some ChartContent {
        if let selected {
            RuleMark(x: .value("Selected", selected))
                .foregroundStyle(Color.white.opacity(0.35))
                .lineStyle(StrokeStyle(lineWidth: 1, dash: [3, 3]))
        }
    }

    /// The value nearest the selected time, formatted, else the latest.
    private func readout(_ format: (MetricPoint) -> String?) -> String? {
        guard !points.isEmpty else { return nil }
        let target = selected ?? points.last!.date
        let nearest = points.min { abs($0.date.timeIntervalSince(target)) < abs($1.date.timeIntervalSince(target)) }
        return nearest.flatMap(format)
    }
}

/// Shares the touch-selected time between every chart.
private struct ChartSelectionKey: EnvironmentKey {
    static let defaultValue: Binding<Date?> = .constant(nil)
}

extension EnvironmentValues {
    var chartSelection: Binding<Date?> {
        get { self[ChartSelectionKey.self] }
        set { self[ChartSelectionKey.self] = newValue }
    }
}

private struct ChartCard<ChartBody: View>: View {
    let title: String
    let unit: String
    let selected: Date?
    let readout: String?
    @ViewBuilder let chart: () -> ChartBody
    @Environment(\.chartSelection) private var selection

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.tt(.headline))
                Spacer()
                if let readout {
                    Text(readout).font(.tt(.subheadline).monospacedDigit()).foregroundStyle(.secondary)
                }
            }
            if let selected {
                Text(selected.formatted(date: .omitted, time: .standard))
                    .font(.tt(.caption2)).foregroundStyle(.tertiary)
            }
            chart()
                .chartXSelection(value: selection)
                .chartXAxis { AxisMarks(values: .automatic(desiredCount: 4)) }
                .frame(height: 150)
        }
        .padding(16)
        .card()
    }
}

// MARK: - Energy & usage

struct EnergyCard: View {
    let totals: MetricTotals?
    @AppStorage("tt.energy.pricePerKWh") private var price = 0.15
    @State private var editing = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                SectionTitle(text: "Energy & usage today")
                Button { editing = true } label: { Image(systemName: "dollarsign.circle") }
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
                    .accessibilityLabel("Electricity price")
            }
            if let t = totals {
                let kwh = t.energyWhToday / 1000
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 14) {
                    Big(value: kwh < 1 ? String(format: "%.0f", t.energyWhToday) : String(format: "%.2f", kwh),
                        unit: kwh < 1 ? "Wh" : "kWh", label: String(format: "≈ $%.2f at $%.2f/kWh", kwh * price, price))
                    Big(value: compact(t.tokensToday), unit: "tokens", label: "\(t.requestsToday) requests")
                    Big(value: t.energyWhToday > 0.01 ? compact(t.tokensToday / t.energyWhToday) : "–",
                        unit: "tok/Wh", label: "Efficiency")
                    Big(value: String(format: "%.1f", t.energyWhAll / 1000), unit: "kWh",
                        label: "\(compact(t.tokensAll)) tokens all-time")
                }
            } else {
                Text("Collecting…").font(.tt(.subheadline)).foregroundStyle(.secondary)
            }
        }
        .padding(16)
        .card()
        .alert("Electricity price", isPresented: $editing) {
            TextField("Price per kWh", value: $price, format: .number).keyboardType(.decimalPad)
            Button("Done") {}
        } message: {
            Text("Used to estimate what running the box costs.")
        }
    }

    private func compact(_ value: Double) -> String {
        value.formatted(.number.notation(.compactName).precision(.fractionLength(0...1)))
    }

    private struct Big: View {
        let value: String
        let unit: String
        let label: String
        var body: some View {
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    Text(value).font(.system(size: 24, weight: .semibold, design: .rounded).monospacedDigit())
                    Text(unit).font(.tt(.caption)).foregroundStyle(.secondary)
                }
                Text(label).font(.tt(.caption2)).foregroundStyle(.secondary).lineLimit(1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

// MARK: - Health

/// Things worth knowing about, worst first; hidden when all is well.
struct HealthCard: View {
    let session: BoxSession
    let now: MetricsNow?

    private var alerts: [(icon: String, color: Color, text: String)] {
        var out: [(icon: String, color: Color, text: String)] = []
        if let error = session.statusError { out.append(("wifi.exclamationmark", .red, error)) }
        if let failure = session.startFailure { out.append(("xmark.octagon.fill", .red, "Last start failed: \(failure)")) }
        for (i, chip) in (now?.point?.chips ?? []).enumerated() {
            // Blackhole throttles at 90 °C.
            if let t = chip.tempC, t >= 88 {
                out.append(("thermometer.high", .red, "Chip \(i) is hot: \(Temp.string(t)), about to slow itself down"))
            } else if let t = chip.tempC, t >= 80 {
                out.append(("thermometer.medium", .orange, "Chip \(i) is warm: \(Temp.string(t))"))
            }
        }
        if let free = session.diskFreeBytes, free < 100_000_000_000 {
            out.append(("internaldrive", .orange, "Only \(ByteFormat.string(free)) of disk left for models."))
        }
        if let waiting = now?.inference?.serving?.requestsWaiting, waiting > 2 {
            out.append(("hourglass", .orange, "\(waiting) requests are queued."))
        }
        return out
    }

    var body: some View {
        let items = alerts
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 10) {
                SectionTitle(text: "Needs attention")
                ForEach(Array(items.enumerated()), id: \.offset) { _, alert in
                    Label(alert.text, systemImage: alert.icon)
                        .font(.tt(.subheadline))
                        .foregroundStyle(alert.color)
                }
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .card(tint: .orange.opacity(0.15))
        }
    }
}

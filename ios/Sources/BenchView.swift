import SwiftUI
import UIKit

/// "How fast is my box?": one tap benchmarks whatever is serving, and the result
/// is a card worth sharing -- speed, batch throughput, an image's time, and what a
/// million tokens costs in electricity at your price.
struct BenchCard: View {
    let session: BoxSession
    @AppStorage("tt.energy.pricePerKWh") private var price = 0.15
    @State private var result: BenchResult?
    @State private var running = false
    @State private var started: Date?
    @State private var error: String?
    @State private var shareImage: UIImage?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                SectionTitle(text: "Benchmark")
                if result != nil && !running {
                    Button { share() } label: { Image(systemName: "square.and.arrow.up") }
                        .buttonStyle(.glass)
                        .accessibilityLabel("Share result")
                }
            }
            if let result {
                BenchResultCard(result: result, price: price, boxName: session.box.name)
            } else if !running {
                Text("See what your box can really do: speed for one user, throughput for many at once, an image's time, and the cost of a million tokens in electricity.")
                    .font(.tt(.subheadline))
                    .foregroundStyle(.secondary)
            }
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.tt(.footnote))
                    .foregroundStyle(.orange)
            }
            Button {
                Task { await run() }
            } label: {
                HStack(spacing: 8) {
                    if running {
                        ProgressView()
                        if let started {
                            TimelineView(.periodic(from: .now, by: 1)) { context in
                                Text("Benchmarking… \(Elapsed.string(since: Int(started.timeIntervalSince1970), now: context.date))")
                                    .monospacedDigit()
                            }
                        }
                    } else {
                        Image(systemName: "gauge.with.dots.needle.67percent")
                        Text(result == nil ? "How fast is my box?" : "Run again")
                    }
                }
                .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .controlSize(.large)
            .disabled(running || !session.isServing)
        }
        .padding(18)
        .card(tint: Brand.accent.opacity(0.12), radius: 26)
        .sheet(item: Binding(get: { shareImage.map(ShareItem.init) }, set: { if $0 == nil { shareImage = nil } })) { item in
            ShareSheet(items: [item.image])
        }
    }

    private func run() async {
        running = true
        started = Date()
        error = nil
        defer { running = false }
        do {
            result = try await session.bench()
        } catch {
            self.error = error.localizedDescription
        }
    }

    @MainActor private func share() {
        guard let result else { return }
        let card = BenchResultCard(result: result, price: price, boxName: session.box.name)
            .padding(24)
            .frame(width: 420)
            .background(Brand.backdrop)
            .environment(\.colorScheme, .dark)
        let renderer = ImageRenderer(content: card)
        renderer.scale = 3
        shareImage = renderer.uiImage
    }
}

private struct ShareItem: Identifiable {
    let id = UUID()
    let image: UIImage
}

private struct ShareSheet: UIViewControllerRepresentable {
    let items: [Any]
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}

/// The result, laid out to read well both in the app and as a shared image.
struct BenchResultCard: View {
    let result: BenchResult
    let price: Double
    let boxName: String

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                TTMark().fill(Brand.accent).frame(width: 22, height: 22)
                VStack(alignment: .leading, spacing: 1) {
                    Text(boxName).font(.tt(.headline))
                    Text(subtitle).font(.tt(.caption)).foregroundStyle(.secondary)
                }
            }
            LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], alignment: .leading, spacing: 14) {
                if let tps = result.singleTps {
                    Stat(value: String(format: "%.0f", tps), unit: "tok/s", label: "One user")
                }
                if let tps = result.batchTps {
                    Stat(value: String(format: "%.0f", tps), unit: "tok/s", label: "\(result.batchSize) users at once")
                }
                if let ttft = result.ttftS {
                    Stat(value: ttft < 1 ? String(format: "%.0f", ttft * 1000) : String(format: "%.1f", ttft),
                         unit: ttft < 1 ? "ms" : "s", label: "First token")
                }
                if let image = result.imageS {
                    Stat(value: String(format: "%.1f", image), unit: "s", label: "Per image")
                }
                if let watts = result.watts {
                    Stat(value: String(format: "%.0f", watts), unit: "W", label: "Chip power")
                }
                if let wh = result.whPerMillionTokens {
                    Stat(value: String(format: "$%.2f", wh / 1000 * price), unit: "",
                         label: "Electricity per 1M tokens")
                }
            }
            ForEach(result.notes, id: \.self) { note in
                Text(note).font(.tt(.caption)).foregroundStyle(.secondary)
            }
            Text("Private, on my own Tenstorrent hardware.")
                .font(.tt(.caption2))
                .foregroundStyle(.tertiary)
        }
    }

    private var subtitle: String {
        [result.model.map(Brand.shortModel), result.imageModel.map(Brand.shortModel)]
            .compactMap { $0 }
            .joined(separator: " + ")
    }

    private struct Stat: View {
        let value: String
        let unit: String
        let label: String
        var body: some View {
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 3) {
                    Text(value).font(.tt(.title).weight(.bold).monospacedDigit())
                    if !unit.isEmpty { Text(unit).font(.tt(.caption).weight(.semibold)).foregroundStyle(.secondary) }
                }
                Text(label).font(.tt(.caption)).foregroundStyle(.secondary)
            }
        }
    }
}

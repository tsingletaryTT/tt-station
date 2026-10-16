import Charts
import SwiftUI

/// Lab mode: tt-lab runs gpt-oss straight on the Blackhole chips with its own
/// firmware -- no frameworks in between -- and can prove every output logit
/// matches its reference bit for bit. For seeing what the silicon itself does.
struct LabView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var status: LabStatus?
    @State private var modelID = "gpt-oss-20b"
    @State private var prompt = "Explain in three sentences how a mixture-of-experts model decides which experts to use."
    @State private var tokens = 512
    @State private var tiles = 32
    @State private var check = false
    @State private var profile = true
    @State private var output = ""
    @State private var result: LabEvent?
    @State private var running = false
    @State private var runTask: Task<Void, Never>?
    @State private var confirmStop = false
    @State private var error: String?

    private var model: LabStatus.Model? { status?.models.first { $0.id == modelID } }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    intro
                    if let status, !status.installed {
                        Label("tt-lab isn't built on this box yet. Build it once on the box: git clone https://github.com/tenstorrent/tt-lab ~/code/tt-lab, then ./make.py --env SFPI_PATH=/opt/tenstorrent/sfpi :build",
                              systemImage: "hammer")
                            .font(.tt(.footnote))
                            .padding(14)
                            .card(tint: .orange.opacity(0.2), radius: 18)
                    }
                    if let status { models(status) }
                    if model?.ready == true { controls }
                    if !output.isEmpty || running { transcript }
                    if let result, let stats = result.stats { LabResults(stats: stats, ok: result.ok ?? false, seconds: result.seconds) }
                    if let previous = result?.previous {
                        Button {
                            Task { try? await session.start(model: previous) }
                        } label: {
                            Label("Start \(Brand.shortModel(previous)) again", systemImage: "arrow.uturn.backward")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glass)
                    }
                    if let error {
                        Label(error, systemImage: "exclamationmark.triangle.fill").font(.tt(.footnote)).foregroundStyle(.orange)
                    }
                    if let history = status?.history, !history.isEmpty { historyList(history) }
                }
                .padding(16)
            }
            .background(Brand.backdrop)
            .navigationTitle("Silicon check")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task { await poll() }
            .confirmationDialog("The silicon check needs all the chips", isPresented: $confirmStop, titleVisibility: .visible) {
                Button("Stop \(status?.serving.map(Brand.shortModel) ?? "the model") and run", role: .destructive) {
                    start(stopServing: true)
                }
            } message: {
                Text("tt-lab runs its own firmware on the chips, so whatever is serving stops for the run. You can start it again afterwards.")
            }
        }
    }

    private var intro: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("gpt-oss on bare silicon", systemImage: "flask").font(.tt(.headline))
            Text("tt-lab runs OpenAI's gpt-oss directly on the Blackhole chips with hand-written firmware: no TT-Metal, no vLLM. It can check every output logit against its reference model, bit for bit.")
                .font(.tt(.subheadline))
                .foregroundStyle(.secondary)
        }
        .padding(16)
        .card(tint: Brand.accent.opacity(0.15), radius: 20)
    }

    private func models(_ status: LabStatus) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionTitle(text: "Model")
            ForEach(status.models) { model in
                HStack(spacing: 12) {
                    Image(systemName: modelID == model.id ? "largecircle.fill.circle" : "circle")
                        .foregroundStyle(Brand.accent)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(model.title).font(.tt(.headline))
                        if let job = model.preparing {
                            ProgressView(value: progress(job.bytes, job.total))
                            Text(job.stage == "requantizing" ? "Preparing for the chips…" : "Downloading \(ByteFormat.string(job.bytes)) of \(String(format: "%.0f", model.downloadGb)) GB")
                                .font(.tt(.caption)).foregroundStyle(.secondary)
                        } else if model.ready {
                            Text("Ready").font(.tt(.caption)).foregroundStyle(.green)
                        } else {
                            Text(String(format: "%.0f GB download, plus a one-time conversion", model.downloadGb))
                                .font(.tt(.caption)).foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                    if !model.ready && model.preparing == nil {
                        Button("Get") { Task { await prepare(model.id) } }.buttonStyle(.glass)
                    }
                }
                .padding(12)
                .card(radius: 16)
                .onTapGesture {
                    modelID = model.id
                    tiles = model.tiles.first ?? 32
                }
            }
            if let job = status.job, job.stage == "failed", let message = job.error {
                Text(message).font(.tt(.footnote)).foregroundStyle(.orange)
            }
        }
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 12) {
            SectionTitle(text: "Run")
            TextField("Prompt", text: $prompt, axis: .vertical)
                .lineLimit(2...6)
                .keyboardDoneButton()
                .padding(12)
                .card(radius: 14)
            if let model, model.tiles.count > 1 {
                Picker("Chips", selection: $tiles) {
                    ForEach(model.tiles, id: \.self) { t in
                        Text(t == 32 ? "All 4 chips" : "1 chip").tag(t)
                    }
                }
                .pickerStyle(.segmented)
            }
            Stepper("Up to \(tokens) tokens", value: $tokens, in: 16...2048, step: 64)
            Toggle("Check every logit against the reference", isOn: $check)
            Toggle("Profile each stage", isOn: $profile)
            Button {
                if running {
                    runTask?.cancel()
                } else if status?.serving != nil {
                    confirmStop = true
                } else {
                    start(stopServing: false)
                }
            } label: {
                Label(running ? "Stop" : "Run on the chips", systemImage: running ? "stop.fill" : "play.fill")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .controlSize(.large)
        }
    }

    private var transcript: some View {
        let parts = Harmony.split(output)
        return VStack(alignment: .leading, spacing: 10) {
            SectionTitle(text: running ? "Running…" : "Output")
            if !parts.thinking.isEmpty {
                DisclosureGroup("Thinking") {
                    Text(parts.thinking).font(.tt(.footnote)).foregroundStyle(.secondary).textSelection(.enabled)
                }
            }
            if !parts.answer.isEmpty {
                Text(parts.answer).textSelection(.enabled)
            }
            if !parts.log.isEmpty {
                DisclosureGroup("Log") {
                    Text(parts.log).font(.ttMono(12, relativeTo: .caption)).foregroundStyle(.secondary).textSelection(.enabled)
                }
            }
        }
        .padding(14)
        .card(radius: 18)
    }

    private func historyList(_ runs: [LabStatus.Run]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            SectionTitle(text: "Recent runs")
            ForEach(runs.prefix(8)) { run in
                HStack {
                    Image(systemName: run.ok ? "checkmark.circle.fill" : "xmark.circle.fill")
                        .foregroundStyle(run.ok ? .green : .red)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(run.prompt).lineLimit(1).font(.tt(.subheadline))
                        Text("\(run.model) · \(run.tiles == 32 ? "4 chips" : "1 chip")\(run.stats.check == "exact" ? " · bit-exact" : "")")
                            .font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if let tps = run.stats.decodeTps {
                        Text("\(Int(tps)) tok/s").font(.tt(.caption).monospacedDigit())
                    } else if let ms = run.stats.msPerToken {
                        Text(String(format: "%.1f ms/tok", ms)).font(.tt(.caption).monospacedDigit())
                    }
                }
            }
        }
    }

    private func progress(_ bytes: Int64, _ total: Int64?) -> Double? {
        guard let total, total > 0 else { return nil }
        return min(1, Double(bytes) / Double(total))
    }

    private func poll() async {
        while !Task.isCancelled {
            if let fresh = try? await session.lab() {
                status = fresh
            }
            try? await Task.sleep(for: .seconds(status?.job?.stage == "downloading" || status?.job?.stage == "requantizing" ? 2 : 10))
        }
    }

    private func prepare(_ id: String) async {
        do {
            try await session.prepareLab(model: id)
            modelID = id
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func start(stopServing: Bool) {
        output = ""
        result = nil
        error = nil
        running = true
        let request = LabRunRequest(model: modelID, prompt: prompt, tokens: check ? min(tokens, 8) : tokens,
                                    tiles: tiles, check: check, profile: profile, stopServing: stopServing)
        runTask = Task {
            do {
                for try await event in session.runLab(request) {
                    switch event.type {
                    case "text": output += event.text ?? ""
                    case "done": result = event
                    default: break
                    }
                }
            } catch is CancellationError {
            } catch {
                self.error = error.localizedDescription
            }
            running = false
            status = try? await session.lab()
        }
    }
}

/// The numbers from a run.
private struct LabResults: View {
    let stats: LabStats
    let ok: Bool
    let seconds: Double?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                SectionTitle(text: "Results")
                if stats.check == "exact" {
                    Label("Bit-exact", systemImage: "checkmark.seal.fill")
                        .font(.tt(.caption).weight(.bold))
                        .foregroundStyle(.green)
                } else if stats.check == "mismatch" {
                    Label("Mismatch", systemImage: "xmark.seal.fill").font(.tt(.caption).weight(.bold)).foregroundStyle(.red)
                } else if !ok {
                    Label("Failed", systemImage: "xmark.octagon.fill").font(.tt(.caption).weight(.bold)).foregroundStyle(.red)
                }
            }
            LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], alignment: .leading, spacing: 12) {
                if let v = stats.decodeTps { stat(String(format: "%.0f", v), "tok/s", "Generating") }
                if let v = stats.prefillTps { stat(String(format: "%.0f", v), "tok/s", "Reading the prompt") }
                if let v = stats.msPerToken { stat(String(format: "%.2f", v), "ms", "Per token on the chips") }
                if let v = stats.preloadS { stat(String(format: "%.1f", v), "s", "Loading weights") }
                if let v = stats.dramGib { stat(String(format: "%.1f", v), "GiB", "In chip memory") }
                if let v = stats.tokens { stat(String(format: "%.0f", v), "", "Tokens generated") }
            }
            if let profile = stats.profile, !profile.isEmpty {
                Text("Where the time goes").font(.tt(.subheadline).weight(.semibold))
                Chart(profile, id: \.stage) { stage in
                    BarMark(x: .value("Share", stage.percent ?? 0), y: .value("Stage", stage.stage))
                        .foregroundStyle(Brand.accent.gradient)
                        .annotation(position: .trailing) {
                            Text(String(format: "%.0f%%", stage.percent ?? 0)).font(.tt(.caption2)).foregroundStyle(.secondary)
                        }
                }
                .chartXAxis(.hidden)
                .frame(height: CGFloat(profile.count) * 22 + 10)
            }
        }
        .padding(16)
        .card(tint: Brand.accent.opacity(0.12), radius: 20)
    }

    private func stat(_ value: String, _ unit: String, _ label: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline, spacing: 3) {
                Text(value).font(.tt(.title2).weight(.bold).monospacedDigit())
                if !unit.isEmpty { Text(unit).font(.tt(.caption).weight(.semibold)).foregroundStyle(.secondary) }
            }
            Text(label).font(.tt(.caption)).foregroundStyle(.secondary)
        }
    }
}

/// gpt-oss writes in OpenAI's Harmony format: `<|channel|>analysis<|message|>`
/// for its reasoning and `<|channel|>final<|message|>` for the answer. Split
/// tt-lab's output into those, plus its own log lines.
enum Harmony {
    static func split(_ raw: String) -> (thinking: String, answer: String, log: String) {
        var log: [String] = []
        var body = ""
        // tt-lab prints its stats right after the generated text, without a newline.
        var text = raw
        for marker in ["prefill: ", "generated: ", "decode: ", "profile: ", "host: "] {
            text = text.replacingOccurrences(of: marker, with: "\n" + marker)
        }
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let l = String(line)
            if isLog(l) { log.append(l) } else { body += (body.isEmpty ? "" : "\n") + l }
        }
        var thinking = ""
        var answer = ""
        for segment in body.components(separatedBy: "<|channel|>").dropFirst() {
            let pieces = segment.components(separatedBy: "<|message|>")
            guard pieces.count >= 2 else { continue }
            let text = clean(pieces.dropFirst().joined(separator: "<|message|>"))
            if pieces[0].hasPrefix("final") { answer += text } else { thinking += text }
        }
        if thinking.isEmpty && answer.isEmpty { answer = clean(body) }
        return (thinking.trimmingCharacters(in: .whitespacesAndNewlines),
                answer.trimmingCharacters(in: .whitespacesAndNewlines),
                log.joined(separator: "\n"))
    }

    private static func clean(_ text: String) -> String {
        var t = text
        for marker in ["<|end|>", "<|return|>", "<|start|>assistant", "<|start|>", "<|call|>"] {
            t = t.replacingOccurrences(of: marker, with: "")
        }
        // The prompt echo before the assistant's first channel.
        if let range = t.range(of: "<|message|>") { t = String(t[range.upperBound...]) }
        return t
    }

    private static func isLog(_ line: String) -> Bool {
        let prefixes = ["warning:", "loaded TTQ", "prefill:", "generated:", "decode:", "profile:", "host:", "silicon ",
                        "proxy ", "  logits", "BRISC", "error:"]
        return prefixes.contains { line.hasPrefix($0) } || line.contains("cycles/token")
    }
}

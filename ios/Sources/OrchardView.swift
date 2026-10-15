import SwiftUI
import UIKit

/// tt-orchard's names for its stages and states (`orchard/lexicon.py`), with
/// the plain names beside them.
enum OrchardWords {
    static let stages: [(emoji: String, name: String, plain: String)] = [
        ("🔭", "survey", "Intake: how it differs from a model that runs today"),
        ("🧪", "soil test", "Environment and CPU reference"),
        ("🌱", "graft", "Functional on one chip"),
        ("🌳", "trunk", "Full model"),
        ("🌿", "rows", "1, 2 and 4 chips"),
        ("🚪", "farm gate", "Serving"),
        ("🍯", "taste test", "Quality and benchmark"),
        ("📦", "crate", "Package"),
        ("🧺", "harvest", "Review bundle"),
    ]

    static func state(_ state: String) -> (emoji: String, name: String, detail: String) {
        switch state {
        case "preparing": return ("🌰", "Planting", "Checking the model and downloading its weights.")
        case "running": return ("🌤", "Growing", "Working through the stages.")
        case "paused": return ("🌧", "Rain delay", "Paused. It's holding the chips and waiting for you.")
        case "ready-for-operator-review": return ("🍎", "Ripe", "Ready for your review. Nothing was published.")
        case "blocked": return ("❄️", "Frost", "It stopped and said why. The chips are free again.")
        case "aborted": return ("🍂", "Fallen", "Stopped. The chips are free again.")
        case "stopped-or-crashed": return ("⛈", "Storm damage", "It stopped on an error. Retry to resume from where it was.")
        case "refused": return ("🚫", "Didn't start", "tt-orchard's preflight refused it. See the console.")
        default: return ("🌰", "Seed", "Not started.")
        }
    }

    static let reports: [(id: String, title: String, icon: String)] = [
        ("results", "Results", "checklist"),
        ("risks", "Risks", "exclamationmark.shield"),
        ("card", "Model card", "doc.richtext"),
        ("publish", "Publish commands", "terminal"),
        ("blocked", "Why it stopped", "snowflake"),
        ("console", "Console", "text.alignleft"),
    ]
}

/// Orchard: bring up a new model on the box, with the box. The phone starts a
/// run, follows it stage by stage, and opens the review bundle at the end.
struct OrchardView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var status: OrchardStatus?
    @State private var model = ""
    @State private var preflight: OrchardPreflight?
    @State private var checking = false
    @State private var starting = false
    @State private var acceptCredentials = false
    @State private var confirmStop = false
    @State private var confirmSetup = false
    @State private var error: String?

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Give Orchard a model from Hugging Face. It works out how the model differs from one that already runs on Blackhole, writes and runs the tests, checks the output against a CPU reference on 1, 2 and 4 chips, and stops at a review bundle. Its agents run on this box, and it never publishes anything.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                }
                if let error {
                    Section { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange) }
                }
                if let status {
                    if !status.ready || status.setupRunning {
                        setupSection(status)
                    }
                    if status.ready {
                        newRunSection(status)
                    }
                    if !status.runs.isEmpty {
                        Section("Runs") {
                            ForEach(status.runs) { run in
                                NavigationLink {
                                    OrchardRunView(session: session, name: run.runName, initial: run)
                                } label: {
                                    RunRow(run: run)
                                }
                            }
                        }
                    }
                } else {
                    Section { HStack { ProgressView(); Text("Asking the box…").foregroundStyle(.secondary) } }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            .navigationTitle("Orchard")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .refreshable { await load(fresh: true) }
            .task {
                while !Task.isCancelled {
                    await load(fresh: false)
                    let live = status?.setupRunning == true || status?.runs.contains(where: \.live) == true
                    try? await Task.sleep(for: .seconds(live ? 5 : 20))
                }
            }
            .confirmationDialog("A bring-up needs all four chips", isPresented: $confirmStop, titleVisibility: .visible) {
                Button("Stop the model and start", role: .destructive) { Task { await start(stopServing: true) } }
            } message: {
                Text("Whatever is serving stops for the run, which takes hours. It starts again when the run ends.")
            }
            .confirmationDialog("Set up Orchard on the box?", isPresented: $confirmSetup, titleVisibility: .visible) {
                Button("Set up") { Task { await runSetup() } }
            } message: {
                Text("Runs tt-orchard's own setup: installs tt-gozer (the chip lease manager), a Python environment for the CPU reference, links the tt-orchard command, writes its QuietBox 2 config, and downloads the coder model the agents use (about 160 GB). It never resets a chip or publishes anything.")
            }
        }
    }

    // MARK: setup

    @ViewBuilder
    private func setupSection(_ status: OrchardStatus) -> some View {
        Section {
            if !status.installed {
                Text("tt-orchard isn't on the box yet. Setup downloads it first.")
                    .font(.tt(.callout))
            }
            ForEach(status.setup, id: \.self) { step in
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: icon(step.status)).foregroundStyle(tint(step.status))
                    VStack(alignment: .leading, spacing: 2) {
                        Text(step.name).font(.tt(.subheadline).weight(.semibold))
                        Text(step.detail).font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                }
            }
            if let configError = status.configError, status.installed, status.config == nil {
                Label(configError, systemImage: "doc.badge.gearshape").font(.tt(.caption)).foregroundStyle(.secondary)
            }
            ForEach(status.notes, id: \.self) { note in
                VStack(alignment: .leading, spacing: 6) {
                    Text(note.text).font(.tt(.caption))
                    if let command = note.command {
                        Button {
                            UIPasteboard.general.string = command
                        } label: {
                            Label(command, systemImage: "doc.on.doc").font(.ttMono(12, relativeTo: .caption))
                        }
                    }
                }
            }
            if status.setupRunning {
                VStack(alignment: .leading, spacing: 6) {
                    HStack { ProgressView(); Text("Setting up…").foregroundStyle(.secondary) }
                    Text(status.setupLog.suffix(12).joined(separator: "\n"))
                        .font(.ttMono(11, relativeTo: .caption2))
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            } else {
                Button { confirmSetup = true } label: {
                    Label(status.installed ? "Do the setup steps" : "Set up Orchard", systemImage: "wrench.and.screwdriver")
                }
                .buttonStyle(.glass)
            }
        } header: {
            Text("Setup")
        } footer: {
            if status.setup.contains(where: { $0.status == "fail" }) {
                Text("Steps marked with a cross need you on the box: setup can't install them itself.")
            }
        }
    }

    private func icon(_ status: String) -> String {
        switch status {
        case "ok": return "checkmark.circle.fill"
        case "warn": return "exclamationmark.circle.fill"
        case "todo": return "circle.dashed"
        default: return "xmark.circle.fill"
        }
    }

    private func tint(_ status: String) -> Color {
        switch status {
        case "ok": return .green
        case "warn": return .yellow
        case "todo": return Brand.accent
        default: return .red
        }
    }

    // MARK: new run

    @ViewBuilder
    private func newRunSection(_ status: OrchardStatus) -> some View {
        Section {
            if status.busy {
                Label("A bring-up is running. One at a time.", systemImage: "leaf.fill")
                    .foregroundStyle(.secondary)
            } else {
                TextField("org/name, e.g. Cloudflare/clef", text: $model)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.ttMono(17, relativeTo: .body))
                    .onSubmit { Task { await check() } }
                    .onChange(of: model) { preflight = nil }
                if checking {
                    HStack { ProgressView(); Text("Checking with tt-orchard…").foregroundStyle(.secondary) }
                } else if let preflight {
                    PreflightRows(preflight: preflight)
                    if preflight.checks.contains(where: { $0.reason == "credentials-needed" }) {
                        Toggle(isOn: $acceptCredentials) {
                            VStack(alignment: .leading, spacing: 2) {
                                Text("Let the agents see my credential files")
                                Text("Agent shells run as your user on the box, so code they run could read them.")
                                    .font(.tt(.caption)).foregroundStyle(.secondary)
                            }
                        }
                        .onChange(of: acceptCredentials) { Task { await check() } }
                    }
                    if !preflight.blocked {
                        Button {
                            Task { await start(stopServing: false) }
                        } label: {
                            Label(preflight.resuming ? "Resume bring-up" : "Start bring-up", systemImage: "leaf")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glassProminent)
                        .disabled(starting)
                    }
                } else {
                    Button { Task { await check() } } label: { Label("Check", systemImage: "checklist") }
                        .disabled(model.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        } header: {
            Text("New bring-up")
        } footer: {
            if let coder = status.config?.coder {
                Text("The agents run \(Brand.shortModel(coder.target)) on \(coder.chips) chips; the other chips test the new model. Runs take hours, and you'll get a notification at each stage.")
            }
        }
        Section {
            Toggle(isOn: Binding(get: { status.packaging }, set: { on in
                Task {
                    do { try await session.setOrchardPackaging(on); await load(fresh: true) }
                    catch { self.error = error.localizedDescription }
                }
            })) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Package it so you can serve it")
                    Text("Adds a packaging stage (a v6 bundle, boot-checked on the chips), so a finished bring-up can be served with one tap. Nothing is ever published.")
                        .font(.tt(.caption)).foregroundStyle(.secondary)
                }
            }
        }
    }

    // MARK: actions

    private func load(fresh: Bool) async {
        do {
            status = try await session.orchard(fresh: fresh)
            if error?.hasPrefix("Couldn't reach") == true { error = nil }
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func check() async {
        let id = model.trimmingCharacters(in: .whitespaces)
        guard !id.isEmpty else { return }
        checking = true
        error = nil
        defer { checking = false }
        do {
            preflight = try await session.orchardPreflight(model: id, acceptCredentials: acceptCredentials)
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func start(stopServing: Bool) async {
        guard let preflight else { return }
        starting = true
        error = nil
        defer { starting = false }
        do {
            try await session.startOrchard(model: preflight.model, stopServing: stopServing,
                                           acceptCredentials: acceptCredentials)
            model = ""
            self.preflight = nil
            await load(fresh: false)
        } catch AgentError.http(409, let message) where message.contains("Stop") {
            confirmStop = true
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func runSetup() async {
        do {
            try await session.orchardSetup()
            await load(fresh: true)
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct PreflightRows: View {
    let preflight: OrchardPreflight

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(preflight.model).font(.tt(.headline))
                Spacer()
                if preflight.resuming { Text("resumes").font(.tt(.caption)).foregroundStyle(.secondary) }
            }
            Text(download).font(.tt(.caption)).foregroundStyle(.secondary)
            ForEach(preflight.checks, id: \.self) { check in
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: check.status == "ok" ? "checkmark.circle.fill"
                          : check.status == "warn" ? "exclamationmark.circle.fill" : "xmark.octagon.fill")
                        .foregroundStyle(check.status == "ok" ? .green : check.status == "warn" ? .yellow : .red)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(check.name).font(.tt(.caption).weight(.semibold))
                        Text(check.detail).font(.tt(.caption2)).foregroundStyle(.secondary)
                    }
                }
            }
        }
        .padding(.vertical, 4)
    }

    private var download: String {
        var parts: [String] = []
        parts.append(preflight.local ? "Weights already on the box" : String(format: "%.1f GB to download", preflight.downloadGb))
        if let license = preflight.license { parts.append("license \(license)") }
        return parts.joined(separator: " · ")
    }
}

private struct RunRow: View {
    let run: OrchardRun

    var body: some View {
        let state = OrchardWords.state(run.state)
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(state.emoji)
                Text(run.title).font(.tt(.headline)).lineLimit(1)
                Spacer()
                Text(state.name).font(.tt(.caption)).foregroundStyle(.secondary)
            }
            StageDots(stages: run.stages, current: run.live ? run.stage?.current : nil)
            if let p = run.progress {
                if run.live {
                    ProgressView(value: p.overall)
                        .tint(Brand.accent)
                    Text([p.title, p.overallEtaS.map { OrchardProgress.approx($0) + " left in all" }]
                        .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(1)
                } else if let needs = p.needsYou {
                    Text(needs).font(.tt(.caption)).foregroundStyle(.orange).lineLimit(2)
                }
            }
        }
        .padding(.vertical, 4)
    }
}

/// Nine dots, one per stage.
private struct StageDots: View {
    let stages: [OrchardRun.Stage]
    let current: Int?

    var body: some View {
        HStack(spacing: 6) {
            ForEach(0..<OrchardWords.stages.count, id: \.self) { n in
                Circle()
                    .fill(color(n))
                    .frame(width: 8, height: 8)
                    .overlay(Circle().stroke(Brand.accent, lineWidth: current == n ? 2 : 0).padding(-2))
            }
        }
    }

    private func color(_ n: Int) -> Color {
        switch stages.last(where: { $0.stage == n })?.status {
        case "pass": return .green
        case "fail": return .red
        case "escalated": return .orange
        case "running": return Brand.accent
        case "skipped": return .white.opacity(0.25)
        default: return .white.opacity(0.1)
        }
    }
}

/// One run: its stages, what to do next, and the review bundle.
struct OrchardRunView: View {
    let session: BoxSession
    let name: String
    let initial: OrchardRun
    @State private var run: OrchardRun?
    @State private var error: String?
    @State private var working = false
    @State private var confirmAbort = false
    @State private var confirmStop = false
    @State private var report: OrchardReport?
    @State private var served = false
    @State private var confirmServeStop = false

    var body: some View {
        let run = self.run ?? initial
        let state = OrchardWords.state(run.state)
        List {
            Section {
                HStack(spacing: 14) {
                    Text(state.emoji).font(.tt(.largeTitle))
                    VStack(alignment: .leading, spacing: 3) {
                        Text(state.name).font(.tt(.title3).weight(.semibold))
                        Text(state.detail).font(.tt(.footnote)).foregroundStyle(.secondary)
                    }
                }
                if let p = run.progress {
                    NowCard(progress: p, live: run.live)
                }
                if let reason = run.blocked?.reason, run.state == "blocked" {
                    Label(reason, systemImage: "snowflake").font(.tt(.callout))
                }
                if let reason = run.pause?.reason, run.state == "paused" {
                    Label(reason, systemImage: "pause.circle").font(.tt(.callout))
                }
                if let hint = run.hint, !hint.isEmpty, run.live {
                    Text(hint).font(.tt(.caption)).foregroundStyle(.secondary)
                }
                if let restore = run.restore {
                    Label("\(Brand.shortModel(restore)) starts again when this ends.", systemImage: "arrow.uturn.backward")
                        .font(.tt(.caption)).foregroundStyle(.secondary)
                }
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                }
            }
            controls(run)
            Section("Stages") {
                ForEach(0..<OrchardWords.stages.count, id: \.self) { n in
                    StageRow(n: n, stage: run.stages.last(where: { $0.stage == n }),
                             current: run.live && run.stage?.current == n,
                             typical: run.progress?.typicalS[safe: n])
                }
            }
            if let reports = run.reports, !reports.isEmpty {
                Section(run.state == "ready-for-operator-review" ? "Harvest basket" : "Reports") {
                    ForEach(OrchardWords.reports.filter { reports.contains($0.id) }, id: \.id) { item in
                        Button {
                            Task { await open(item.id, title: item.title) }
                        } label: {
                            Label(item.title, systemImage: item.icon)
                        }
                    }
                }
            }
            if let console = run.console, !console.isEmpty, run.live {
                Section("Latest output") {
                    Text(console.suffix(15).joined(separator: "\n"))
                        .font(.ttMono(11, relativeTo: .caption2))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Brand.backdrop)
        .navigationTitle(run.title)
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await load() }
        .task {
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds((self.run ?? initial).live ? 5 : 30))
            }
        }
        .confirmationDialog("Stop this bring-up?", isPresented: $confirmAbort, titleVisibility: .visible) {
            Button("Stop it", role: .destructive) { Task { await control("abort") } }
        } message: {
            Text("It releases the chips. Its ledger stays, so Retry picks it up again later.")
        }
        .confirmationDialog("A bring-up needs all four chips", isPresented: $confirmStop, titleVisibility: .visible) {
            Button("Stop the model and retry", role: .destructive) { Task { await control("retry", stopServing: true) } }
        }
        .sheet(item: $report) { report in ReportView(report: report) }
        .confirmationDialog("Serving it replaces what's serving now", isPresented: $confirmServeStop, titleVisibility: .visible) {
            Button("Stop it and serve \((self.run ?? initial).title)", role: .destructive) {
                Task { await serve(stopServing: true) }
            }
        }
        .alert("Starting \((self.run ?? initial).title)", isPresented: $served) {
            Button("OK", role: .cancel) {}
        } message: {
            Text("It loads onto the chips now. You'll get a notification when it's ready to chat with.")
        }
    }

    @ViewBuilder
    private func controls(_ run: OrchardRun) -> some View {
        Section {
            switch run.state {
            case "running":
                Button { Task { await control("pause") } } label: { Label("Pause", systemImage: "pause.fill") }
                Button(role: .destructive) { confirmAbort = true } label: { Label("Stop", systemImage: "stop.fill") }
            case "paused":
                Button { Task { await control("resume") } } label: { Label("Resume", systemImage: "play.fill") }
                Button(role: .destructive) { confirmAbort = true } label: { Label("Stop", systemImage: "stop.fill") }
            case "preparing":
                Button(role: .destructive) { confirmAbort = true } label: { Label("Stop", systemImage: "stop.fill") }
            case "blocked", "stopped-or-crashed", "refused", "aborted":
                Button { Task { await control("retry") } } label: {
                    Label("Retry from where it stopped", systemImage: "arrow.clockwise")
                }
            case "ready-for-operator-review" where run.servable:
                Button { Task { await serve(stopServing: false) } } label: {
                    Label("Serve \(run.title) on the box", systemImage: "play.fill")
                }
            case "ready-for-operator-review":
                Text("To serve it, turn on packaging in Orchard and run it again.")
                    .font(.tt(.caption)).foregroundStyle(.secondary)
            default:
                EmptyView()
            }
        }
        .disabled(working)
    }

    private func load() async {
        do {
            run = try await session.orchardRun(name)
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func control(_ action: String, stopServing: Bool = false) async {
        working = true
        error = nil
        defer { working = false }
        do {
            try await session.orchardControl(name, action: action, stopServing: stopServing)
            try? await Task.sleep(for: .seconds(1))
            await load()
        } catch AgentError.http(409, let message) where action == "retry" && message.contains("Stop") {
            confirmStop = true
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func serve(stopServing: Bool) async {
        working = true
        error = nil
        defer { working = false }
        do {
            try await session.serveOrchard(name, stopServing: stopServing)
            served = true
        } catch AgentError.http(409, let message) where message.contains("Stop it first") {
            confirmServeStop = true
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func open(_ which: String, title: String) async {
        do {
            report = OrchardReport(title: title, text: try await session.orchardReport(name, which))
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct StageRow: View {
    let n: Int
    let stage: OrchardRun.Stage?
    let current: Bool
    let typical: Double?

    var body: some View {
        let words = OrchardWords.stages[n]
        HStack(spacing: 12) {
            Text(words.emoji).font(.tt(.title3)).opacity(stage == nil ? 0.35 : 1)
            VStack(alignment: .leading, spacing: 2) {
                Text(words.name.capitalized).font(.tt(.subheadline).weight(.semibold))
                Text(stage?.name.isEmpty == false ? stage!.name : words.plain)
                    .font(.tt(.caption)).foregroundStyle(.secondary)
            }
            Spacer()
            if current {
                ProgressView()
            } else if stage == nil, let typical {
                Text("usually " + OrchardProgress.approx(typical).replacingOccurrences(of: "about ", with: "~"))
                    .font(.tt(.caption2)).foregroundStyle(.tertiary)
            } else if let stage {
                VStack(alignment: .trailing, spacing: 2) {
                    Text(label(stage.status)).font(.tt(.caption).weight(.medium)).foregroundStyle(color(stage.status))
                    if let wall = stage.wallS, wall > 0 {
                        Text(duration(wall)).font(.tt(.caption2)).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }

    private func label(_ status: String) -> String {
        switch status {
        case "pass": return "Passed"
        case "fail": return "Failed"
        case "skipped": return "Skipped"
        case "escalated": return "Escalated"
        default: return "Running"
        }
    }

    private func color(_ status: String) -> Color {
        switch status {
        case "pass": return .green
        case "fail": return .red
        case "escalated": return .orange
        default: return .secondary
        }
    }

    private func duration(_ seconds: Double) -> String {
        let m = Int(seconds / 60)
        return m >= 60 ? "\(m / 60) h \(m % 60) min" : "\(max(1, m)) min"
    }
}

struct OrchardReport: Identifiable {
    let title: String
    let text: String
    var id: String { title }
}

private struct ReportView: View {
    let report: OrchardReport
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ScrollView {
                Text(markdown)
                    .font(report.title == "Console" || report.title == "Publish commands" ? .ttMono(12) : .tt(.callout))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(16)
            }
            .background(Brand.backdrop)
            .navigationTitle(report.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    ShareLink(item: report.text, subject: Text(report.title)) { Image(systemName: "square.and.arrow.up") }
                }
            }
        }
    }

    private var markdown: AttributedString {
        guard report.title != "Console", report.title != "Publish commands",
              let parsed = try? AttributedString(markdown: report.text,
                                                 options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
        else { return AttributedString(report.text) }
        return parsed
    }
}

/// What's happening now: the step's bar, time left, the whole run's bar, and
/// what (if anything) the owner should do.
private struct NowCard: View {
    let progress: OrchardProgress
    let live: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if live, !progress.title.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(alignment: .firstTextBaseline) {
                        Text(progress.title).font(.tt(.headline))
                        Spacer()
                        if let eta = progress.etaS {
                            Text(OrchardProgress.approx(eta) + " left")
                                .font(.tt(.caption).monospacedDigit()).foregroundStyle(.secondary)
                        }
                    }
                    if let fraction = progress.fraction {
                        ProgressView(value: fraction).tint(Brand.accent)
                    } else {
                        // Can't be measured (waiting for chips, between steps).
                        ProgressView().controlSize(.small)
                    }
                    if !progress.detail.isEmpty {
                        Text(progress.detail).font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                }
            }
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text("Whole run").font(.tt(.caption).weight(.semibold))
                    Spacer()
                    Text(overallLabel).font(.tt(.caption).monospacedDigit()).foregroundStyle(.secondary)
                }
                ProgressView(value: progress.overall).tint(.green)
                if live, let note = progress.estimateNote {
                    Text(note).font(.tt(.caption2)).foregroundStyle(.tertiary)
                }
            }
            if let needs = progress.needsYou {
                Label(needs, systemImage: "hand.point.right.fill")
                    .font(.tt(.callout))
                    .foregroundStyle(.orange)
            } else if let hint = progress.hint {
                Label(hint, systemImage: "bell.badge")
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 4)
    }

    private var overallLabel: String {
        let pct = "\(Int((progress.overall * 100).rounded()))%"
        guard live, let eta = progress.overallEtaS else { return pct }
        return pct + " · " + OrchardProgress.approx(eta) + " left"
    }
}

private extension Array {
    subscript(safe index: Int) -> Element? { indices.contains(index) ? self[index] : nil }
}

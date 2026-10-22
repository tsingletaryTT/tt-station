// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// The Box tab: what the active box is doing, in order of how often you need
/// it. What's serving (with the one action that matters now), anything in
/// progress, the chips at a glance (tap for the hardware page), Box Doctor's
/// verdict, and the things to try. Stop, reset and power live in the menu;
/// connections, notifications and preferences live in Settings.
struct DashboardView: View {
    @Environment(AppModel.self) private var app

    var body: some View {
        NavigationStack {
            Group {
                if let session = app.session {
                    BoxHome(session: session, telemetry: app.telemetry)
                } else {
                    ContentUnavailableView("No box selected", systemImage: "server.rack",
                                           description: Text("Add or pick a box in Settings."))
                }
            }
            .background { Brand.backdrop }
            .navigationTitle(app.activeBox?.name ?? "Box")
            .toolbar {
                if app.store.boxes.count > 1 {
                    ToolbarItem(placement: .topBarLeading) { BoxSwitcher() }
                }
            }
        }
    }
}

/// Switch boxes without leaving the Box tab.
private struct BoxSwitcher: View {
    @Environment(AppModel.self) private var app

    var body: some View {
        Menu {
            ForEach(app.store.boxes) { box in
                Button {
                    app.activate(box)
                } label: {
                    if app.activeBox?.id == box.id {
                        Label(box.name, systemImage: "checkmark")
                    } else {
                        Text(box.name)
                    }
                }
            }
            Divider()
            Button("Manage boxes", systemImage: "gearshape") { app.selectedTab = .settings }
        } label: {
            Image(systemName: "server.rack")
        }
        .accessibilityLabel("Switch box")
    }
}

private struct BoxHome: View {
    let session: BoxSession
    let telemetry: TelemetryFeed
    @Environment(AppModel.self) private var app
    @State private var confirming: BoxControl?
    @State private var working: BoxControl?
    @State private var controlError: String?
    @State private var showPower = false
    // Read so a unit change redraws every temperature.
    @AppStorage(Temp.key) private var tempUnit = "c"

    enum BoxControl: String, Identifiable {
        case stop, reset
        var id: String { rawValue }
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 18) {
                TelemetryBanner(telemetry: telemetry)
                if let controlError {
                    Label(controlError, systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }
                ServingCard(session: session, onChooseModel: { app.selectedTab = .models },
                            onOpenCreate: { app.selectedTab = .create })
                BoxGlanceCard(session: session, telemetry: telemetry)
                WaitingCard(session: session)
                HappeningNow(session: session)
                NavigationLink {
                    HardwareView(session: session, telemetry: telemetry)
                } label: {
                    ChipsSummary(telemetry: telemetry)
                }
                .buttonStyle(.plain)
                DoctorCard(session: session)
                ExploreGrid(session: session)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
            .readableWidth()
        }
        .scrollIndicators(.hidden)
        .refreshable { await session.refreshStatus() }
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Menu {
                    // The box refuses these too; disabled here so a member isn't surprised.
                    Button("Stop model", systemImage: "stop.fill") { confirming = .stop }
                        .disabled(!session.isServing || working != nil || session.access?.mayControlModels == false)
                    Button("Reset chips", systemImage: "arrow.counterclockwise") { confirming = .reset }
                        .disabled(working != nil || (session.access != nil && !session.isOwner))
                    Divider()
                    Button("Sleep and power…", systemImage: "power") { showPower = true }
                } label: {
                    if working != nil { ProgressView() } else { Image(systemName: "ellipsis.circle") }
                }
                .accessibilityLabel("Box actions")
            }
        }
        .confirmationDialog(confirming == .stop ? "Stop serving the model?" : "Reset the chips?",
                            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
                            titleVisibility: .visible, presenting: confirming) { control in
            Button(control == .stop ? "Stop model" : "Reset chips", role: .destructive) {
                Task { await perform(control) }
            }
        } message: { control in
            Text(control == .stop
                 ? "Everyone using \(session.box.name) loses it: the assistant, Code and apps go offline until a model is started again."
                 : "Stops serving and resets the Tenstorrent chips. Everyone using \(session.box.name) loses the model until one is started again.")
        }
        .sheet(isPresented: $showPower) { PowerSheet(session: session) }
    }

    private func perform(_ control: BoxControl) async {
        working = control
        controlError = nil
        defer { working = nil }
        do {
            switch control {
            case .stop: try await session.stopServing()
            case .reset: try await session.resetChips()
            }
        } catch {
            controlError = error.localizedDescription
        }
    }
}

/// The chips at a glance: one row of four, hottest first in your eye line.
private struct ChipsSummary: View {
    let telemetry: TelemetryFeed

    var body: some View {
        let live = telemetry.isLive()
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Chips").font(.tt(.headline))
                Spacer()
                if let watts = telemetry.totalPower {
                    Text("\(Int(watts.rounded())) W").font(.tt(.subheadline).weight(.semibold).monospacedDigit())
                        .foregroundStyle(.secondary)
                }
                Image(systemName: "chevron.right").foregroundStyle(.tertiary)
            }
            if telemetry.chips.isEmpty {
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Waiting for live readings…").font(.tt(.subheadline)).foregroundStyle(.secondary)
                }
            } else {
                HStack(spacing: 10) {
                    ForEach(telemetry.chips) { chip in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(Temp.short(chip.tempC))
                                .font(.tt(.title2).weight(.semibold).monospacedDigit())
                                .foregroundStyle(chip.tempC.map(Brand.tempColor) ?? Color.secondary)
                            Capsule()
                                .fill(Color.white.opacity(0.1))
                                .overlay(alignment: .leading) {
                                    GeometryReader { g in
                                        Capsule().fill(Brand.accent)
                                            .frame(width: g.size.width * CGFloat(ChipStyle.activity(chip)))
                                    }
                                }
                                .frame(height: 4)
                            Text(chip.powerW.map { "\(Int($0.rounded())) W" } ?? "–")
                                .font(.tt(.caption2).monospacedDigit()).foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                Text("Temperatures in \(Temp.symbol) · bars show load")
                    .font(.tt(.caption2)).foregroundStyle(.tertiary)
            }
        }
        .padding(16)
        .opacity(live || telemetry.chips.isEmpty ? 1 : 0.5)
        .card()
    }
}

/// Everything about the hardware: each chip, health, inference, energy,
/// history, benchmark and the serving log.
struct HardwareView: View {
    let session: BoxSession
    let telemetry: TelemetryFeed
    @State private var metrics = MetricsModel()
    @State private var logLines: [String] = []
    @State private var logError: String?
    @AppStorage(Temp.key) private var tempUnit = "c"
    private let columns = [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)]

    var body: some View {
        ScrollView {
            VStack(spacing: 22) {
                TelemetryBanner(telemetry: telemetry)
                if !telemetry.chips.isEmpty {
                    LazyVGrid(columns: columns, spacing: 12) {
                        ForEach(telemetry.chips) { ChipCard(chip: $0, live: telemetry.isLive()) }
                    }
                }
                HealthCard(session: session, now: metrics.now)
                InferenceCard(now: metrics.now, kind: session.servedKind)
                EnergyCard(totals: metrics.now?.totals ?? metrics.history?.totals)
                HistoryCharts(metrics: metrics, session: session)
                BenchCard(session: session)
                VStack(spacing: 12) {
                    HStack {
                        SectionTitle(text: "Serving log")
                        Button { Task { await loadLogs() } } label: { Image(systemName: "arrow.clockwise") }
                            .buttonStyle(.glass)
                            .accessibilityLabel("Refresh log")
                    }
                    LogCard(lines: logLines, error: logError)
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
            .readableWidth()
        }
        .scrollIndicators(.hidden)
        .background(Brand.backdrop)
        .navigationTitle("Hardware")
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await loadLogs() }
        .task(id: session.box.id) { await metrics.run(session) }
        .task(id: session.box.id) {
            while !Task.isCancelled {
                await loadLogs()
                try? await Task.sleep(for: .seconds(10))
            }
        }
    }

    private func loadLogs() async {
        do {
            logLines = try await session.logs(tail: 40)
            logError = nil
        } catch {
            logError = error.localizedDescription
        }
    }
}

/// Long work in progress on the box, with its progress: an Orchard bring-up
/// and transcription jobs. Shows nothing when nothing is going.
private struct HappeningNow: View {
    let session: BoxSession
    @State private var orchard: OrchardRun?
    @State private var bringup: BringupRun?
    @State private var deciding = false
    @State private var jobs: [BoxJob] = []
    @State private var showOrchard = false
    @State private var showJobs = false

    var body: some View {
        VStack(spacing: 10) {
            if let run = orchard {
                Button { showOrchard = true } label: {
                    row(icon: "leaf.fill", title: "Bringing up \(run.title)",
                        detail: run.progress?.title ?? OrchardWords.state(run.state).name,
                        value: run.progress?.overall,
                        trailing: run.progress?.overallEtaS.map { OrchardProgress.approx($0) + " left" })
                }
                .buttonStyle(.plain)
            }
            if let run = bringup {
                VStack(alignment: .leading, spacing: 10) {
                    row(icon: "hammer.fill", title: "Bringing up \(run.title)", detail: run.detail,
                        value: run.total > 0 ? Double(run.done) / Double(run.total) : nil, trailing: nil)
                    ForEach(run.approvals) { a in
                        VStack(alignment: .leading, spacing: 8) {
                            Text("\(run.agent) wants to \(a.kind == "command" ? "run" : "change") something that \(a.why):")
                                .font(.tt(.footnote)).foregroundStyle(.secondary)
                            if let cmd = a.command {
                                Text(cmd).font(.ttMono(12)).lineLimit(4)
                                    .padding(8).frame(maxWidth: .infinity, alignment: .leading)
                                    .background(.black.opacity(0.3), in: RoundedRectangle(cornerRadius: 8))
                            }
                            HStack {
                                Button("Allow") { decide(run, a, "accept") }.buttonStyle(.borderedProminent).tint(Brand.accent)
                                Button("Deny", role: .destructive) { decide(run, a, "decline") }.buttonStyle(.bordered)
                            }
                            .disabled(deciding)
                        }
                        .padding(12)
                        .background(Vesper.warning.opacity(0.12), in: RoundedRectangle(cornerRadius: 14))
                    }
                }
            }
            ForEach(jobs.filter(\.isActive)) { job in
                Button { showJobs = true } label: {
                    row(icon: "waveform", title: "Transcribing \(job.title)",
                        detail: job.status == "running" ? "Part \(job.done) of \(job.total)" : job.status.capitalized,
                        value: job.total > 0 ? Double(job.done) / Double(job.total) : nil, trailing: nil)
                }
                .buttonStyle(.plain)
            }
        }
        .task(id: session.box.id) {
            while !Task.isCancelled {
                orchard = (try? await session.orchard())?.runs.first(where: \.live)
                bringup = (try? await session.bringupRuns())?.first(where: \.live)
                jobs = (try? await session.jobs()) ?? []
                let busy = orchard != nil || bringup != nil || jobs.contains(where: \.isActive)
                try? await Task.sleep(for: .seconds(busy ? 10 : 30))
            }
        }
        .fullScreenCover(isPresented: $showOrchard) { OrchardView(session: session) }
        .sheet(isPresented: $showJobs) { JobsView(session: session) }
    }

    private func decide(_ run: BringupRun, _ a: BringupApproval, _ decision: String) {
        deciding = true
        Task {
            try? await session.decideBringup(run: run.id, approval: a.id, decision: decision)
            bringup = (try? await session.bringupRuns())?.first(where: \.live)
            deciding = false
        }
    }

    private func row(icon: String, title: String, detail: String, value: Double?, trailing: String?) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(systemName: icon).foregroundStyle(Brand.accent)
                VStack(alignment: .leading, spacing: 1) {
                    Text(title).font(.tt(.subheadline).weight(.semibold)).lineLimit(1)
                    Text(detail).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 0)
                if let trailing { Text(trailing).font(.tt(.caption).monospacedDigit()).foregroundStyle(.secondary) }
                Image(systemName: "chevron.right").foregroundStyle(.tertiary)
            }
            if let value { ProgressView(value: value).tint(Brand.accent) } else { ProgressView().progressViewStyle(.linear) }
        }
        .padding(14)
        .card(radius: 18)
    }
}

/// Things to do with the box beyond chat and pictures.
private struct ExploreGrid: View {
    let session: BoxSession
    @State private var open: Item?

    enum Item: String, Identifiable, CaseIterable {
        case code, research, routines, transcribe, orchard, lab, benchmark
        var id: String { rawValue }
        var title: String {
            switch self {
            case .code: return "Code"
            case .research: return "Research"
            case .routines: return "Routines"
            case .orchard: return "Bring up a model"
            case .transcribe: return "Transcribe"
            case .lab: return "Silicon check"
            case .benchmark: return "Benchmark"
            }
        }
        var detail: String {
            switch self {
            case .code: return "Start, follow and steer coding sessions"
            case .research: return "Reports with sources, from the web"
            case .routines: return "Jobs the box does on its own"
            case .orchard: return "Any Hugging Face model, tested on 1–4 chips"
            case .transcribe: return "Record in the room or upload, any length"
            case .lab: return "gpt-oss on bare silicon"
            case .benchmark: return "Speed and cost per token"
            }
        }
        var icon: String {
            switch self {
            case .code: return "chevron.left.forwardslash.chevron.right"
            case .research: return "magnifyingglass"
            case .routines: return "clock.arrow.circlepath"
            case .orchard: return "leaf"
            case .transcribe: return "waveform.badge.mic"
            case .lab: return "flask"
            case .benchmark: return "speedometer"
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionTitle(text: "Explore")
            LazyVGrid(columns: [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)], spacing: 10) {
                ForEach(Item.allCases) { item in
                    Button { open = item } label: {
                        VStack(alignment: .leading, spacing: 6) {
                            Image(systemName: item.icon).font(.tt(.title3)).foregroundStyle(Brand.accent)
                            Text(item.title).font(.tt(.subheadline).weight(.semibold))
                            Text(item.detail).font(.tt(.caption2)).foregroundStyle(.secondary)
                                .multilineTextAlignment(.leading).lineLimit(2)
                        }
                        .frame(maxWidth: .infinity, minHeight: 92, alignment: .topLeading)
                        .padding(14)
                        .card(radius: 18)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .fullScreenCover(item: Binding(get: { open == .orchard || open == .lab ? open : nil },
                                       set: { open = $0 })) { item in
            if item == .orchard { OrchardView(session: session) } else { LabView(session: session) }
        }
        .sheet(item: Binding(get: { open == .orchard || open == .lab ? nil : open },
                             set: { open = $0 })) { item in
            if item == .research {
                ResearchListView(session: session)
            } else if item == .transcribe {
                JobsView(session: session)
            } else if item == .code {
                CodeListView(session: session)
            } else if item == .routines {
                RoutinesListView(session: session)
            } else {
                NavigationStack {
                    ScrollView { BenchCard(session: session).padding(16) }
                        .background(Brand.backdrop)
                        .navigationTitle("Benchmark")
                        .navigationBarTitleDisplayMode(.inline)
                }
            }
        }
    }
}

/// Sleep and wake: the model (stays reachable) and the whole box (owners;
/// waking it needs Wake-on-LAN), from the Box tab's menu or Settings.
struct PowerSheet: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 16) {
                    ModelSleepCard(session: session)
                    PowerCard(session: session)
                }
                .padding(16)
            }
                .background(Brand.backdrop)
                .navigationTitle("Power")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
        .presentationDetents([.medium, .large])
    }
}

/// Serving state as a hero card: model, endpoint, mesh.
private struct ServingCard: View {
    let session: BoxSession
    let onChooseModel: () -> Void
    var onOpenCreate: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 14) {
                TTMark()
                    .fill(Brand.accent.gradient)
                    .frame(width: 30, height: 30)
                    .frame(width: 56, height: 56)
                    .glassEffect(.regular.tint(Brand.accent.opacity(0.35)), in: .rect(cornerRadius: 16))
                VStack(alignment: .leading, spacing: 4) {
                    Text(headline)
                        .font(.tt(.title3).weight(.semibold))
                        .lineLimit(1)
                        .minimumScaleFactor(0.7)
                    Text(subline)
                        .font(.tt(.caption))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                Spacer(minLength: 0)
            }
            HStack(spacing: 8) {
                if let mesh = session.status?.deviceMesh { Badge(text: mesh.uppercased(), icon: "square.grid.2x2") }
                if let chips = session.status?.chips { Badge(text: chips, icon: "cpu") }
                Badge(text: session.address, icon: "network")
            }
            if session.status != nil && !session.isServing && session.startingModel == nil {
                Button(action: onChooseModel) {
                    Label(session.startFailure == nil ? "Start a model" : "Choose a model", systemImage: "play.fill")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.large)
            } else if session.canMakeImages && session.isSplit {
                Button { onOpenCreate() } label: {
                    Label("Make images", systemImage: "wand.and.stars").frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.large)
            } else if session.isServing, let kind = session.servedKind, kind == "image" || kind == "video" {
                Button { onOpenCreate() } label: {
                    Label(kind == "image" ? "Make images" : "Make videos", systemImage: "wand.and.stars")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.large)
            }
        }
        .padding(18)
        .card(tint: Brand.accent.opacity(0.12), radius: 26)
    }

    private var headline: String {
        if session.statusError != nil { return "Unreachable" }
        if let starting = session.startingModel { return "Starting \(Brand.shortModel(starting))…" }
        if session.startFailure != nil { return "\(session.serve?.displayModel ?? "Model") failed to start" }
        if session.isSplit, let chat = session.serve?.displayModel, let image = session.companion?.displayModel {
            return "\(chat) + \(image)"
        }
        if session.isServing, let model = session.serve?.displayModel ?? session.status?.servingModel.map(Brand.shortModel) {
            return model
        }
        return session.status == nil ? "Connecting…" : "Idle"
    }

    private var subline: String {
        if let error = session.statusError { return error }
        if session.startingModel != nil {
            let elapsed = session.serve.map { " (\(Elapsed.string(since: $0.since)) so far)" } ?? ""
            return "Loading onto the chips\(elapsed). This can take a few minutes; you can leave the app."
        }
        if let failure = session.startFailure { return failure }
        if session.isSplit, let companion = session.companion {
            let chips = session.coserve?.layout
            let chat = "Chat on chips \(chips?.chat ?? "0,1")"
            let image = "\(companion.displayModel ?? "images") on \(chips?.image ?? "2,3")"
            switch companion.phase {
            case "starting": return "\(chat) · \(image) loading (\(Elapsed.string(since: companion.since)))"
            case "failed": return "\(chat) · \(companion.displayModel ?? "Image model") failed: \(companion.error ?? "unknown")"
            default: return "\(chat) · \(image)"
            }
        }
        if session.isServing, let served = session.status?.servingModel {
            switch session.servedKind {
            case "image": return "Making images · \(served)"
            case "video": return "Making video · \(served)"
            case "speech": return "Speech-to-text · \(served)"
            case "tts": return "Text-to-speech · \(served)"
            default: return "Serving \(served)"
            }
        }
        return "Nothing is serving right now."
    }

    private struct Badge: View {
        let text: String
        let icon: String
        var body: some View {
            Label(text, systemImage: icon)
                .font(.ttMono(11, .semibold, relativeTo: .caption2))
                .lineLimit(1)
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
                .glassEffect(.regular, in: .capsule)
        }
    }
}

/// One chip: big temperature, a heat bar, power and AI clock.
private struct ChipCard: View {
    let chip: ChipReading
    var live = true

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Chip \(chip.index)").font(.tt(.caption).weight(.semibold)).foregroundStyle(.secondary)
                Spacer()
                if let board = chip.boardType {
                    Text(board.uppercased()).font(.ttMono(11, relativeTo: .caption2)).foregroundStyle(.tertiary)
                }
            }
            HStack(alignment: .firstTextBaseline, spacing: 2) {
                Text(chip.tempC.map { "\(Int(Temp.value($0).rounded()))" } ?? "–")
                    .font(.system(size: 34, weight: .semibold, design: .rounded).monospacedDigit())
                Text(Temp.symbol).font(.tt(.subheadline)).foregroundStyle(.secondary)
            }
            HeatBar(celsius: chip.tempC, throttle: chip.throttleC ?? 90)
            Text(thermalLine)
                .font(.tt(.caption2))
                .foregroundStyle(.secondary)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Label(chip.powerW.map { "\(Int($0.rounded())) W" } ?? "–", systemImage: "bolt.fill")
                Spacer()
                Label(chip.aiclkMHz.map { "\(Int($0.rounded())) MHz" } ?? "–", systemImage: "waveform.path")
            }
            .font(.tt(.caption2).monospacedDigit())
            .foregroundStyle(.secondary)
            .labelStyle(.titleAndIcon)
        }
        .padding(14)
        .opacity(live ? 1 : 0.45)
        .card(radius: 20)
    }

    /// "Memory 36 °C · slows down at 90 °C": a second sensor to compare, and how
    /// much headroom there is.
    private var thermalLine: String {
        var parts: [String] = []
        if let mem = chip.memTempC { parts.append("Memory \(Temp.string(mem))") }
        parts.append("slows down at \(Temp.string(chip.throttleC ?? 90))")
        return parts.joined(separator: " · ")
    }
}

/// Room temperature (25 °C) on the left, the throttle point on the right.
private struct HeatBar: View {
    let celsius: Double?
    var throttle: Double = 90

    var body: some View {
        GeometryReader { proxy in
            let fraction = min(max(((celsius ?? 0) - 25) / max(throttle - 25, 1), 0), 1)
            ZStack(alignment: .leading) {
                Capsule().fill(Color.white.opacity(0.08))
                Capsule()
                    .fill(Brand.tempColor(celsius ?? 0).gradient)
                    .frame(width: proxy.size.width * fraction)
            }
        }
        .frame(height: 5)
        .animation(.easeInOut(duration: 0.6), value: celsius)
    }
}

private struct LogCard: View {
    let lines: [String]
    let error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let error {
                Text(error).font(.tt(.footnote)).foregroundStyle(.red)
            } else if lines.isEmpty {
                Text("No log lines yet.").font(.tt(.footnote)).foregroundStyle(.secondary)
            } else {
                ForEach(Array(lines.suffix(14).enumerated()), id: \.offset) { _, line in
                    Text(line)
                        .font(.ttMono(10.5))
                        .foregroundStyle(line.contains("ERROR") || line.contains("Traceback") ? Color.red
                                         : line.contains("WARN") ? Color.yellow : Color.secondary)
                        .lineLimit(2)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
        .padding(14)
        .card(radius: 18)
        .textSelection(.enabled)
    }
}

/// Says when the chip readings aren't live, and why, instead of showing old
/// numbers as if they were current.
struct TelemetryBanner: View {
    let telemetry: TelemetryFeed

    var body: some View {
        TimelineView(.periodic(from: .now, by: 5)) { context in
            if !telemetry.isLive(at: context.date), telemetry.updatedAt != nil || telemetry.unavailable != nil {
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "pause.circle.fill").foregroundStyle(.orange)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Chip readings aren't live").font(.tt(.subheadline).weight(.semibold))
                        Text(detail(at: context.date)).font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                }
                .padding(12)
                .card(radius: 16)
            }
        }
    }

    private func detail(at now: Date) -> String {
        let reason = telemetry.unavailable ?? "The box hasn't sent a reading lately."
        guard let at = telemetry.updatedAt else { return reason }
        let age = Int(now.timeIntervalSince(at))
        let ago = age < 90 ? "\(age) s" : age < 5400 ? "\(age / 60) min" : "\(age / 3600) h"
        return reason + " The numbers below are from \(ago) ago."
    }
}

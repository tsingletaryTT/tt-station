// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// The Models tab: the box's model catalog (from the official tt CLI). Download
/// models onto the box, watch the download, and start one serving. Starting asks
/// first, because it replaces whatever is serving -- including the assistant's brain.
struct ModelsView: View {
    @Environment(AppModel.self) private var app
    @State private var query = ""
    @State private var orchard = false

    var body: some View {
        NavigationStack {
            Group {
                if let session = app.session {
                    ModelsContent(session: session, query: query, onOrchard: { orchard = true })
                } else {
                    ContentUnavailableView("No box selected", systemImage: "server.rack",
                                           description: Text("Add or pick a box in Settings."))
                }
            }
            .background(Brand.backdrop)
            .navigationTitle("Models")
            .searchable(text: $query, prompt: "Search models")
            .toolbar {
                if app.session != nil {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button { orchard = true } label: {
                            Label("Bring up a new model", systemImage: "leaf")
                        }
                    }
                }
            }
            .fullScreenCover(isPresented: $orchard) {
                if let session = app.session { OrchardView(session: session) }
            }
        }
    }
}

private struct ModelsContent: View {
    let session: BoxSession
    let query: String
    var onOrchard: () -> Void = {}
    @Environment(AppModel.self) private var app
    @State private var pendingStart: TtModel?
    @State private var pendingDelete: TtModel?
    @State private var actionError: String?
    @State private var showEverything = false

    /// Models that drove the assistant reliably in testing (tool calling, 10/10).
    private static let recommended: Set<String> = ["Qwen3-32B", "Qwen/Qwen3-32B"]

    var body: some View {
        Group {
            if let catalog = session.catalog {
                list(catalog)
            } else if let error = session.catalogError {
                ContentUnavailableView {
                    Label("Couldn't load models", systemImage: "exclamationmark.triangle")
                } description: {
                    Text(error)
                } actions: {
                    Button("Try again") { Task { await session.loadCatalog(force: true) } }
                        .buttonStyle(.glassProminent)
                }
            } else {
                ProgressView("Loading models…")
            }
        }
        .task(id: session.box.id) { await session.loadCatalog() }
    }

    private func list(_ catalog: [TtModel]) -> some View {
        let filtered = query.isEmpty ? catalog : catalog.filter {
            $0.name.localizedCaseInsensitiveContains(query) || $0.modelType.localizedCaseInsensitiveContains(query)
        }
        let downloading = filtered.filter { session.pull(for: $0)?.isRunning == true }
        let onBox = filtered.filter { $0.isReady && $0.weightsState != "container" && session.pull(for: $0)?.isRunning != true }
            .sorted { rank($0) < rank($1) }
        let available = filtered.filter {
            ($0.weightsState == "missing" || $0.weightsState == "partial" || $0.weightsState == "container")
                && $0.supported && session.pull(for: $0)?.isRunning != true
        }
            .sorted { rank($0) < rank($1) }
        let serving = session.status?.servingModel

        return ScrollView {
            VStack(spacing: 22) {
                if let starting = session.startingModel {
                    StartingCard(model: starting, since: session.serve?.since, eta: session.serve?.etaS)
                }
                ModesStrip(session: session)
                if let note = session.access?.modelsNote {
                    Label(note, systemImage: "lock")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(radius: 16)
                }
                Button { showEverything = true } label: {
                    HStack(spacing: 12) {
                        Image(systemName: "square.stack.3d.down.right").font(.tt(.title3)).foregroundStyle(Brand.accent)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Everything it can serve").font(.tt(.headline))
                            Text("Community packages and their profiles, native bundles, and what's serving now.")
                                .font(.tt(.caption)).foregroundStyle(.secondary).multilineTextAlignment(.leading)
                        }
                        Spacer(minLength: 0)
                        Image(systemName: "chevron.right").foregroundStyle(.tertiary)
                    }
                    .padding(16)
                    .card()
                }
                .buttonStyle(.plain)
                if let failure = session.startFailure {
                    Label("\(session.serve?.displayModel ?? "The model") couldn't start: \(failure)",
                          systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }
                StorageCard(freeBytes: session.diskFreeBytes,
                            onBox: catalog.filter { $0.weightsState == "complete" }.count,
                            downloading: downloading.count, canDownload: session.canDownload)
                if let actionError {
                    Label(actionError, systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }
                if filtered.isEmpty {
                    ContentUnavailableView.search(text: query)
                }
                section("Downloading", downloading, serving: serving)
                section("On this box", onBox, serving: serving)
                section(session.canDownload ? "More for this box" : "Not downloaded", available, serving: serving)
                Button(action: onOrchard) {
                    HStack(spacing: 12) {
                        Image(systemName: "leaf").font(.tt(.title3)).foregroundStyle(Brand.accent)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(query.isEmpty ? "Not in the list?" : "Can't find \(query)?").font(.tt(.headline))
                            Text("Bring up any Hugging Face model on this box with Orchard. It runs for hours and tells you when it's done.")
                                .font(.tt(.caption)).foregroundStyle(.secondary).multilineTextAlignment(.leading)
                        }
                        Spacer(minLength: 0)
                        Image(systemName: "chevron.right").foregroundStyle(.tertiary)
                    }
                    .padding(16)
                    .card()
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
        }
        .scrollIndicators(.hidden)
        .refreshable { await session.loadCatalog(force: true) }
        .sheet(isPresented: $showEverything) { ServeAnyView(session: session) }
        .confirmationDialog("Delete \(pendingDelete.map { Brand.shortModel($0.name) } ?? "")?",
                            isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }),
                            titleVisibility: .visible, presenting: pendingDelete) { model in
            Button("Delete \(ByteFormat.string(model.downloadedBytes ?? 0))", role: .destructive) {
                run { _ = try await session.removeModel(model) }
            }
        } message: { model in
            Text("Removes \(Brand.shortModel(model.name))'s downloaded weights from the box. You can download it again later.")
        }
        .confirmationDialog("Start \(pendingStart.map { Brand.shortModel($0.name) } ?? "")?",
                            isPresented: Binding(get: { pendingStart != nil }, set: { if !$0 { pendingStart = nil } }),
                            titleVisibility: .visible, presenting: pendingStart) { model in
            Button("Start \(Brand.shortModel(model.name))") {
                run {
                    try await session.start(model: model.name)
                    app.selectedTab = AppModel.tab(for: model.kind)
                }
            }
        } message: { model in
            Text(startMessage(model, serving: serving))
        }
    }

    private func isServing(_ model: TtModel, serving: String?) -> Bool {
        session.isServing
            && (session.serve?.model.map(Brand.shortModel) == model.name
                || Brand.shortModel(serving ?? "") == model.name || serving == model.hfRepo)
    }

    /// Recommended first, then models that work with the assistant, then by name.
    private func rank(_ model: TtModel) -> String {
        let tier = Self.recommended.contains(model.name) ? "0" : model.worksWithAssistant ? "1" : model.isChatModel ? "2" : "3"
        return tier + model.name.lowercased()
    }

    @ViewBuilder
    private func section(_ title: String, _ models: [TtModel], serving: String?) -> some View {
        if !models.isEmpty {
            VStack(spacing: 10) {
                SectionTitle(text: title)
                GlassEffectContainer(spacing: 10) {
                    VStack(spacing: 10) {
                        ForEach(models) { model in
                            ModelCard(
                                model: model,
                                pull: session.pull(for: model),
                                isServing: session.isServing
                                    && (session.serve?.model.map(Brand.shortModel) == model.name
                                        || Brand.shortModel(serving ?? "") == model.name || serving == model.hfRepo),
                                isRecommended: Self.recommended.contains(model.name),
                                canDownload: session.canDownload,
                                isBusy: session.startingModel != nil,
                                onStart: { pendingStart = model },
                                onDownload: { run { try await session.download(model) } },
                                onCancel: { run { try await session.cancelDownload(model) } }
                            )
                            .contextMenu {
                                if model.weightsState == "complete" && !isServing(model, serving: serving) {
                                    Button("Delete from box", systemImage: "trash", role: .destructive) {
                                        pendingDelete = model
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    private func run(_ work: @escaping () async throws -> Void) {
        actionError = nil
        Task {
            do { try await work() } catch { actionError = error.localizedDescription }
        }
    }

    private func startMessage(_ model: TtModel, serving: String?) -> String {
        var parts: [String] = []
        if let serving {
            parts.append("This replaces \(Brand.shortModel(serving)), and the assistant is offline until the new model is ready.")
        }
        switch model.kind {
        case "image": parts.append("You'll make images in the Create tab; chat is unavailable while it serves.")
        case "video": parts.append("You'll make videos in the Create tab; chat is unavailable while it serves.")
        default:
            if !model.worksWithAssistant {
                parts.append("This model can't drive the assistant's box controls, so expect plain chat only.")
            }
        }
        parts.append("Loading usually takes a few minutes.")
        return parts.joined(separator: " ")
    }
}

/// Free disk and what's on the box, at the top of the tab.
private struct StorageCard: View {
    let freeBytes: Int64?
    let onBox: Int
    let downloading: Int
    let canDownload: Bool

    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: "internaldrive.fill")
                .font(.tt(.title2))
                .foregroundStyle(Brand.accent)
                .frame(width: 48, height: 48)
                .glassEffect(.regular.tint(Brand.accent.opacity(0.3)), in: .rect(cornerRadius: 14))
            VStack(alignment: .leading, spacing: 3) {
                Text(freeBytes.map { "\(ByteFormat.string($0)) free" } ?? "Storage")
                    .font(.tt(.headline))
                Text(subtitle).font(.tt(.caption)).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(16)
        .card(tint: Brand.accent.opacity(0.1))
    }

    private var subtitle: String {
        var parts = ["\(onBox) model\(onBox == 1 ? "" : "s") on the box"]
        if downloading > 0 { parts.append("\(downloading) downloading") }
        if !canDownload { parts.append("downloads need the tt CLI on the box") }
        return parts.joined(separator: " · ")
    }
}

private struct ModelCard: View {
    let model: TtModel
    let pull: PullJob?
    let isServing: Bool
    let isRecommended: Bool
    let canDownload: Bool
    let isBusy: Bool
    let onStart: () -> Void
    let onDownload: () -> Void
    let onCancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: icon)
                    .font(.tt(.headline))
                    .foregroundStyle(Brand.accent)
                    .frame(width: 36, height: 36)
                    .glassEffect(.regular.tint(Brand.accent.opacity(isRecommended ? 0.45 : 0.2)), in: .rect(cornerRadius: 10))
                VStack(alignment: .leading, spacing: 5) {
                    Text(Brand.shortModel(model.name))
                        .font(.tt(.body).weight(.semibold))
                        .lineLimit(1)
                    tags
                }
                Spacer(minLength: 6)
                trailing
            }
            if let pull, pull.isRunning {
                VStack(alignment: .leading, spacing: 6) {
                    ShimmerBar()
                    HStack {
                        Text("\(ByteFormat.string(pull.bytesOnDisk)) downloaded")
                        Spacer()
                        Text(elapsed(since: pull.startedAt))
                    }
                    .font(.tt(.caption).monospacedDigit())
                    .foregroundStyle(.secondary)
                }
            } else if let pull, pull.state == "failed" {
                Label(pull.error ?? "The download failed.", systemImage: "exclamationmark.triangle.fill")
                    .font(.tt(.caption))
                    .foregroundStyle(.red)
                    .lineLimit(5)
                if let link = pull.actionURL.flatMap(URL.init(string:)) {
                    LicenseFix(url: link, onRetry: onDownload)
                }
            } else if model.weightsState == "partial" {
                Text("Partly downloaded · \(ByteFormat.string(model.downloadedBytes ?? 0)). Resume to finish it.")
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            } else if model.weightsState == "container" && !canStartHere {
                Text("Speech models aren't usable from the app yet.")
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            } else if model.weightsState == "container" {
                Text("Downloads its own weights when it starts.")
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            } else if model.weightsState == "missing", let gb = model.minDiskGB {
                Text("Needs about \(Int(gb.rounded())) GB of disk")
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .card(tint: isServing ? Color.green.opacity(0.12) : nil, radius: 20)
    }

    private var tags: some View {
        HStack(spacing: 6) {
            Tag(text: typeLabel)
            if let status = model.status { Tag(text: status.capitalized, color: statusColor(status)) }
            if isRecommended {
                Tag(text: "Best for chat", color: Brand.accent)
            } else if model.worksWithAssistant {
                Tag(text: "Chat-ready", color: Brand.accent)
            }
        }
    }

    @ViewBuilder private var trailing: some View {
        if isServing {
            StatusPill(state: .serving, text: "Serving")
        } else if pull?.isRunning == true {
            Button("Cancel", role: .cancel, action: onCancel)
                .buttonStyle(.glass)
                .controlSize(.small)
        } else if model.isReady {
            if canStartHere {
                Button("Start", action: onStart)
                    .buttonStyle(.glassProminent)
                    .controlSize(.small)
                    .disabled(isBusy)
            } else {
                Text(model.weightsState == "container" ? "" : "Downloaded").font(.tt(.caption)).foregroundStyle(.secondary)
            }
        } else if canDownload {
            Button(action: onDownload) {
                Label(pull?.state == "failed" ? "Retry" : model.weightsState == "partial" ? "Resume" : "Get",
                      systemImage: "arrow.down")
            }
            .buttonStyle(.glass)
            .controlSize(.small)
        }
    }

    /// The app has an experience for chat, image and video models; speech/TTS
    /// models can be downloaded but not used from here yet.
    private var canStartHere: Bool { ["chat", "image", "video", "other"].contains(model.kind) }

    private var icon: String {
        if isRecommended { return "star.fill" }
        switch model.modelType {
        case "llm": return "text.bubble.fill"
        case "audio": return "waveform"
        case "image": return "photo.fill"
        case "video": return "film.fill"
        case "text_to_speech": return "speaker.wave.2.fill"
        default: return "cube.fill"
        }
    }

    private var typeLabel: String {
        switch model.modelType {
        case "llm": return model.paramCount.map { "LLM · \(Int($0.rounded()))B" } ?? "LLM"
        case "audio": return "Speech"
        case "image": return "Image"
        case "video": return "Video"
        case "text_to_speech": return "Voice"
        default: return model.modelType.capitalized
        }
    }

    private func statusColor(_ status: String) -> Color {
        switch status.uppercased() {
        case "COMPLETE": return .green
        case "FUNCTIONAL": return .blue
        default: return .orange
        }
    }

    private func elapsed(since start: Int) -> String {
        let seconds = max(0, Int(Date().timeIntervalSince1970) - start)
        return seconds < 60 ? "\(seconds)s" : "\(seconds / 60)m \(seconds % 60)s"
    }
}

private struct Tag: View {
    let text: String
    var color: Color = .secondary

    var body: some View {
        Text(text)
            .font(.tt(.caption2).weight(.semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(color.opacity(0.14), in: Capsule())
    }
}

enum ByteFormat {
    static func string(_ bytes: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: bytes, countStyle: .decimal)
    }
}

/// An indeterminate progress bar: a purple highlight sweeping along a track. `tt`
/// doesn't report a total, so there is no percentage to show.
struct ShimmerBar: View {
    var body: some View {
        TimelineView(.animation) { context in
            let phase = context.date.timeIntervalSinceReferenceDate.truncatingRemainder(dividingBy: 1.6) / 1.6
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.white.opacity(0.08))
                    Capsule()
                        .fill(LinearGradient(colors: [.clear, Brand.accent, .clear], startPoint: .leading, endPoint: .trailing))
                        .frame(width: proxy.size.width * 0.35)
                        .offset(x: proxy.size.width * (phase * 1.35 - 0.35))
                }
                .clipShape(Capsule())
            }
        }
        .frame(height: 5)
    }
}

/// A gated model's two-step fix, made obvious: open the license page; once the
/// user is back, one prominent button retries the download.
struct LicenseFix: View {
    let url: URL
    let onRetry: () -> Void
    @Environment(\.openURL) private var openURL
    @State private var opened = false

    var body: some View {
        if opened {
            Button(action: onRetry) {
                Label("I've accepted it — download now", systemImage: "arrow.down.circle.fill")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .controlSize(.regular)
        } else {
            Button {
                opened = true
                openURL(url)
            } label: {
                Label("Accept the license on Hugging Face", systemImage: "arrow.up.right.square")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glass)
            .controlSize(.regular)
        }
    }
}

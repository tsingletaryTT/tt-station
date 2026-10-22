// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import AVKit
import Photos
import SwiftUI

/// The Create tab: the image studio while an image model serves, and otherwise
/// a launcher for getting one going.
struct CreateView: View {
    @Environment(AppModel.self) private var app
    @State private var transcribing = false

    var body: some View {
        NavigationStack {
            Group {
                if let session = app.session {
                    if session.canMakeImages {
                        ImageStudio(session: session)
                    } else {
                        CreateLauncher(session: session)
                    }
                } else {
                    ContentUnavailableView("No box selected", systemImage: "server.rack",
                                           description: Text("Add or pick a box in Settings."))
                }
            }
            .background(Brand.backdrop)
            .navigationTitle("Create")
            .toolbar {
                if app.session != nil {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button { transcribing = true } label: {
                            Label("Transcribe", systemImage: "waveform.badge.mic")
                        }
                    }
                }
            }
            .sheet(isPresented: $transcribing, onDismiss: { app.incomingRecording = nil }) {
                if let session = app.session { JobsView(session: session, incoming: app.incomingRecording) }
            }
            // A recording opened in TT Station from another app.
            .onChange(of: app.incomingRecording, initial: true) { _, url in
                if url != nil, app.session != nil { transcribing = true }
            }
        }
    }
}

// MARK: - Launcher

/// No image model is serving: explain the one-model-at-a-time box, offer the
/// image models it has (start, or download first), show the gallery.
private struct CreateLauncher: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @State private var actionError: String?

    var body: some View {
        ScrollView {
            VStack(spacing: 22) {
                if let starting = session.startingModel {
                    StartingCard(model: starting, since: session.serve?.since, eta: session.serve?.etaS)
                } else {
                    intro
                }
                if let failure = session.startFailure {
                    Label(failure, systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }
                if let actionError {
                    Label(actionError, systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }
                modelSection("Images", kind: "image", icon: "photo.on.rectangle.angled")
                GalleryGrid(title: "Your creations", kind: nil)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
        }
        .scrollIndicators(.hidden)
        .task(id: session.box.id) {
            await session.loadCatalog()
        }
        .refreshable {
            await session.loadCatalog(force: true)
        }
    }

    private var intro: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Make images on your box", systemImage: "wand.and.stars")
                .font(.tt(.headline))
            Text(introText)
                .font(.tt(.subheadline))
                .foregroundStyle(.secondary)
        }
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .card(tint: Brand.accent.opacity(0.12))
    }

    private var introText: String {
        let serving = session.status?.servingModel.map(Brand.shortModel)
        let tail = "Your box runs one model at a time, so starting one replaces "
            + (serving.map { "\($0), and chat is offline until you switch back." } ?? "whatever is serving.")
        return "Start an image model below. " + tail
    }

    @ViewBuilder
    private func modelSection(_ title: String, kind: String, icon: String) -> some View {
        let models = (session.catalog ?? []).filter { $0.kind == kind && $0.supported }
        if !models.isEmpty {
            VStack(spacing: 10) {
                SectionTitle(text: title)
                ForEach(models) { model in
                    LaunchRow(model: model, icon: icon, pull: session.pull(for: model),
                              isBusy: session.startingModel != nil) { action in
                        Task { await perform(action, model) }
                    }
                }
            }
        }
    }

    private func perform(_ action: CreateLauncherAction, _ model: TtModel) async {
        actionError = nil
        do {
            switch action {
            case .start: try await session.start(model: model.name)
            case .download: try await session.download(model)
            case .cancel: try await session.cancelDownload(model)
            }
        } catch {
            actionError = error.localizedDescription
        }
    }
}

private struct LaunchRow: View {
    let model: TtModel
    let icon: String
    let pull: PullJob?
    let isBusy: Bool
    let onAction: (CreateLauncherAction) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 12) {
                Image(systemName: icon)
                    .foregroundStyle(Brand.accent)
                    .frame(width: 36, height: 36)
                    .glassEffect(.regular.tint(Brand.accent.opacity(0.25)), in: .rect(cornerRadius: 10))
                VStack(alignment: .leading, spacing: 2) {
                    Text(Brand.shortModel(model.name)).font(.tt(.body).weight(.semibold))
                    Text(subtitle).font(.tt(.caption)).foregroundStyle(.secondary)
                }
                Spacer()
                button
            }
            if pull?.isRunning == true {
                ShimmerBar()
            }
            if pull?.state == "failed", let link = pull?.actionURL.flatMap(URL.init(string:)) {
                LicenseFix(url: link) { onAction(.download) }
            }
        }
        .padding(14)
        .card(radius: 20)
    }

    private var subtitle: String {
        if let pull, pull.isRunning { return "Downloading · \(ByteFormat.string(pull.bytesOnDisk))" }
        if let pull, pull.state == "failed" { return pull.error ?? "Download failed" }
        switch model.weightsState {
        case "complete", "container": return "Ready"
        case "partial": return "Partly downloaded · \(ByteFormat.string(model.downloadedBytes ?? 0))"
        default: return model.minDiskGB.map { "Download first · about \(Int($0.rounded())) GB" } ?? "Download first"
        }
    }

    @ViewBuilder private var button: some View {
        if pull?.isRunning == true {
            Button("Cancel") { onAction(.cancel) }.buttonStyle(.glass).controlSize(.small)
        } else if model.isReady {
            Button("Start") { onAction(.start) }.buttonStyle(.glassProminent).controlSize(.small).disabled(isBusy)
        } else {
            Button(model.weightsState == "partial" ? "Resume" : "Get") { onAction(.download) }
                .buttonStyle(.glass).controlSize(.small)
        }
    }
}

enum CreateLauncherAction { case start, download, cancel }

/// A model coming up: what, how long so far, and that it's safe to leave.
struct StartingCard: View {
    let model: String
    let since: Int?
    /// How long this model usually takes to load on this box.
    var eta: Int? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                ProgressView()
                Text("Starting \(Brand.shortModel(model))").font(.tt(.headline))
                Spacer()
                if let since {
                    TimelineView(.periodic(from: .now, by: 1)) { context in
                        Text(Elapsed.string(since: since, now: context.date))
                            .font(.tt(.subheadline).monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
            }
            ShimmerBar()
            TimelineView(.periodic(from: .now, by: 10)) { context in
                Text(remaining(now: context.date))
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            }
        }
        .padding(18)
        .card(tint: Brand.accent.opacity(0.15))
    }

    private func remaining(now: Date) -> String {
        let tail = " You can leave the app; it keeps going on the box, and you'll get a notification."
        guard let eta, let since else {
            return "Loading a model onto the chips takes a few minutes." + tail
        }
        let left = eta - (Int(now.timeIntervalSince1970) - since)
        if left > 60 { return "Usually ready in about \(Int((Double(left) / 60).rounded(.up))) min." + tail }
        if left > -120 { return "Almost ready." + tail }
        return "Taking longer than usual; Box Doctor can check on it." + tail
    }
}

enum Elapsed {
    static func string(since: Int, now: Date = Date()) -> String {
        let seconds = max(0, Int(now.timeIntervalSince1970) - since)
        return seconds < 60 ? "\(seconds)s" : "\(seconds / 60)m \(String(format: "%02d", seconds % 60))s"
    }
}

// MARK: - Gallery

/// Creations as a grid (all of them, or one kind); tap for the viewer.
struct GalleryGrid: View {
    let title: String
    let kind: Creation.Kind?
    var onReuse: ((Creation) -> Void)?
    @Environment(AppModel.self) private var app
    @State private var selected: Creation?

    private let columns = [GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8)]

    var body: some View {
        let items = kind.map { app.creations.items($0) } ?? app.creations.items
        if !items.isEmpty {
            VStack(spacing: 10) {
                SectionTitle(text: title)
                LazyVGrid(columns: columns, spacing: 8) {
                    ForEach(items) { creation in
                        Button { selected = creation } label: { GalleryTile(creation: creation) }
                            .buttonStyle(.plain)
                    }
                }
            }
            .sheet(item: $selected) { creation in
                CreationViewer(creation: creation, onReuse: onReuse)
            }
        }
    }
}

private struct GalleryTile: View {
    let creation: Creation
    @Environment(AppModel.self) private var app

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            Rectangle().fill(Color.white.opacity(0.06))
            if let image = app.creations.image(for: creation) {
                Image(uiImage: image).resizable().scaledToFill()
            } else if creation.kind == .video, let url = app.creations.url(for: creation) {
                VideoThumbnail(url: url)
            }
            if creation.kind == .video {
                Image(systemName: creation.isPending ? "hourglass" : "play.circle.fill")
                    .font(.tt(.title2))
                    .foregroundStyle(.white)
                    .shadow(radius: 4)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            if creation.status == "failed" || creation.status == "cancelled" {
                Label(creation.status.capitalized, systemImage: "xmark.octagon.fill")
                    .font(.tt(.caption2).weight(.semibold))
                    .padding(6)
                    .background(.red.opacity(0.7), in: Capsule())
                    .padding(6)
            }
        }
        .aspectRatio(1, contentMode: .fit)
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
    }
}

/// A frame from a video file, for its tile.
struct VideoThumbnail: View {
    let url: URL
    @State private var image: UIImage?

    var body: some View {
        Group {
            if let image { Image(uiImage: image).resizable().scaledToFill() } else { Color.black }
        }
        .task(id: url) {
            let generator = AVAssetImageGenerator(asset: AVURLAsset(url: url))
            generator.appliesPreferredTrackTransform = true
            if let frame = try? await generator.image(at: CMTime(seconds: 0.5, preferredTimescale: 600)).image {
                image = UIImage(cgImage: frame)
            }
        }
    }
}

/// Full-screen view of one creation: play/zoom, save to Photos, share, reuse, delete.
struct CreationViewer: View {
    let creation: Creation
    var onReuse: ((Creation) -> Void)?
    @Environment(AppModel.self) private var app
    @Environment(\.dismiss) private var dismiss
    @State private var saveMessage: String?

    var body: some View {
        NavigationStack {
            VStack(spacing: 16) {
                media
                    .frame(maxWidth: .infinity)
                    .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
                VStack(alignment: .leading, spacing: 6) {
                    Text(creation.prompt).font(.tt(.body))
                    Text(details).font(.tt(.caption)).foregroundStyle(.secondary)
                    if let error = creation.error {
                        Text(error).font(.tt(.caption)).foregroundStyle(.red)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if let saveMessage {
                    Text(saveMessage).font(.tt(.footnote)).foregroundStyle(.secondary)
                }
                Spacer()
                GlassEffectContainer(spacing: 10) {
                    HStack(spacing: 10) {
                        if let url = app.creations.url(for: creation) {
                            Button { Task { await saveToPhotos(url) } } label: {
                                Label("Save", systemImage: "square.and.arrow.down").frame(maxWidth: .infinity)
                            }
                            .buttonStyle(.glassProminent)
                            ShareLink(item: url) {
                                Label("Share", systemImage: "square.and.arrow.up").frame(maxWidth: .infinity)
                            }
                            .buttonStyle(.glass)
                        }
                        if let onReuse {
                            Button {
                                onReuse(creation)
                                dismiss()
                            } label: {
                                Label("Reuse", systemImage: "arrow.uturn.backward").frame(maxWidth: .infinity)
                            }
                            .buttonStyle(.glass)
                        }
                    }
                    .controlSize(.large)
                }
            }
            .padding(16)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .destructiveAction) {
                    Button(role: .destructive) {
                        app.creations.delete(creation)
                        dismiss()
                    } label: { Image(systemName: "trash") }
                }
            }
        }
    }

    @ViewBuilder private var media: some View {
        if creation.kind == .image, let image = app.creations.image(for: creation) {
            Image(uiImage: image).resizable().scaledToFit()
        } else if creation.kind == .video, let url = app.creations.url(for: creation) {
            VideoPlayer(player: AVPlayer(url: url)).aspectRatio(16 / 9, contentMode: .fit)
        } else {
            ContentUnavailableView(creation.isPending ? "Still rendering on the box" : "No file",
                                   systemImage: creation.isPending ? "hourglass" : "questionmark.square.dashed")
                .frame(height: 240)
        }
    }

    private var details: String {
        var parts = [Brand.shortModel(creation.model), "\(creation.steps) steps"]
        if let seed = creation.seed { parts.append("seed \(seed)") }
        if let seconds = creation.seconds { parts.append(String(format: "%.1fs", seconds)) }
        parts.append(creation.createdAt.formatted(date: .abbreviated, time: .shortened))
        return parts.joined(separator: " · ")
    }

    private func saveToPhotos(_ url: URL) async {
        let status = await PHPhotoLibrary.requestAuthorization(for: .addOnly)
        guard status == .authorized || status == .limited else {
            saveMessage = "Allow TT Station to add to Photos in Settings to save."
            return
        }
        do {
            try await PHPhotoLibrary.shared().performChanges {
                if creation.kind == .video {
                    _ = PHAssetChangeRequest.creationRequestForAssetFromVideo(atFileURL: url)
                } else {
                    _ = PHAssetChangeRequest.creationRequestForAssetFromImage(atFileURL: url)
                }
            }
            saveMessage = "Saved to Photos."
        } catch {
            saveMessage = "Couldn't save: \(error.localizedDescription)"
        }
    }
}

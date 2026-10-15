import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Long jobs on the box -- transcribe a two-hour recording and walk away. The
/// box queues them, switches itself to Whisper when it may, and sends a
/// notification when the transcript is ready.
struct JobsView: View {
    let session: BoxSession
    /// A recording handed over from another app, to confirm and send.
    var incoming: URL? = nil
    @Environment(AppModel.self) private var app
    @Environment(\.dismiss) private var dismiss
    @State private var jobs: [BoxJob] = []
    @State private var importing = false
    @State private var picked: URL?
    /// The upload's name when it isn't the file's (a recording).
    @State private var pickedName: String?
    @State private var recorder = AudioRecorder()
    /// A recording whose upload failed: kept so it can be sent again.
    @State private var unsent: (url: URL, name: String)?
    @State private var uploading: String?
    @State private var error: String?
    @State private var viewing: BoxJob?

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Record a meeting or a note, or pick a file, and transcribe it on your box: any length. It switches itself to Whisper when it's free, or at night if you choose, and notifies you when it's done. Transcripts are added to Documents so you can ask the assistant about them.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                }
                if let uploading {
                    Section {
                        HStack(spacing: 12) {
                            ProgressView()
                            Text("Sending \(uploading) to the box…").foregroundStyle(.secondary)
                        }
                    }
                }
                if let error {
                    Section { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange) }
                }
                if let unsent, uploading == nil {
                    Section {
                        Label("\(unsent.name) hasn't reached the box yet. It's saved on this phone.", systemImage: "icloud.slash")
                        Button("Send it again") {
                            pickedName = unsent.name
                            picked = unsent.url
                        }
                    }
                }
                Section {
                    if jobs.isEmpty && uploading == nil {
                        Text("No jobs yet.").foregroundStyle(.secondary)
                    }

                    ForEach(jobs) { job in
                        JobRow(job: job, onRunNow: { Task { await runNow(job) } }, onOpen: { viewing = job })
                            .swipeActions {
                                Button(job.isActive ? "Cancel" : "Delete", role: .destructive) {
                                    Task { await delete(job) }
                                }
                            }
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            // Outside the list: two buttons in one list row share a single tap,
            // which opened both the recorder and the file picker (so neither).
            .safeAreaInset(edge: .bottom) {
                if recorder.recording {
                    RecordingBar(recorder: recorder, onStop: stopRecording, onDiscard: { recorder.discard() })
                        .padding(.horizontal, 16)
                        .padding(.bottom, 8)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                } else if uploading == nil {
                    HStack(spacing: 12) {
                        Button { Task { await startRecording() } } label: {
                            Label("Record", systemImage: "mic.fill").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glassProminent)
                        .controlSize(.large)
                        Button { importing = true } label: {
                            Label("Choose a file", systemImage: "folder").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glass)
                        .controlSize(.large)
                    }
                    .padding(.horizontal, 16)
                    .padding(.bottom, 8)
                }
            }
            .navigationTitle("Transcribe")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
            }
            .fileImporter(isPresented: $importing, allowedContentTypes: [.audio, .movie, .mpeg4Audio, .mpeg4Movie]) { result in
                if case .success(let url) = result { pickedName = nil; picked = url }
            }
            .confirmationDialog("When should the box transcribe it?",
                                isPresented: Binding(get: { picked != nil && !recorder.recording }, set: { shown in
                                    guard !shown else { return }
                                    // A recording dismissed here stays on the phone, ready to send.
                                    if let url = picked, let name = pickedName { unsent = (url, name) }
                                    picked = nil
                                }),
                                titleVisibility: .visible, presenting: picked) { url in
                Button("As soon as the box is free") { Task { await upload(url, tonight: false) } }
                Button("Tonight") { Task { await upload(url, tonight: true) } }
            } message: { _ in
                Text("Whisper needs the whole box. If a model is serving, \"as soon as possible\" asks you before switching; \"tonight\" switches between 1 and 6 am and puts your model back afterwards.")
            }
            .sheet(item: $viewing) { job in TranscriptView(session: session, job: job) }
            .animation(.smooth, value: recorder.recording)
            .onAppear {
                if let incoming, picked == nil {
                    pickedName = incoming.lastPathComponent
                    picked = incoming
                } else if unsent == nil, let left = RecordingBar.unsent().first {
                    // Recorded earlier and never sent (closed mid-recording, or the upload failed).
                    unsent = (left, left.lastPathComponent)
                }
            }
            .onDisappear {
                // Closing Transcribe mid-recording keeps what was recorded on the phone.
                if recorder.recording { _ = recorder.stop() }
            }
            .task {
                while !Task.isCancelled {
                    if let fresh = try? await session.jobs() { jobs = fresh }
                    try? await Task.sleep(for: .seconds(jobs.contains(where: \.isActive) ? 4 : 15))
                }
            }
        }
    }

    private func startRecording() async {
        error = nil
        if !(await recorder.start()) {
            error = recorder.error ?? "The recording couldn't start."
        }
    }

    private func stopRecording() {
        guard let url = recorder.stop() else { return }
        let stamp = Date().formatted(.dateTime.month(.abbreviated).day().hour().minute())
        pickedName = "Recording \(stamp).\(url.pathExtension)"
        picked = url
    }

    private func upload(_ url: URL, tonight: Bool) async {
        picked = nil
        let name = pickedName ?? url.lastPathComponent
        let recorded = pickedName != nil
        pickedName = nil
        uploading = name
        error = nil
        defer { uploading = nil }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        do {
            let job = try await session.uploadTranscription(file: url, name: name, tonight: tonight)
            jobs.insert(job, at: 0)
            // The box has the recording now.
            if recorded {
                try? FileManager.default.removeItem(at: url)
                unsent = nil
            }
        } catch {
            self.error = error.localizedDescription
            if recorded { unsent = (url, name) }
        }
    }

    private func runNow(_ job: BoxJob) async {
        do { try await session.runJobNow(job.id) } catch { self.error = error.localizedDescription }
        jobs = (try? await session.jobs()) ?? jobs
    }

    private func delete(_ job: BoxJob) async {
        do { try await session.deleteJob(job.id) } catch { self.error = error.localizedDescription }
        jobs = (try? await session.jobs()) ?? jobs
    }
}

private struct JobRow: View {
    let job: BoxJob
    let onRunNow: () -> Void
    let onOpen: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(systemName: icon).foregroundStyle(color)
                VStack(alignment: .leading, spacing: 2) {
                    Text(job.title).font(.tt(.headline)).lineLimit(1)
                    Text(subtitle).font(.tt(.caption)).foregroundStyle(.secondary)
                }
                Spacer()
                if job.status == "done" {
                    Button("Open", action: onOpen).buttonStyle(.glass)
                }
            }
            if job.status == "running", job.total > 0 {
                ProgressView(value: Double(job.done), total: Double(job.total))
            } else if job.status == "switching" {
                ProgressView()
            }
            if job.status == "waiting" {
                Button(action: onRunNow) {
                    Label("Switch the box to transcription now", systemImage: "arrow.triangle.2.circlepath")
                        .font(.tt(.footnote))
                }
                .buttonStyle(.glass)
            }
            if let error = job.error, job.status == "failed" {
                Text(error).font(.tt(.caption)).foregroundStyle(.red)
            }
        }
        .padding(.vertical, 4)
    }

    private var length: String {
        guard let d = job.durationS else { return "" }
        return d >= 3600 ? String(format: " · %dh %02dm", Int(d) / 3600, Int(d) % 3600 / 60) : " · \(max(1, Int(d / 60))) min"
    }

    private var subtitle: String {
        switch job.status {
        case "queued": return "In line" + length
        case "waiting": return job.when == "tonight" ? "Tonight, between 1 and 6 am" : "Waiting: the box is serving another model"
        case "switching": return "Loading Whisper…"
        case "running": return "Transcribing \(job.done) of \(job.total) parts" + length
        case "done": return "Transcribed" + length
        case "cancelled": return "Cancelled"
        default: return "Failed"
        }
    }

    private var icon: String {
        switch job.status {
        case "done": return "checkmark.circle.fill"
        case "failed": return "xmark.circle.fill"
        case "waiting": return job.when == "tonight" ? "moon.stars.fill" : "hourglass"
        default: return "waveform"
        }
    }

    private var color: Color {
        switch job.status {
        case "done": return .green
        case "failed": return .red
        default: return Brand.accent
        }
    }
}

/// A finished transcript: read, copy, share (text or subtitles), or ask the
/// assistant about it.
private struct TranscriptView: View {
    let session: BoxSession
    let job: BoxJob
    @Environment(AppModel.self) private var app
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    @State private var srtURL: URL?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                Text(text.isEmpty ? "Loading…" : text)
                    .font(.tt(.callout))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(16)
                if let error { Text(error).foregroundStyle(.orange).padding() }
            }
            .background(Brand.backdrop)
            .navigationTitle(job.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        Button("Ask the assistant about it", systemImage: "sparkles") {
                            dismiss()
                            app.selectedTab = .assistant
                            Task {
                                await session.send("Using my document \"Transcript: \(job.title)\", summarize the key points, decisions and action items.")
                            }
                        }
                        Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = text }
                        ShareLink(item: text, subject: Text(job.title)) { Label("Share text", systemImage: "square.and.arrow.up") }
                        if let srtURL {
                            ShareLink(item: srtURL) { Label("Share subtitles (.srt)", systemImage: "captions.bubble") }
                        }
                    } label: {
                        Image(systemName: "ellipsis.circle")
                    }
                }
            }
            .task {
                do {
                    text = String(decoding: try await session.jobResult(job.id), as: UTF8.self)
                    let srt = try await session.jobResult(job.id, format: "srt")
                    let url = FileManager.default.temporaryDirectory
                        .appendingPathComponent((job.title as NSString).deletingPathExtension + ".srt")
                    try srt.write(to: url)
                    srtURL = url
                } catch {
                    self.error = error.localizedDescription
                }
            }
        }
    }
}

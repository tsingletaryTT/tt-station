// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// Transcribe → On the box: record what the box hears (its microphone) and
/// plays (a meeting on its speakers), then transcribe it there. The room is
/// always told: the box chimes when it starts and stops, shows it on its own
/// screen, and the other paired phones get a notification -- so starting asks first.
struct BoxRecordingSection: View {
    let session: BoxSession
    @State private var status: RecordStatus?
    @State private var title = ""
    @State private var confirming = false
    @State private var working = false
    @State private var problem: String?

    var body: some View {
        Section {
            if let recording = status?.recording, recording.ended != true {
                VStack(alignment: .leading, spacing: 8) {
                    Label("Recording \u{201C}\(recording.title)\u{201D}", systemImage: "record.circle")
                        .foregroundStyle(.red)
                        .font(.tt(.body).weight(.semibold))
                    Text(detail(recording)).font(.tt(.caption)).foregroundStyle(.secondary)
                    HStack {
                        Button("Stop and transcribe") { Task { await stop(discard: false) } }
                            .buttonStyle(.glassProminent)
                        Spacer(minLength: 0)
                        Button("Discard", role: .destructive) { Task { await stop(discard: true) } }
                            .buttonStyle(.glass)
                    }
                    .controlSize(.small)
                    .disabled(working)
                }
            } else if let status {
                TextField("Name (optional)", text: $title)
                Button {
                    confirming = true
                } label: {
                    HStack {
                        if working { ProgressView() }
                        Label("Record on the box", systemImage: "record.circle")
                    }
                }
                .disabled(working || (status.microphone == nil && status.speakers == nil))
                Text(sources(status)).font(.tt(.caption)).foregroundStyle(.secondary)
            } else {
                ProgressView()
            }
            if let problem { Text(problem).font(.tt(.caption)).foregroundStyle(.orange) }
        } header: {
            Text("On the box")
        }
        .task(id: session.box.id) {
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(status?.recording == nil ? 15 : 2))
            }
        }
        .confirmationDialog("Record in the room?", isPresented: $confirming, titleVisibility: .visible) {
            Button("Start recording") { Task { await start() } }
        } message: {
            Text("The box chimes and shows that it's recording, and the other phones paired with it are told. It records its microphone and whatever it's playing; you and the box's owners can read the transcript.")
        }
    }

    private func sources(_ s: RecordStatus) -> String {
        switch (s.microphone, s.speakers) {
        case (nil, nil): return "The box has no microphone or speakers to record."
        case (nil, _): return "No microphone on the box: records what it plays (a meeting on its speakers)."
        case (_, nil): return "Records the box's microphone."
        default: return "Records the box's microphone and what it plays, mixed."
        }
    }

    private func detail(_ r: RecordStatus.Recording) -> String {
        var parts = ["\(r.seconds / 60):\(String(format: "%02d", r.seconds % 60))"]
        if let who = r.startedBy { parts.append("started by \(who)") }
        if let max = r.maxSeconds { parts.append("stops by itself after \(max / 3600) h") }
        return parts.joined(separator: " · ")
    }

    private func load() async {
        do { status = try await session.recordStatus(); problem = nil } catch { problem = error.localizedDescription }
    }

    private func start() async {
        working = true
        defer { working = false }
        do {
            let name = title.trimmingCharacters(in: .whitespacesAndNewlines)
            try await session.startBoxRecording(RecordStart(title: name.isEmpty ? nil : name,
                                                            mic: status?.microphone != nil,
                                                            system: status?.speakers != nil))
            title = ""
            await load()
        } catch {
            problem = error.localizedDescription
        }
    }

    private func stop(discard: Bool) async {
        working = true
        defer { working = false }
        do {
            try await session.stopBoxRecording(discard: discard)
            await load()
        } catch {
            problem = error.localizedDescription
        }
    }
}

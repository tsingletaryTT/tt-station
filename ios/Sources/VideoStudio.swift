import SwiftUI

/// Text-to-video on the box's served video model (Wan2.2), via the agent's
/// `/generate/video` job routes. A clip takes minutes, so jobs are recorded in the
/// gallery the moment they're submitted and polled until done -- including after
/// the app was closed and reopened.
struct VideoStudio: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @State private var prompt = ""
    @State private var negativePrompt = ""
    @State private var steps: Double = 20
    @State private var randomSeed = true
    @State private var seed = ""
    @State private var showSettings = false
    @State private var submitting = false
    @State private var error: String?
    @FocusState private var promptFocused: Bool

    private var model: String { session.serve?.model ?? session.status?.servingModel ?? "video model" }

    private var pending: [Creation] {
        app.creations.items(.video).filter { $0.isPending && $0.boxID == session.box.id }
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 18) {
                HStack(spacing: 10) {
                    Image(systemName: "film.stack").foregroundStyle(Brand.accent)
                    Text(Brand.shortModel(model)).font(.tt(.headline))
                    Spacer()
                    StatusPill(state: .serving, text: "Ready")
                }
                .padding(.top, 4)

                composer
                    .keyboardDoneButton()

                if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }

                if !pending.isEmpty {
                    VStack(spacing: 10) {
                        SectionTitle(text: "Rendering")
                        ForEach(pending) { job in
                            PendingVideoCard(creation: job) { cancel(job) }
                        }
                    }
                }

                GalleryGrid(title: "Videos", kind: .video) { reuse($0) }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
        }
        .scrollDismissesKeyboard(.interactively)
        .scrollIndicators(.hidden)
        .task(id: session.box.id) { await pollJobs() }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 14) {
            TextField("Describe a short video…", text: $prompt, axis: .vertical)
                .lineLimit(3...8)
                .focused($promptFocused)
                .padding(14)
                .glassEffect(.regular, in: .rect(cornerRadius: 18))

            DisclosureGroup(isExpanded: $showSettings) {
                VStack(alignment: .leading, spacing: 14) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Steps: \(Int(steps))").font(.tt(.subheadline))
                        Slider(value: $steps, in: 12...50, step: 1)
                        Text("More steps: smoother, much slower.").font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                    Toggle("Random seed", isOn: $randomSeed)
                    if !randomSeed {
                        TextField("Seed", text: $seed).keyboardType(.numberPad)
                    }
                    TextField("Avoid (negative prompt)", text: $negativePrompt, axis: .vertical)
                        .lineLimit(1...3)
                }
                .padding(.top, 10)
            } label: {
                Label("Settings", systemImage: "slider.horizontal.3").font(.tt(.subheadline))
            }

            Button(action: submit) {
                HStack(spacing: 8) {
                    if submitting { ProgressView() } else { Image(systemName: "film") }
                    Text(submitting ? "Sending to the box…" : "Generate video")
                }
                .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .controlSize(.large)
            .disabled(prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || submitting)

            Text("A clip takes several minutes on the box. You can leave the app; it'll be waiting here.")
                .font(.tt(.caption))
                .foregroundStyle(.secondary)
        }
        .padding(16)
        .card()
    }

    private func reuse(_ creation: Creation) {
        prompt = creation.prompt
        if let s = creation.seed {
            randomSeed = false
            seed = String(s)
        }
        steps = Double(min(max(creation.steps, 12), 50))
    }

    private func submit() {
        promptFocused = false
        error = nil
        submitting = true
        let text = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        let usedSeed = randomSeed ? Int.random(in: 0...2_147_483_647) : (Int(seed) ?? 0)
        let request = VideoRequest(prompt: text, negativePrompt: negativePrompt.isEmpty ? nil : negativePrompt,
                                   numInferenceSteps: Int(steps), seed: usedSeed)
        let base = Creation(id: UUID(), kind: .video, prompt: text, model: model, seed: usedSeed,
                            steps: Int(steps), createdAt: Date(), boxID: session.box.id,
                            seconds: nil, fileName: nil, jobID: nil, status: "queued", error: nil)
        Task {
            defer { submitting = false }
            do {
                switch try await session.submitVideo(request) {
                case .job(let job):
                    var creation = base
                    creation.jobID = job.id
                    creation.status = job.status
                    app.creations.add(creation)
                    prompt = ""
                case .video(let data):
                    // A server in synchronous mode hands back the finished clip.
                    var creation = base
                    creation.fileName = try app.creations.write(data, ext: "mp4")
                    creation.status = "done"
                    creation.seconds = Date().timeIntervalSince(base.createdAt)
                    app.creations.add(creation)
                    prompt = ""
                }
            } catch AgentError.http(409, let message) {
                error = message
                await session.refreshStatus()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func cancel(_ creation: Creation) {
        guard let id = creation.jobID else { return }
        app.creations.update(creation.id) { $0.status = "cancelling" }
        Task {
            do {
                try await session.cancelVideo(id: id)
            } catch {
                app.creations.update(creation.id) { $0.error = error.localizedDescription }
            }
        }
    }

    /// Poll every pending job for this box; download finished clips. Runs for as
    /// long as the studio is on screen; reopening the app picks up where it left off.
    private func pollJobs() async {
        while !Task.isCancelled {
            for creation in pending {
                guard let id = creation.jobID else { continue }
                do {
                    let job = try await session.videoJob(id: id)
                    if job.status == "completed" {
                        app.creations.update(creation.id) { $0.status = "downloading" }
                        let data = try await session.downloadVideo(id: id)
                        let file = try app.creations.write(data, ext: "mp4")
                        let seconds = job.completedAt.flatMap { done in job.createdAt.map { Double(done - $0) } }
                            ?? Date().timeIntervalSince(creation.createdAt)
                        app.creations.update(creation.id) {
                            $0.fileName = file
                            $0.status = "done"
                            $0.seconds = seconds
                        }
                    } else if job.status != creation.status {
                        app.creations.update(creation.id) {
                            $0.status = job.status
                            $0.error = job.error?.text
                        }
                    }
                } catch AgentError.http(404, _) {
                    // The box forgot the job: the server restarted (jobs live in its memory).
                    app.creations.update(creation.id) {
                        $0.status = "failed"
                        $0.error = "The box restarted and lost this video. Try again."
                    }
                } catch {
                    // Transient (network, model switching): try again next round.
                }
            }
            try? await Task.sleep(for: .seconds(5))
        }
    }
}

private struct PendingVideoCard: View {
    let creation: Creation
    let onCancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                ProgressView()
                VStack(alignment: .leading, spacing: 2) {
                    Text(creation.prompt).font(.tt(.subheadline)).lineLimit(2)
                    TimelineView(.periodic(from: .now, by: 1)) { context in
                        Text("\(label) · \(Elapsed.string(since: Int(creation.createdAt.timeIntervalSince1970), now: context.date))")
                            .font(.tt(.caption).monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                if creation.status != "cancelling" && creation.status != "downloading" {
                    Button("Cancel", role: .cancel, action: onCancel)
                        .buttonStyle(.glass)
                        .controlSize(.small)
                }
            }
            ShimmerBar()
        }
        .padding(14)
        .card(radius: 20)
    }

    private var label: String {
        switch creation.status {
        case "queued": return "Queued"
        case "in_progress": return "Rendering"
        case "cancelling": return "Cancelling"
        case "downloading": return "Downloading"
        default: return creation.status.capitalized
        }
    }
}

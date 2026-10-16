import SwiftUI

/// Box tab → Research: ask the box to research a question on the web (it
/// reads the pages and writes a report with citations), follow it, read it,
/// and share it with everyone on the box. Reports are the asker's (and the
/// owners') unless shared.
struct ResearchListView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var list: ResearchList?
    @State private var question = ""
    @State private var depth = "standard"
    @State private var starting = false
    @State private var error: String?
    @State private var opened: ResearchJob?

    var body: some View {
        NavigationStack {
            List {
                Section {
                    TextField("What should it find out?", text: $question, axis: .vertical)
                        .lineLimit(2...6)
                    Picker("How deep", selection: $depth) {
                        Text("Quick").tag("quick")
                        Text("Standard").tag("standard")
                        Text("Deep").tag("deep")
                    }
                    .pickerStyle(.segmented)
                    Button {
                        Task { await start() }
                    } label: {
                        HStack {
                            if starting { ProgressView() }
                            Label("Research it", systemImage: "magnifyingglass")
                        }
                    }
                    .disabled(starting || question.trimmingCharacters(in: .whitespacesAndNewlines).count < 8
                              || list?.check?.ok == false)
                } footer: {
                    if let check = list?.check, !check.ok {
                        Text(check.why ?? "The box isn't ready for research.")
                    } else {
                        Text("It searches the web, reads what it finds, and writes a report with sources. Deep takes longer. You'll get a notification when it's done.")
                    }
                }
                if let error {
                    Section { Text(error).foregroundStyle(.orange) }
                }
                Section("Reports") {
                    if let jobs = list?.jobs, jobs.isEmpty {
                        Text("No reports yet.").foregroundStyle(.secondary)
                    }
                    ForEach(list?.jobs ?? []) { job in
                        NavigationLink(value: job) { ResearchRow(job: job) }
                    }
                }
            }
            .navigationTitle("Research")
            .navigationBarTitleDisplayMode(.inline)
            .navigationDestination(for: ResearchJob.self) { job in
                ResearchReportView(session: session, job: job)
            }
            .navigationDestination(item: $opened) { job in
                ResearchReportView(session: session, job: job)
            }
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .refreshable { await load() }
            .task {
                while !Task.isCancelled {
                    await load()
                    let active = list?.jobs.contains(where: \.isActive) == true
                    try? await Task.sleep(for: .seconds(active ? 5 : 20))
                }
            }
        }
    }

    private func load() async {
        do {
            list = try await session.research()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func start() async {
        starting = true
        defer { starting = false }
        do {
            let job = try await session.startResearch(ResearchRequest(
                question: question.trimmingCharacters(in: .whitespacesAndNewlines), depth: depth))
            question = ""
            error = nil
            await load()
            opened = job
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct ResearchRow: View {
    let job: ResearchJob

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(job.displayTitle).font(.tt(.body).weight(.semibold)).lineLimit(2)
                Spacer(minLength: 0)
                if job.isActive { ProgressView().controlSize(.mini) }
                if job.shared == true { Image(systemName: "person.2").foregroundStyle(.secondary) }
            }
            Text(subtitle).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
        }
        .padding(.vertical, 2)
    }

    private var subtitle: String {
        var parts: [String] = []
        switch job.state {
        case "queued": parts.append("Waiting its turn")
        case "running": parts.append(job.step ?? "Researching")
        case "failed": parts.append(job.error ?? "Didn't finish")
        case "stopped": parts.append("Stopped")
        default: if let summary = job.summary { parts.append(summary) }
        }
        if let who = job.startedByName { parts.append(who) }
        parts.append(Date(timeIntervalSince1970: job.created).formatted(.relative(presentation: .named)))
        return parts.joined(separator: " · ")
    }
}

/// One report: its progress while it runs, then the report and its sources.
struct ResearchReportView: View {
    let session: BoxSession
    let job: ResearchJob
    @State private var report: ResearchReport?
    @State private var problem: String?

    private var current: ResearchJob { report?.job ?? job }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text(current.question).font(.tt(.headline))
                if current.isActive {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text(current.step ?? "Researching…").font(.tt(.footnote)).foregroundStyle(.secondary)
                    }
                }
                if let error = current.error {
                    Label(error, systemImage: "exclamationmark.triangle").font(.tt(.footnote)).foregroundStyle(.orange)
                }
                if let text = report?.report {
                    MarkdownView(text: text)
                }
                if let sources = report?.sources, !sources.isEmpty, report?.report != nil {
                    SectionTitle(text: "Sources")
                    ForEach(sources) { source in
                        if let url = URL(string: source.url) {
                            Link(destination: url) {
                                HStack(alignment: .top, spacing: 8) {
                                    Text("[\(source.n)]").font(.ttMono(12)).foregroundStyle(.secondary)
                                    VStack(alignment: .leading, spacing: 2) {
                                        Text(source.title ?? url.host() ?? source.url).font(.tt(.footnote)).lineLimit(2)
                                        Text(url.host() ?? source.url).font(.tt(.caption2)).foregroundStyle(.secondary)
                                    }
                                }
                                .frame(maxWidth: .infinity, alignment: .leading)
                            }
                        }
                    }
                }
                if let problem { Text(problem).font(.tt(.footnote)).foregroundStyle(.orange) }
            }
            .padding(16)
            .readableWidth()
        }
        .navigationTitle(current.displayTitle)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if report?.canChange == true {
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        if current.isActive {
                            Button("Stop", systemImage: "stop.fill", role: .destructive) {
                                Task { await act { try await session.stopResearch(job.id) } }
                            }
                        }
                        Button(current.shared == true ? "Make private" : "Share with everyone on the box",
                               systemImage: current.shared == true ? "lock" : "person.2") {
                            Task { await act { try await session.shareResearch(job.id, shared: current.shared != true) } }
                        }
                    } label: {
                        Image(systemName: "ellipsis.circle")
                    }
                }
            }
            if let text = report?.report {
                ToolbarItem(placement: .topBarTrailing) {
                    ShareLink(item: text, subject: Text(current.displayTitle)) { Image(systemName: "square.and.arrow.up") }
                }
            }
        }
        .task(id: job.id) {
            repeat {
                await load()
                try? await Task.sleep(for: .seconds(4))
            } while !Task.isCancelled && current.isActive
        }
    }

    private func load() async {
        do {
            report = try await session.researchReport(job.id)
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }

    private func act(_ work: () async throws -> Void) async {
        do {
            try await work()
            await load()
        } catch {
            problem = error.localizedDescription
        }
    }
}

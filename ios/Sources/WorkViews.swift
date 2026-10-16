import SwiftUI

/// "Waiting for you": everything on the box that needs the person's OK -- a
/// Code session's command or edit, a routine's action -- answerable here, the
/// same as from its notification. Hidden when nothing's waiting.
struct WaitingCard: View {
    let session: BoxSession
    @State private var waiting: [Approval] = []
    @State private var answering: String?
    @State private var problem: String?

    var body: some View {
        Group {
            if !waiting.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    SectionTitle(text: "Waiting for you")
                    ForEach(waiting) { a in
                        ApprovalRow(approval: a, busy: answering == a.id) { allow, always in
                            Task { await answer(a, allow: allow, always: always) }
                        }
                    }
                    if let problem {
                        Text(problem).font(.tt(.caption)).foregroundStyle(.orange)
                    }
                }
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .animation(.snappy, value: waiting)
        .task(id: session.box.id) {
            while !Task.isCancelled {
                if let list = try? await session.approvals() { waiting = list }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func answer(_ a: Approval, allow: Bool, always: Bool) async {
        answering = a.id
        defer { answering = nil }
        do {
            try await session.answer(approval: a.id, allow: allow, always: always)
            problem = nil
            waiting.removeAll { $0.id == a.id }
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// One approval: where it's from, what it wants, and the answers.
struct ApprovalRow: View {
    let approval: Approval
    var busy = false
    let onAnswer: (_ allow: Bool, _ always: Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: approval.kind == "code" ? "chevron.left.forwardslash.chevron.right" : "clock.arrow.circlepath")
                    .foregroundStyle(Brand.accent)
                Text(approval.place).font(.tt(.subheadline).weight(.semibold)).lineLimit(1)
                Spacer(minLength: 0)
                if busy { ProgressView().controlSize(.small) }
            }
            Text(approval.summary)
                .font(approval.kind == "code" ? .ttMono(12) : .tt(.footnote))
                .lineLimit(4)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(10)
                .background(.black.opacity(0.25), in: .rect(cornerRadius: 10))
            HStack(spacing: 8) {
                Button("Allow") { onAnswer(true, false) }
                    .buttonStyle(.glassProminent)
                if approval.canAlways {
                    Button("Always") { onAnswer(true, true) }
                        .buttonStyle(.glass)
                }
                Spacer(minLength: 0)
                Button("Don't allow", role: .destructive) { onAnswer(false, false) }
                    .buttonStyle(.glass)
            }
            .controlSize(.small)
            .disabled(busy)
        }
        .padding(14)
        .card(tint: .orange.opacity(0.15), radius: 18)
    }
}

// MARK: - Code

/// Box tab → Code: the box's coding sessions, to follow and steer from the phone.
struct CodeListView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var sessions: [CodeSession]?
    @State private var error: String?
    @State private var creating = false
    @State private var started: CodeSession?

    var body: some View {
        NavigationStack {
            List {
                if let sessions, sessions.isEmpty {
                    ContentUnavailableView("No Code sessions", systemImage: "chevron.left.forwardslash.chevron.right",
                                           description: Text("Start one with + (in the box's sandbox), or in TT Station on your computer: pick a project folder and say what to build or fix."))
                }
                ForEach(sessions ?? []) { s in
                    NavigationLink {
                        CodeThreadView(session: session, code: s)
                    } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            HStack(spacing: 6) {
                                Text(s.title).font(.tt(.body).weight(.semibold)).lineLimit(1)
                                Spacer(minLength: 0)
                                if s.waiting > 0 {
                                    Label("Needs you", systemImage: "hand.raised.fill").labelStyle(.titleAndIcon)
                                        .font(.tt(.caption2)).foregroundStyle(.orange)
                                } else if s.busy {
                                    ProgressView().controlSize(.mini)
                                }
                            }
                            Text(s.folder.replacingOccurrences(of: #"^/home/[^/]+"#, with: "~", options: .regularExpression))
                                .font(.ttMono(11)).foregroundStyle(.secondary).lineLimit(1)
                            Text(subtitle(s)).font(.tt(.caption)).foregroundStyle(.secondary)
                        }
                        .padding(.vertical, 2)
                    }
                }
                if let error {
                    Text(error).font(.tt(.footnote)).foregroundStyle(.orange)
                }
            }
            .navigationTitle("Code")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    Button("New session", systemImage: "plus") { creating = true }
                }
            }
            .sheet(isPresented: $creating) {
                NewCodeSessionSheet(session: session, recent: RecentFolders.from(sessions ?? [])) { started = $0 }
            }
            .navigationDestination(item: $started) { code in
                CodeThreadView(session: session, code: code)
            }
            .refreshable { await load() }
            .task {
                while !Task.isCancelled {
                    await load()
                    try? await Task.sleep(for: .seconds(6))
                }
            }
        }
    }

    private func subtitle(_ s: CodeSession) -> String {
        var parts: [String] = [s.sandbox ? "Sandbox" : "On the box"]
        if s.changes > 0 { parts.append("\(s.changes) changed file\(s.changes == 1 ? "" : "s")") }
        parts.append(Date(timeIntervalSince1970: s.last).formatted(.relative(presentation: .named)))
        return parts.joined(separator: " · ")
    }

    private func load() async {
        do {
            sessions = try await session.codeSessions()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// One Code session: what it's doing, its questions, and a box to tell it more.
struct CodeThreadView: View {
    let session: BoxSession
    let code: CodeSession
    @State private var events: [CodeEvent] = []
    @State private var info: CodeSession?
    @State private var waiting: [Approval] = []
    @State private var draft = ""
    @State private var sending = false
    @State private var problem: String?

    private var live: CodeSession { info ?? code }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(visible) { e in
                        CodeEventRow(event: e).id(e.seq)
                    }
                    ForEach(waiting) { a in
                        ApprovalRow(approval: a) { allow, always in
                            Task {
                                try? await session.answer(approval: a.id, allow: allow, always: always)
                                waiting.removeAll { $0.id == a.id }
                            }
                        }
                    }
                    if live.busy && waiting.isEmpty {
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text("Working…").font(.tt(.footnote)).foregroundStyle(.secondary)
                        }
                        .id("working")
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .padding(16)
                .readableWidth()
            }
            .onChange(of: events.count) { _, _ in withAnimation { proxy.scrollTo("end", anchor: .bottom) } }
        }
        .safeAreaInset(edge: .bottom) { composer }
        .navigationTitle(live.title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if live.busy || live.running {
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        if live.busy {
                            Button("Stop this turn", systemImage: "stop.fill") { Task { try? await session.interruptCode(code.id) } }
                        }
                        Button("Stop the agent", systemImage: "power") {
                            Task {
                                do { try await session.stopCode(code.id); problem = nil } catch { problem = error.localizedDescription }
                                await poll()
                            }
                        }
                    } label: {
                        Image(systemName: "stop.circle")
                    } primaryAction: {
                        Task {
                            if live.busy { try? await session.interruptCode(code.id) } else { try? await session.stopCode(code.id) }
                        }
                    }
                    .accessibilityLabel("Stop")
                }
            }
        }
        .task(id: code.id) {
            while !Task.isCancelled {
                await poll()
                try? await Task.sleep(for: .seconds(live.busy ? 2 : 6))
            }
        }
    }

    /// What's worth showing on a phone: the conversation, the tools it used, failures.
    private var visible: [CodeEvent] {
        events.filter { ["user", "text", "tool", "error", "notice", "done"].contains($0.kind) || ($0.kind == "tool_result" && $0.isError == true) }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let problem {
                Text(problem).font(.tt(.caption)).foregroundStyle(.orange)
            }
            HStack(alignment: .bottom, spacing: 8) {
                TextField(live.busy ? "Add to what it's doing…" : "Tell it what to do next…", text: $draft, axis: .vertical)
                    .lineLimit(1...5)
                    .keyboardDoneButton()
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .glassEffect(.regular, in: .rect(cornerRadius: 20))
                if live.busy {
                    Menu {
                        Button("Add to this turn", systemImage: "text.badge.plus") { send(interrupt: false) }
                        Button("Stop and send", systemImage: "arrow.uturn.forward") { send(interrupt: true) }
                    } label: {
                        Image(systemName: "arrow.up").font(.tt(.body).weight(.semibold)).frame(width: 40, height: 40)
                    } primaryAction: {
                        send(interrupt: false)
                    }
                    .buttonStyle(.glassProminent)
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || sending)
                } else {
                    Button { send(interrupt: false) } label: {
                        Image(systemName: "arrow.up").font(.tt(.body).weight(.semibold)).frame(width: 40, height: 40)
                    }
                    .buttonStyle(.glassProminent)
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || sending)
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 8)
    }

    private func send(interrupt: Bool) {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        sending = true
        Task {
            defer { sending = false }
            do {
                try await session.sendCode(code.id, text: text, interrupt: interrupt)
                draft = ""
                problem = nil
                await poll()
            } catch {
                problem = error.localizedDescription
            }
        }
    }

    private func poll() async {
        let after = events.last?.seq ?? 0
        guard let detail = try? await session.codeSession(code.id, after: after) else { return }
        info = detail.session
        let fresh = detail.events.filter { $0.seq > after }
        if !fresh.isEmpty { events.append(contentsOf: fresh) }
        if detail.session.waiting > 0 {
            waiting = ((try? await session.approvals()) ?? []).filter { $0.session == code.id }
        } else {
            waiting = []
        }
    }
}

private struct CodeEventRow: View {
    let event: CodeEvent

    var body: some View {
        switch event.kind {
        case "user":
            HStack {
                Spacer(minLength: 40)
                Text(event.text ?? "")
                    .font(.tt(.body))
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(Brand.accentFill.opacity(0.35), in: .rect(cornerRadius: 18))
            }
        case "text":
            MarkdownView(text: event.text ?? "")
        case "tool":
            Label {
                Text(toolLine).font(.ttMono(12)).lineLimit(2).foregroundStyle(.secondary)
            } icon: {
                Image(systemName: toolIcon).foregroundStyle(Brand.accent)
            }
        case "tool_result":
            Text(event.output ?? "That step failed.")
                .font(.ttMono(11)).foregroundStyle(.orange).lineLimit(3)
        case "done":
            Label(event.ok == false ? "Stopped" : "Done", systemImage: event.ok == false ? "stop.circle" : "checkmark.circle")
                .font(.tt(.caption)).foregroundStyle(.secondary)
        default:
            Text(event.text ?? "").font(.tt(.footnote)).foregroundStyle(.secondary)
        }
    }

    private var toolIcon: String {
        switch event.name {
        case "Bash": return "terminal"
        case "Edit", "Write", "NotebookEdit": return "pencil"
        case "Read": return "doc.text"
        case "WebSearch", "WebFetch": return "globe"
        default: return "wrench.and.screwdriver"
        }
    }

    private var toolLine: String {
        let i = event.input
        let file = (i?.filePath ?? i?.path).map { ($0 as NSString).lastPathComponent }
        switch event.name {
        case "Bash": return i?.description ?? i?.command ?? "Ran a command"
        case "Edit": return "Edited \(file ?? "a file")"
        case "Write": return "Wrote \(file ?? "a file")"
        case "Read": return "Read \(file ?? "a file")"
        default: return [event.name, i?.pattern ?? file].compactMap { $0 }.joined(separator: " ")
        }
    }
}

// MARK: - Routines

/// Box tab → Routines: jobs the box does on its own, their reports, Run now.
struct RoutinesListView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var routines: [Routine]?
    @State private var running: RoutineRun?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            List {
                if let running {
                    Section("Running now") {
                        NavigationLink { RoutineRunView(session: session, runID: running.id, title: running.name) } label: {
                            HStack(spacing: 10) {
                                if running.status == "waiting" {
                                    Image(systemName: "hand.raised.fill").foregroundStyle(.orange)
                                } else {
                                    ProgressView().controlSize(.small)
                                }
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(running.name).font(.tt(.body).weight(.semibold))
                                    Text(running.pending.map { "Needs your OK: \($0.summary)" } ?? running.activity ?? "Working")
                                        .font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
                                }
                            }
                        }
                    }
                }
                if let routines, routines.isEmpty {
                    ContentUnavailableView("No routines", systemImage: "clock.arrow.circlepath",
                                           description: Text("Make one in TT Station on your computer: say what the box should do and when."))
                }
                Section {
                    ForEach(routines ?? []) { r in
                        NavigationLink { RoutineView(session: session, routine: r) } label: {
                            VStack(alignment: .leading, spacing: 4) {
                                HStack {
                                    Text(r.name).font(.tt(.body).weight(.semibold)).lineLimit(1)
                                    Spacer(minLength: 0)
                                    if let last = r.last { RunStatus(status: last.status) }
                                }
                                Text(r.enabled ? r.when : "\(r.when) · off").font(.tt(.caption)).foregroundStyle(.secondary)
                                if let headline = r.last?.headline {
                                    Text(headline).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
                                }
                            }
                            .padding(.vertical, 2)
                        }
                    }
                }
                if let error {
                    Text(error).font(.tt(.footnote)).foregroundStyle(.orange)
                }
            }
            .navigationTitle("Routines")
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .refreshable { await load() }
            .task {
                while !Task.isCancelled {
                    await load()
                    try? await Task.sleep(for: .seconds(running == nil ? 10 : 3))
                }
            }
        }
    }

    private func load() async {
        do {
            let r = try await session.routines()
            routines = r.routines
            running = r.running
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct RunStatus: View {
    let status: String
    var body: some View {
        let (label, color): (String, Color) = switch status {
        case "done": ("Done", Brand.accent)
        case "running": ("Running", .blue)
        case "waiting": ("Needs you", .orange)
        case "stopped": ("Stopped", .secondary)
        default: ("Didn't finish", .red)
        }
        Text(label).font(.ttMono(10, .medium)).foregroundStyle(color)
    }
}

private struct RoutineView: View {
    let session: BoxSession
    let routine: Routine
    @State private var problem: String?
    @State private var started: RoutineRun?
    @State private var enabled: Bool?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text(routine.goal).font(.tt(.body))
                Label(routine.when, systemImage: "clock")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
                if routine.schedule != nil {
                    Toggle(isOn: Binding(get: { enabled ?? routine.enabled }, set: { on in Task { await setEnabled(on) } })) {
                        Text((enabled ?? routine.enabled) ? "Runs on its schedule" : "Paused").font(.tt(.subheadline))
                    }
                }
                Button {
                    Task {
                        do { started = try await session.runRoutine(routine.id); problem = nil }
                        catch { problem = error.localizedDescription }
                    }
                } label: {
                    Label("Run now", systemImage: "play.fill").frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                if let problem { Text(problem).font(.tt(.footnote)).foregroundStyle(.orange) }
                if let last = routine.last {
                    SectionTitle(text: "Last run")
                    RunReport(session: session, runID: last.id)
                }
            }
            .padding(16)
            .readableWidth()
        }
        .navigationTitle(routine.name)
        .navigationBarTitleDisplayMode(.inline)
        .navigationDestination(item: $started) { run in
            RoutineRunView(session: session, runID: run.id, title: run.name)
        }
    }

    private func setEnabled(_ on: Bool) async {
        do {
            try await session.setRoutine(routine, enabled: on)
            enabled = on
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// A run as it goes: what it's doing, its question, then its report.
private struct RoutineRunView: View {
    let session: BoxSession
    let runID: String
    let title: String

    var body: some View {
        ScrollView {
            RunReport(session: session, runID: runID, follow: true)
                .padding(16)
                .readableWidth()
        }
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
    }
}

private struct RunReport: View {
    let session: BoxSession
    let runID: String
    var follow = false
    @State private var run: RoutineRun?
    @State private var answering = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let run {
                HStack {
                    RunStatus(status: run.status)
                    Text(Date(timeIntervalSince1970: run.started).formatted(date: .abbreviated, time: .shortened))
                        .font(.tt(.caption)).foregroundStyle(.secondary)
                    Spacer()
                    if run.status == "running" || run.status == "waiting" {
                        Button("Stop", role: .destructive) { Task { try? await session.stopRoutineRun(run.id) } }
                            .controlSize(.small)
                    }
                }
                if let p = run.pending {
                    ApprovalRow(approval: Approval(id: "routine:\(run.id)", kind: "routine", place: run.name, summary: p.summary,
                                                   canAlways: false, session: nil, run: run.id), busy: answering) { allow, _ in
                        Task {
                            answering = true
                            try? await session.answer(approval: "routine:\(run.id)", allow: allow)
                            answering = false
                            await load()
                        }
                    }
                } else if let activity = run.activity {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text(activity).font(.tt(.footnote)).foregroundStyle(.secondary)
                    }
                }
                if let report = run.report {
                    MarkdownView(text: report)
                }
            } else {
                ProgressView()
            }
        }
        .task(id: runID) {
            repeat {
                await load()
                try? await Task.sleep(for: .seconds(3))
            } while follow && !Task.isCancelled && (run?.finished == nil)
        }
    }

    private func load() async {
        if let r = try? await session.routineRun(runID) { run = r }
    }
}

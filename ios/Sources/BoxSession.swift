// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation

/// Everything the app knows about the active box: status polling, the assistant
/// conversation and its confirmations, and direct box controls. Shared by every tab.
@MainActor
@Observable
final class BoxSession {
    let box: SavedBox
    /// The open conversation's transcript.
    private(set) var items: [ChatItem] = []
    /// Which saved conversation `items` belongs to.
    private(set) var conversationID = UUID()
    private(set) var status: BoxStatus?
    private(set) var statusError: String?
    private(set) var isThinking = false
    /// Called with the answer so far as it streams in (voice mode speaks it
    /// sentence by sentence instead of waiting for the end).
    @ObservationIgnored var onAnswerText: ((String) -> Void)?
    /// The agent's serving lifecycle (`GET /serve`); `nil` from agents that predate it.
    private(set) var serve: ServeState?
    /// Set the moment the user starts a model, until the agent's own lifecycle says
    /// it's starting -- so the UI reacts instantly, not on the next poll.
    private var pendingStart: String?
    /// The box no longer accepts this phone's token: it must pair again.
    private(set) var needsRepair = false
    /// The box's model catalog (from the tt CLI when the agent has it), loaded on demand.
    private(set) var catalog: [TtModel]?
    /// Downloads the box is running or ran, newest first.
    private(set) var pulls: [PullJob] = []
    private(set) var diskFreeBytes: Int64?
    /// `false` when the agent has no tt CLI: the catalog is the legacy `/models`
    /// list and downloads aren't available.
    private(set) var canDownload = false
    private(set) var catalogError: String?
    /// Whether (and with which models) the box can run chat and images at once.
    private(set) var coserve: CoserveInfo?
    /// Whether this app and the box speak the same API (from `/status`).
    private(set) var compat: ClientCompat = .ok
    /// What this phone may do on the box: owner or member, and whether it may
    /// start and stop models. `nil` until the box has said.
    var access: BoxAccess?
    /// The model's sleep settings and what's asleep (`GET /idle`); `nil` from
    /// agents without idle sleep.
    var idle: IdleInfo?

    let client: AgentClient
    /// Called when the user starts something slow (a download, a model load) --
    /// the moment to offer notifications for when it's done.
    @ObservationIgnored var onLongOperation: (() -> Void)?
    private let conversations: ConversationStore
    private var streamTask: Task<Void, Never>?
    private var pollTask: Task<Void, Never>?
    private var pullPollTask: Task<Void, Never>?
    @ObservationIgnored private var unauthorizedObserver: NSObjectProtocol?

    init(box: SavedBox, token: String, conversations: ConversationStore) throws {
        self.box = box
        self.client = try AgentClient(box: box, token: token)
        self.conversations = conversations
        // Any request this box refuses with 401 (from any screen) means it forgot
        // this phone: offer to pair again instead of failing call after call.
        let base = client.baseURL.absoluteString
        unauthorizedObserver = NotificationCenter.default.addObserver(
            forName: AgentClient.unauthorized, object: nil, queue: .main) { [weak self] note in
            guard (note.object as? String) == base else { return }
            Task { @MainActor in self?.needsRepair = true }
        }
        // Pick up where you left off: the most recent conversation with this box.
        if let latest = conversations.conversations(for: box.id).first {
            conversationID = latest.id
            items = latest.items
        }
    }

    var isServing: Bool { status?.servingModel != nil }
    var address: String { client.address }
    var token: String? { client.token }

    /// The model being brought up, if any (from the agent, or the one just requested).
    var startingModel: String? {
        if let serve, serve.isStarting { return serve.model ?? pendingStart ?? "a model" }
        return pendingStart
    }

    /// What the served model is for: `chat`, `image`, `video`, `speech`, `tts`, `other`.
    var servedKind: String? {
        guard isServing else { return nil }
        if let kind = serve?.kind { return kind }
        // An agent without `/serve`: guess from the catalog by served id.
        let served = status?.servingModel.map(Brand.shortModel)
        return catalog?.first { $0.name == served || $0.hfRepo == status?.servingModel }?.kind ?? "chat"
    }

    /// The image half of a split box, when it's doing something.
    var companion: CompanionState? {
        guard let companion = serve?.companion, companion.phase != "idle" else { return nil }
        return companion
    }

    /// Chat and images at once.
    var isSplit: Bool { serve?.split == true }

    /// An image model is serving, alone or beside chat.
    var canMakeImages: Bool { servedKind == "image" || companion?.isServing == true }

    /// The image model to name in the studio.
    var imageModel: String? {
        if companion?.isServing == true { return companion?.model ?? companion?.servedId }
        return servedKind == "image" ? (serve?.model ?? status?.servingModel) : nil
    }

    func loadCoserve() async {
        coserve = try? await client.coserve()
    }

    /// Run `chat` and `image` side by side (one P300 card each).
    func startCoserve(chat: String, image: String) async throws {
        onLongOperation?()
        do {
            try await client.startCoserve(chat: chat, image: image)
        } catch {
            noteAuthFailure(error)
            throw error
        }
        pendingStart = chat
        items.append(.notice("Starting \(Brand.shortModel(chat)) for chat and \(Brand.shortModel(image)) for images, side by side."))
        await refreshStatus()
    }

    /// A chat model is serving (or one of unknown kind, which the agent lets the
    /// assistant try), so the Assistant tab can talk.
    var canChat: Bool { isServing && (servedKind == "chat" || servedKind == "other") }

    /// Why the last start failed, while that's still the latest news.
    var startFailure: String? {
        guard let serve, serve.isFailed else { return nil }
        return serve.error ?? "The model failed to start."
    }

    // MARK: status

    func startPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            var tick = 0
            while !Task.isCancelled {
                await self?.refreshStatus()
                // Who this phone is on the box, and the model's sleep, change
                // rarely: every half minute (and right away at the start).
                if tick % 6 == 0, self?.statusError == nil { await self?.refreshAccess(learnWake: tick == 0) }
                tick += 1
                // Faster while a model is coming up, so "ready" shows promptly.
                let busy = self?.startingModel != nil
                try? await Task.sleep(for: .seconds(busy ? 2 : 5))
            }
        }
    }

    func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
        if let unauthorizedObserver { NotificationCenter.default.removeObserver(unauthorizedObserver) }
        unauthorizedObserver = nil
        pullPollTask?.cancel()
        pullPollTask = nil
    }

    func refreshStatus() async {
        do {
            let latest = try await client.status()
            let wasStarting = startingModel
            status = latest
            statusError = nil
            compat = ClientCompat.check(latest)
            if let next = try? await client.serveState() {
                let previous = serve
                serve = next
                if let pending = pendingStart {
                    let same = { (name: String?) in name.map(Brand.shortModel) == Brand.shortModel(pending) }
                    // Clear only once the agent reports *this* start: right after the
                    // request it can still read the previous model as serving.
                    if next.isStarting || next.isFailed || (next.isServing && (same(next.model) || same(next.servedId))) {
                        pendingStart = nil
                    }
                }
                announceTransition(from: previous, to: next, wasStarting: wasStarting)
            } else if wasStarting != nil, latest.servingModel != nil {
                // Older agent without `/serve`.
                pendingStart = nil
                items.append(.notice("\(Brand.shortModel(latest.servingModel ?? "")) is ready."))
            }
        } catch {
            statusError = error.localizedDescription
        }
    }

    private func announceTransition(from previous: ServeState?, to next: ServeState, wasStarting: String?) {
        if let was = previous?.companion, let now = next.companion, was.phase != now.phase {
            switch (was.phase, now.phase) {
            case ("starting", "serving"):
                items.append(.notice("\(now.displayModel ?? "The image model") is ready. Make images in the Create tab while you chat."))
            case ("starting", "failed"):
                items.append(.error("\(now.displayModel ?? "The image model") couldn't start: \(now.error ?? "unknown error")"))
            default:
                break
            }
        }
        guard let previous, previous.phase != next.phase else { return }
        switch (previous.phase, next.phase) {
        case ("starting", "serving"):
            items.append(.notice("\(next.displayModel ?? "The model") is ready."))
        case ("starting", "failed"):
            items.append(.error("\(next.displayModel ?? "The model") couldn't start: \(next.error ?? "unknown error")"))
        default:
            break
        }
    }

    /// Any authed call that comes back 401 means the box forgot this phone.
    func noteAuthFailure(_ error: Error) {
        if case AgentError.http(401, _) = error { needsRepair = true }
    }

    // MARK: conversation

    /// Settings the Assistant tab's composer menu writes (via `@AppStorage`).
    static let detailedKey = "tt.chat.detailed"
    static let thinkKey = "tt.chat.think"
    private var detailed: Bool { UserDefaults.standard.object(forKey: Self.detailedKey) as? Bool ?? true }
    private var think: Bool { UserDefaults.standard.bool(forKey: Self.thinkKey) }

    /// The transcript as the model sees it: what was said, plus what confirmed
    /// actions did (so "did it work?" has an answer). Notices and errors are UI only.
    private var turns: [ChatTurn] {
        items.compactMap { item in
            switch item.role {
            case .user: return ChatTurn(role: "user", content: item.text)
            case .assistant where !item.text.isEmpty: return ChatTurn(role: "assistant", content: item.text)
            case .action: return item.resolved.map { ChatTurn(role: "assistant", content: $0) }
            default: return nil
            }
        }
    }

    /// Ask `text`. A `spoken` turn (voice mode) asks for the short, plain-text
    /// answer that reads well aloud. Returns the answer, if one arrived.
    @discardableResult
    func send(_ text: String, spoken: Bool = false) async -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !isThinking else { return nil }
        items.append(.user(trimmed))
        save()
        // Voice mode is Tori: she can also run the box's computer.
        return await respond(concise: spoken, tori: spoken)
    }

    /// Answer the transcript as it stands: stream the reply into a new assistant
    /// item, placing any actions the assistant takes just above it.
    @discardableResult
    private func respond(concise: Bool = false, tori: Bool = false) async -> String? {
        let detailed = concise ? false : self.detailed
        let history = Array(turns.suffix(24))
        var answer = ChatItem(role: .assistant)
        answer.isStreaming = true
        answer.model = status?.servingModel
        let answerID = answer.id
        items.append(answer)
        isThinking = true
        let task = Task { [weak self] in
            guard let self else { return }
            do {
                for try await event in self.client.streamChat(history, detailed: detailed, think: self.think, tori: tori) {
                    self.apply(event, to: answerID)
                }
            } catch is CancellationError {
                // Stopped by the user: keep what arrived.
            } catch AgentError.http(404, _) {
                // An agent that predates streaming: ask the plain way.
                await self.respondWithoutStreaming(history, into: answerID)
            } catch {
                self.noteAuthFailure(error)
                self.fail(answerID, error.localizedDescription)
            }
        }
        streamTask = task
        await task.value
        finish(answerID)
        return items.first(where: { $0.id == answerID && $0.role == .assistant })?.text
    }

    private func apply(_ event: ChatEvent, to id: UUID) {
        defer {
            if let item = items.first(where: { $0.id == id }) { onAnswerText?(item.text) }
        }
        guard let index = items.firstIndex(where: { $0.id == id }) else { return }
        switch event {
        case .delta(let text): items[index].text += text
        case .reasoning(let text): items[index].reasoning += text
        case .reset: items[index].text = ""
        case .action(let action):
            items.insert(ChatItem(role: .action, action: action), at: index)
        case let .done(reply, model):
            items[index].text = reply
            if let model { items[index].model = model }
        case .failed(let message):
            fail(id, message)
        }
    }

    private func respondWithoutStreaming(_ history: [ChatTurn], into id: UUID) async {
        do {
            let response = try await client.ask(history)
            guard let index = items.firstIndex(where: { $0.id == id }) else { return }
            for action in response.actions {
                items.insert(ChatItem(role: .action, action: action), at: index)
            }
            if let index = items.firstIndex(where: { $0.id == id }) {
                items[index].text = response.reply
                items[index].model = response.model
            }
        } catch {
            noteAuthFailure(error)
            fail(id, error.localizedDescription)
        }
    }

    /// Turn an answer that failed into an error line (keeping partial text, if any).
    private func fail(_ id: UUID, _ message: String) {
        guard let index = items.firstIndex(where: { $0.id == id }) else { return }
        if items[index].text.isEmpty {
            items[index] = .error(message)
        } else {
            items[index].isStreaming = false
            items.insert(.error(message), at: index + 1)
        }
    }

    private func finish(_ id: UUID) {
        if let index = items.firstIndex(where: { $0.id == id }) {
            items[index].isStreaming = false
            if items[index].role == .assistant && items[index].text.isEmpty && items[index].reasoning.isEmpty {
                items.remove(at: index)
            }
        }
        isThinking = false
        streamTask = nil
        save()
    }

    /// Stop the answer being written; what arrived so far stays.
    func stopGenerating() {
        streamTask?.cancel()
    }

    /// Ask again for the last answer.
    func regenerate() async {
        guard !isThinking, let lastUser = items.lastIndex(where: { $0.role == .user }) else { return }
        items.removeSubrange((lastUser + 1)...)
        await respond()
    }

    /// Take back a message: drop it and everything after; returns its text so the
    /// composer can offer it for editing.
    func edit(_ item: ChatItem) -> String? {
        guard !isThinking, item.role == .user, let index = items.firstIndex(where: { $0.id == item.id }) else { return nil }
        items.removeSubrange(index...)
        save()
        return item.text
    }

    func newChat() {
        guard !isThinking else { return }
        save()
        items = []
        conversationID = UUID()
    }

    func open(_ conversation: Conversation) {
        guard !isThinking else { return }
        save()
        conversationID = conversation.id
        items = conversation.items
    }

    private func save() {
        conversations.save(id: conversationID, boxID: box.id, items: items)
    }

    func resolve(_ action: AssistantAction, approve: Bool) async {
        guard let index = items.firstIndex(where: { $0.role == .action && $0.action?.id == action.id && $0.resolved == nil })
        else { return }
        items[index].resolved = approve ? "Working…" : "Cancelled"
        do {
            let result = try await client.confirm(actionID: action.id, approve: approve)
            items[index].resolved = result.summary ?? result.status.capitalized
            if result.status == "started" {
                pendingStart = action.args?.model ?? "the model"
            }
            save()
            await refreshStatus()
        } catch {
            items[index].resolved = "Failed: \(error.localizedDescription)"
        }
    }

    // MARK: models

    func loadCatalog(force: Bool = false) async {
        if catalog != nil && !force { return }
        do {
            let result = try await client.ttModels()
            catalog = result.models
            pulls = result.pulls
            diskFreeBytes = result.diskFreeBytes
            canDownload = true
            catalogError = nil
            if pulls.contains(where: \.isRunning) { watchPulls() }
        } catch AgentError.http(409, _) {
            // No tt CLI on the box: fall back to the agent's own model list.
            let mesh = status?.deviceMesh?.uppercased()
            let legacy = (try? await client.models()) ?? []
            catalog = legacy
                .filter { model in mesh.map { m in model.devices.contains { $0.uppercased() == m } } ?? true }
                .map(TtModel.init(legacy:))
            canDownload = false
            catalogError = nil
        } catch {
            catalogError = error.localizedDescription
        }
    }

    /// The latest job for `model`, if the box has one.
    func pull(for model: TtModel) -> PullJob? {
        pulls.first { $0.model == model.name }
    }

    /// Start downloading `model` on the box, then watch progress until it ends.
    func download(_ model: TtModel) async throws {
        onLongOperation?()
        let result: String
        do { result = try await client.pull(model: model.name) } catch { noteAuthFailure(error); throw error }
        if result == "already_downloaded" || result == "not_needed" {
            await loadCatalog(force: true)
            return
        }
        await refreshPulls()
        watchPulls()
    }

    func cancelDownload(_ model: TtModel) async throws {
        try await client.cancelPull(model: model.name)
        await refreshPulls()
    }

    private func refreshPulls() async {
        guard let info = try? await client.ttPulls() else { return }
        pulls = info.pulls
        diskFreeBytes = info.diskFreeBytes ?? diskFreeBytes
    }

    /// Poll download progress every two seconds while anything is running; when the
    /// last one ends, reload the catalog so "downloaded" flags update.
    private func watchPulls() {
        guard pullPollTask == nil else { return }
        pullPollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                guard let self else { return }
                await self.refreshPulls()
                if !self.pulls.contains(where: \.isRunning) {
                    self.pullPollTask = nil
                    await self.loadCatalog(force: true)
                    return
                }
            }
        }
    }

    /// Serve `model`. The agent brings it up in the background (`POST /serve`), so
    /// this returns at once and keeps working if the app is backgrounded; status
    /// polling picks up "ready" or the reason it failed. Older agents fall back to
    /// the blocking `/run`.
    func start(model: String) async throws {
        onLongOperation?()
        pendingStart = model
        do {
            try await client.serve(model: model)
        } catch AgentError.http(404, _) {
            let client = self.client
            Task { try? await client.run(model: model) }
        } catch {
            pendingStart = nil
            noteAuthFailure(error)
            throw error
        }
        items.append(.notice("Starting \(Brand.shortModel(model)). This can take a few minutes."))
        await refreshStatus()
    }

    // MARK: generation

    func generateImage(_ request: ImageRequest) async throws -> ImageResult {
        do { return try await client.generateImage(request) } catch { noteAuthFailure(error); throw error }
    }

    func submitVideo(_ request: VideoRequest) async throws -> VideoSubmission {
        do { return try await client.submitVideo(request) } catch { noteAuthFailure(error); throw error }
    }

    func videoJob(id: String) async throws -> VideoJob { try await client.videoJob(id: id) }
    func downloadVideo(id: String) async throws -> Data { try await client.downloadVideo(id: id) }
    func cancelVideo(id: String) async throws { try await client.cancelVideo(id: id) }

    // MARK: direct controls (the Box tab)

    func stopServing() async throws {
        do { try await client.stop() } catch { noteAuthFailure(error); throw error }
        await refreshStatus()
    }

    func resetChips() async throws {
        do { try await client.resetChips() } catch { noteAuthFailure(error); throw error }
        await refreshStatus()
    }

    func metricsHistory(range: String) async throws -> MetricsHistory { try await client.metricsHistory(range: range) }
    /// Chat-drawn images already copied into the phone's gallery: box image id → gallery id.
    private static let importedKey = "tt.chat.importedImages"
    @ObservationIgnored private var imported: [String: String] =
        UserDefaults.standard.dictionary(forKey: BoxSession.importedKey) as? [String: String] ?? [:]

    /// The gallery entry for a chat-drawn image, downloading and adding it the first time.
    func galleryImage(boxImageID id: String, prompt: String?, into store: CreationStore) async throws -> Creation {
        if let known = imported[id].flatMap(UUID.init(uuidString:)),
           let creation = store.items.first(where: { $0.id == known }) {
            return creation
        }
        let data = try await client.creation(id: id)
        let creation = Creation(id: UUID(), kind: .image, prompt: prompt ?? "", model: companion?.model ?? "image model",
                                seed: nil, steps: 4, createdAt: Date(), boxID: box.id, seconds: nil,
                                fileName: try store.write(data, ext: "jpg"), jobID: nil, status: "done", error: nil)
        store.add(creation)
        imported[id] = creation.id.uuidString
        UserDefaults.standard.set(imported, forKey: Self.importedKey)
        return creation
    }

    func connectInfo() async throws -> ConnectInfo { try await client.connectInfo() }
    func bench() async throws -> BenchResult { try await client.bench() }
    func modes() async throws -> [BoxMode] { try await client.modes() }
    func startMode(_ mode: BoxMode) async throws {
        onLongOperation?()
        try await client.startMode(mode.id)
        pendingStart = mode.models.first
        items.append(.notice("Switching to \(mode.title): \(mode.models.map(Brand.shortModel).joined(separator: " + "))."))
        await refreshStatus()
    }
    func doctor() async throws -> DoctorReport { try await client.doctor() }
    func applyFix(_ fix: String) async throws -> DoctorReport { try await client.applyFix(fix) }
    func diagnostics() async throws -> (name: String, data: Data) { try await client.diagnostics() }
    func software() async throws -> SoftwareReport { try await client.software() }
    func updateTt() async throws { try await client.updateTt() }
    func removeModel(_ model: TtModel) async throws -> Int64 {
        let freed = try await client.removeModel(model.name, weights: true)
        await loadCatalog(force: true)
        return freed
    }
    func webClients() async throws -> [WebClient] { try await client.webClients() }
    func lab() async throws -> LabStatus { try await client.lab() }
    func orchard(fresh: Bool = false) async throws -> OrchardStatus { try await client.orchard(fresh: fresh) }
    func serveOrchard(_ name: String, stopServing: Bool) async throws {
        try await client.serveOrchard(name, stopServing: stopServing)
        await refreshStatus()
    }
    func setOrchardPackaging(_ enabled: Bool) async throws { try await client.setOrchardPackaging(enabled) }
    func people() async throws -> PeopleList { try await client.people() }
    func addPerson(name: String, me: Bool) async throws -> PeopleList { try await client.addPerson(name: name, me: me) }
    func setMe(_ person: String?) async throws -> PeopleList { try await client.setMe(person) }
    func renamePerson(_ id: String, to name: String) async throws -> PeopleList { try await client.renamePerson(id, to: name) }
    func removePerson(_ id: String) async throws -> PeopleList { try await client.removePerson(id) }
    func schedules() async throws -> [ModeSchedule] { try await client.schedules() }
    func addSchedule(mode: String, time: String, days: [Int]) async throws -> [ModeSchedule] {
        try await client.addSchedule(mode: mode, time: time, days: days)
    }
    func setSchedule(_ id: String, enabled: Bool) async throws -> [ModeSchedule] { try await client.setSchedule(id, enabled: enabled) }
    func deleteSchedule(_ id: String) async throws -> [ModeSchedule] { try await client.deleteSchedule(id) }
    func sshInfo() async throws -> SSHInfo { try await client.sshInfo() }
    func revokeSSH(publicKey: String) async throws { try await client.revokeSSH(publicKey: publicKey) }
    func energy() async throws -> EnergyReport { try await client.energy() }
    func voice() async throws -> BoxVoiceStatus { try await client.voice() }
    func bringupRuns() async throws -> [BringupRun] { try await client.bringupRuns() }
    func decideBringup(run: String, approval: String, decision: String) async throws {
        try await client.decideBringup(run: run, approval: approval, decision: decision)
    }

    func setVoice(_ id: String) async throws { try await client.setVoice(id) }
    func speak(_ text: String, voice: String?, pcm: Bool = true) async throws -> Data {
        try await client.speak(text, voice: voice, pcm: pcm)
    }
    func orchardSetup() async throws { try await client.orchardSetup() }
    func orchardPreflight(model: String, acceptCredentials: Bool) async throws -> OrchardPreflight {
        try await client.orchardPreflight(model: model, acceptCredentials: acceptCredentials)
    }
    func startOrchard(model: String, stopServing: Bool, acceptCredentials: Bool) async throws {
        try await client.startOrchard(model: model, stopServing: stopServing, acceptCredentials: acceptCredentials)
    }
    func orchardRun(_ name: String) async throws -> OrchardRun { try await client.orchardRun(name) }
    func orchardControl(_ name: String, action: String, stopServing: Bool = false) async throws {
        try await client.orchardControl(name, action: action, stopServing: stopServing)
    }
    func orchardReport(_ name: String, _ which: String) async throws -> String { try await client.orchardReport(name, which) }
    func jobs() async throws -> [BoxJob] { try await client.jobs() }
    func uploadTranscription(file: URL, name: String, tonight: Bool) async throws -> BoxJob {
        onLongOperation?()
        return try await client.uploadTranscription(file: file, name: name, tonight: tonight)
    }
    func runJobNow(_ id: String) async throws { try await client.runJobNow(id) }
    func deleteJob(_ id: String) async throws { try await client.deleteJob(id) }
    func jobResult(_ id: String, format: String = "txt") async throws -> Data { try await client.jobResult(id, format: format) }
    func prepareLab(model: String) async throws { try await client.prepareLab(model: model) }
    func runLab(_ run: LabRunRequest) -> AsyncThrowingStream<LabEvent, Error> { client.runLab(run) }
    func startWebClient(_ tool: String) async throws -> String { try await client.startWebClient(tool) }
    func stopWebClient(_ tool: String) async throws { try await client.stopWebClient(tool) }
    func memories() async throws -> [MemoryItem] { try await client.memories() }
    func events(since: Int) async throws -> (events: [BoxEvent], latest: Int) { try await client.events(since: since) }
    func notifySettings() async throws -> NotifySettings { try await client.notifySettings() }
    func registerPush(token: String, sandbox: Bool, name: String) async throws -> PushRegistration {
        try await client.registerPush(token: token, sandbox: sandbox, name: name)
    }
    func testNotification() async throws -> Int { try await client.testNotification() }
    func powerInfo() async throws -> PowerInfo {
        let info = try await client.powerInfo()
        // Remember how to wake it, for when it's asleep and can't be asked.
        if let target = WakeTarget(info) { WakeOnLAN.remember(target, for: box) }
        return info
    }
    func power(_ action: String) async throws { try await client.power(action) }
    func deleteMemory(id: String) async throws { try await client.deleteMemory(id: id) }
    func clearMemories() async throws { try await client.clearMemories() }
    func createAppKey(name: String) async throws -> (AppKeyInfo, String) { try await client.createAppKey(name: name) }
    func revokeAppKey(id: String) async throws { try await client.revokeAppKey(id: id) }
    func documents() async throws -> [DocSummary] { try await client.documents() }
    func addDocument(name: String, text: String) async throws -> DocSummary {
        try await client.addDocument(name: name, text: text)
    }
    func deleteDocument(id: String) async throws { try await client.deleteDocument(id: id) }
    func metricsNow() async throws -> MetricsNow { try await client.metricsNow() }

    func logs(tail: Int = 40) async throws -> [String] {
        try await client.logs(tail: tail).lines.map(Self.compactLogLine)
    }

    /// Drop vLLM's "(EngineCore_DP0 pid=122) " process prefix; it's noise on a phone.
    private static func compactLogLine(_ line: String) -> String {
        guard line.hasPrefix("("), let end = line.range(of: ") "),
              line[line.startIndex..<end.lowerBound].contains(" pid=") else { return line }
        return String(line[end.upperBound...])
    }
}

// MARK: - Waiting for the person, Code, Routines

extension BoxSession {
    func approvals() async throws -> [Approval] { try await client.approvals() }
    func answer(approval id: String, allow: Bool, always: Bool = false) async throws {
        try await client.answer(approval: id, allow: allow, always: always)
    }
    func codeSessions() async throws -> [CodeSession] { try await client.codeSessions() }
    func codeSession(_ id: String, after: Int) async throws -> CodeSessionDetail { try await client.codeSession(id, after: after) }
    func sendCode(_ id: String, text: String, interrupt: Bool = false) async throws {
        try await client.sendCode(id, text: text, interrupt: interrupt)
    }
    func interruptCode(_ id: String) async throws { try await client.interruptCode(id) }
    func routines() async throws -> (routines: [Routine], running: RoutineRun?) { try await client.routines() }
    func runRoutine(_ id: String) async throws -> RoutineRun { try await client.runRoutine(id) }
    func routineRun(_ id: String) async throws -> RoutineRun { try await client.routineRun(id) }
    func stopRoutineRun(_ id: String) async throws { try await client.stopRoutineRun(id) }
}

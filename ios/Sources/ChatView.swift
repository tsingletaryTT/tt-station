import SwiftUI
import UIKit

/// The Assistant tab: chat with the active box. Answers stream in as the served
/// model writes them, render as Markdown, and are saved as conversations.
struct AssistantView: View {
    @Environment(AppModel.self) private var app
    @State private var showHistory = false
    @State private var showDocuments = false
    @State private var showMemories = false
    @State private var showVoice = false

    var body: some View {
        NavigationStack {
            Group {
                if let session = app.session {
                    ChatContent(session: session)
                } else {
                    ContentUnavailableView("No box selected", systemImage: "server.rack",
                                           description: Text("Add or pick a box in Settings."))
                }
            }
            .background(Brand.backdrop)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Menu {
                        Button("Conversations", systemImage: "text.bubble") { showHistory = true }
                        Button("What I remember", systemImage: "brain.head.profile") { showMemories = true }
                        Button("Documents", systemImage: "doc.text.magnifyingglass") { showDocuments = true }
                    } label: {
                        Image(systemName: "text.bubble")
                    }
                    .accessibilityLabel("Conversations, memory and documents")
                    .disabled(app.session == nil)
                }
                ToolbarItem(placement: .principal) {
                    if let session = app.session { ModelMenu(session: session) } else { TTLogo(height: 14) }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button { showVoice = true } label: { Image(systemName: "waveform") }
                        .accessibilityLabel("Talk")
                        .disabled(app.session == nil)
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button { app.session?.newChat() } label: { Image(systemName: "square.and.pencil") }
                        .accessibilityLabel("New chat")
                        .disabled(app.session?.items.isEmpty ?? true || app.session?.isThinking == true)
                }
            }
            .sheet(isPresented: $showHistory) {
                if let session = app.session { ConversationsSheet(session: session) }
            }
            .sheet(isPresented: $showDocuments) {
                if let session = app.session { DocumentsSheet(session: session) }
            }
            .sheet(isPresented: $showMemories) {
                if let session = app.session { MemoriesSheet(session: session) }
            }
            .fullScreenCover(isPresented: $showVoice) {
                if let session = app.session { VoiceModeView(session: session) }
            }
        }
    }
}

/// The served model as a menu: what's answering, and the downloaded chat
/// models you can switch to (with honest speed/quality hints).
private struct ModelMenu: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app

    private var chatModels: [TtModel] {
        (session.catalog ?? []).filter { $0.kind == "chat" && $0.isReady && $0.supported }
    }

    var body: some View {
        Menu {
            Section("Serving") {
                Text(session.status?.servingModel.map(Brand.shortModel) ?? "Nothing")
            }
            if !chatModels.isEmpty {
                Section("Switch to") {
                    ForEach(chatModels) { model in
                        Button {
                            Task { try? await session.start(model: model.name) }
                        } label: {
                            Label("\(Brand.shortModel(model.name)) — \(hint(model))",
                                  systemImage: model.worksWithAssistant ? "checkmark.seal" : "bubble.left")
                        }
                        .disabled(isServing(model) || session.startingModel != nil)
                    }
                }
            }
            Button("All models…") { app.selectedTab = .models }
        } label: {
            HStack(spacing: 6) {
                TTMark().fill(Brand.accent).frame(width: 14, height: 14)
                Text(title).font(.tt(.subheadline).weight(.semibold)).lineLimit(1)
                Image(systemName: "chevron.down").font(.tt(.caption2).weight(.bold)).foregroundStyle(.secondary)
            }
        }
        .task { await session.loadCatalog() }
    }

    private var title: String {
        if let starting = session.startingModel { return "Starting \(Brand.shortModel(starting))…" }
        return session.status?.servingModel.map(Brand.shortModel) ?? "No model"
    }

    private func isServing(_ model: TtModel) -> Bool {
        let served = session.status?.servingModel
        return Brand.shortModel(served ?? "") == model.name || served == model.hfRepo
    }

    /// Size is the honest proxy on this box: bigger answers better, smaller answers faster.
    private func hint(_ model: TtModel) -> String {
        guard let b = model.paramCount else { return "chat" }
        switch b {
        case ..<12: return "fastest"
        case ..<40: return "balanced"
        default: return "best answers"
        }
    }
}

private struct ChatContent: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @State private var draft = ""
    @FocusState private var inputFocused: Bool
    @State private var voice = VoiceInput()
    @State private var speaker = VoiceOutput()
    /// Hands-free: after an answer is read aloud, listen again.
    @State private var voiceMode = false
    @State private var showVoice = false

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    if let kind = session.servedKind, !session.canChat {
                        NonChatCard(session: session, kind: kind)
                            .padding(.top, 24)
                    } else if session.items.isEmpty {
                        if session.status != nil && !session.isServing && session.startingModel == nil {
                            NotServingCard { app.selectedTab = .models }
                                .padding(.top, 24)
                        } else {
                            EmptyChat(boxName: session.box.name, enabled: session.canChat) { prompt in
                                Task { await session.send(prompt) }
                            }
                            .padding(.top, 24)
                        }
                    }
                    ForEach(session.items) { item in
                        MessageRow(item: item,
                                   isLastAnswer: item.id == session.items.last(where: { $0.role == .assistant })?.id,
                                   busy: session.isThinking,
                                   onResolve: { action, approve in Task { await session.resolve(action, approve: approve) } },
                                   onRegenerate: { Task { await session.regenerate() } },
                                   onEdit: { if let text = session.edit(item) { draft = text; inputFocused = true } })
                            .id(item.id)
                    }
                    Color.clear.frame(height: 4).id("bottom")
                }
                .padding(.horizontal, 16)
            }
            .defaultScrollAnchor(.bottom)
            .scrollDismissesKeyboard(.interactively)
            .scrollIndicators(.hidden)
            // A short conversation has nothing to drag, so a tap anywhere in it
            // also puts the keyboard away (buttons in messages still work).
            .simultaneousGesture(TapGesture().onEnded { inputFocused = false })
            .onChange(of: session.items.count) {
                withAnimation { proxy.scrollTo("bottom", anchor: .bottom) }
            }
        }
        .safeAreaInset(edge: .bottom) {
            if voice.isListening {
                ListeningBar(voice: voice, onCancel: endVoice)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            } else {
                VStack(alignment: .trailing, spacing: 6) {
                    // The composer's Return adds a line, so typing gets its own way
                    // to put the keyboard away. It sits in this inset, above the text
                    // box, rather than on the keyboard's toolbar: iOS lays that bar out
                    // separately and it sometimes landed on top of the composer.
                    if inputFocused {
                        HideKeyboardButton { inputFocused = false }
                            .padding(.horizontal, 12)
                            .transition(.opacity.combined(with: .move(edge: .bottom)))
                    }
                    Composer(draft: $draft, focused: $inputFocused,
                             enabled: session.canChat,
                             busy: session.isThinking,
                             speaking: speaker.isSpeaking,
                             placeholder: placeholder,
                             onSend: send,
                             onStop: { session.stopGenerating() },
                             onMic: { showVoice = true },
                             onHush: endVoice)
                }
                .animation(.snappy, value: inputFocused)
            }
        }
        .animation(.snappy, value: voice.isListening)
        .alert("Voice", isPresented: Binding(
            get: { if case .denied = voice.state { true } else { false } },
            set: { if !$0 { voice.clearError() } })
        ) {
            Button("Open Settings") {
                if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
            }
            Button("OK", role: .cancel) {}
        } message: {
            if case .denied(let why) = voice.state { Text(why) }
        }
        .onDisappear(perform: endVoice)
        .fullScreenCover(isPresented: $showVoice) { VoiceModeView(session: session) }
    }

    private func endVoice() {
        voiceMode = false
        voice.cancel()
        speaker.stop()
    }

    private var placeholder: String {
        if session.startingModel != nil { return "Waiting for the model to load…" }
        if session.isServing && !session.canChat { return "Chat needs a chat model" }
        return session.isServing ? "Message \(session.box.name)" : "Start a model to chat"
    }

    private func send() {
        let text = draft
        draft = ""
        Task { await session.send(text) }
    }
}

// MARK: - Conversations

private struct ConversationsSheet: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            let list = app.conversations.conversations(for: session.box.id)
            Group {
                if list.isEmpty {
                    ContentUnavailableView("No conversations yet", systemImage: "text.bubble",
                                           description: Text("Chats with \(session.box.name) are saved here."))
                } else {
                    List {
                        ForEach(list) { conversation in
                            Button {
                                session.open(conversation)
                                dismiss()
                            } label: {
                                VStack(alignment: .leading, spacing: 3) {
                                    Text(conversation.title).font(.tt(.body).weight(.medium)).lineLimit(1)
                                    Text(conversation.updatedAt.formatted(.relative(presentation: .named)))
                                        .font(.tt(.caption)).foregroundStyle(.secondary)
                                }
                            }
                            .foregroundStyle(conversation.id == session.conversationID ? Brand.accent : .primary)
                        }
                        .onDelete { offsets in
                            for index in offsets { app.conversations.delete(list[index].id) }
                        }
                    }
                }
            }
            .navigationTitle("Conversations")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        session.newChat()
                        dismiss()
                    } label: { Image(systemName: "square.and.pencil") }
                }
            }
        }
        .presentationDetents([.medium, .large])
    }
}

// MARK: - Empty states

/// A non-chat model is serving: say what the box is doing, and offer the way to
/// use it (Create) or the way back to chatting (start a chat model).
private struct NonChatCard: View {
    let session: BoxSession
    let kind: String
    @Environment(AppModel.self) private var app
    @State private var error: String?

    /// The best downloaded chat model to switch back to: one that drives the assistant.
    private var chatModel: TtModel? {
        let ready = (session.catalog ?? []).filter { $0.kind == "chat" && $0.isReady && $0.supported }
        return ready.first { $0.name == "Qwen3-32B" } ?? ready.first { $0.worksWithAssistant } ?? ready.first
    }

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: icon).font(.system(size: 34)).foregroundStyle(Brand.accent)
            VStack(spacing: 6) {
                Text(title).font(.tt(.title3).weight(.semibold)).multilineTextAlignment(.center)
                Text("The box runs one model at a time, and chat needs a chat model.")
                    .font(.tt(.subheadline))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            GlassEffectContainer(spacing: 10) {
                VStack(spacing: 10) {
                    if kind == "image" || kind == "video" {
                        Button { app.selectedTab = .create } label: {
                            Label(kind == "image" ? "Make images" : "Make videos", systemImage: "wand.and.stars")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glassProminent)
                    }
                    if let chatModel {
                        Button {
                            Task {
                                do { try await session.start(model: chatModel.name) } catch { self.error = error.localizedDescription }
                            }
                        } label: {
                            Label("Switch to \(Brand.shortModel(chatModel.name))", systemImage: "bubble.left.and.text.bubble.right")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glass)
                        .disabled(session.startingModel != nil)
                    } else {
                        Button { app.selectedTab = .models } label: {
                            Label("Choose a chat model", systemImage: "square.stack.3d.up").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glass)
                    }
                }
                .controlSize(.large)
            }
            if let error { Text(error).font(.tt(.footnote)).foregroundStyle(.red) }
        }
        .padding(24)
        .card()
        .task { await session.loadCatalog() }
    }

    private var title: String {
        let model = session.serve?.displayModel ?? session.status?.servingModel.map(Brand.shortModel) ?? "A model"
        switch kind {
        case "image": return "\(model) is serving for images"
        case "video": return "\(model) is serving for video"
        case "speech": return "\(model) is serving for speech-to-text"
        case "tts": return "\(model) is serving for text-to-speech"
        default: return "\(model) is serving"
        }
    }

    private var icon: String {
        switch kind {
        case "image": return "photo.on.rectangle.angled"
        case "video": return "film.stack"
        case "speech": return "waveform"
        case "tts": return "speaker.wave.2.fill"
        default: return "cube"
        }
    }
}

private struct NotServingCard: View {
    let onStart: () -> Void

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "moon.zzz.fill")
                .font(.system(size: 34))
                .foregroundStyle(Brand.accent)
            VStack(spacing: 6) {
                Text("Nothing is serving").font(.tt(.title3).weight(.semibold))
                Text("The assistant runs on the model your box serves. Start one to chat.")
                    .font(.tt(.subheadline))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            Button(action: onStart) {
                Label("Choose a model", systemImage: "square.stack.3d.up")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .controlSize(.large)
        }
        .padding(24)
        .card()
    }
}

private struct EmptyChat: View {
    let boxName: String
    let enabled: Bool
    let onPick: (String) -> Void

    private struct Prompt: Hashable {
        let icon: String
        let text: String
    }

    private let prompts = [
        Prompt(icon: "thermometer.medium", text: "What's running, and how hot are the chips?"),
        Prompt(icon: "square.stack.3d.up", text: "What models can this box run?"),
        Prompt(icon: "doc.text.magnifyingglass", text: "Anything wrong in the recent logs?"),
    ]

    var body: some View {
        VStack(spacing: 22) {
            TTMark()
                .fill(Brand.accent.gradient)
                .frame(width: 56, height: 56)
                .shadow(color: Brand.accent.opacity(0.5), radius: 24)
            VStack(spacing: 6) {
                Text("Ask \(boxName) anything").font(.tt(.title3).weight(.semibold))
                Text("It answers with the model it's serving, and checks with you before changing anything.")
                    .font(.tt(.subheadline))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            GlassEffectContainer(spacing: 10) {
                VStack(spacing: 10) {
                    ForEach(prompts, id: \.self) { prompt in
                        Button { onPick(prompt.text) } label: {
                            HStack(spacing: 12) {
                                Image(systemName: prompt.icon)
                                    .foregroundStyle(Brand.accent)
                                    .frame(width: 22)
                                Text(prompt.text).multilineTextAlignment(.leading)
                                Spacer()
                                Image(systemName: "arrow.up.right")
                                    .font(.tt(.caption))
                                    .foregroundStyle(.tertiary)
                            }
                            .font(.tt(.subheadline))
                            .padding(14)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .card(radius: 18, interactive: true)
                    }
                }
            }
            .disabled(!enabled)
            .opacity(enabled ? 1 : 0.4)
        }
        .frame(maxWidth: .infinity)
    }
}

// MARK: - Messages

private struct MessageRow: View {
    let item: ChatItem
    let isLastAnswer: Bool
    let busy: Bool
    let onResolve: (AssistantAction, Bool) -> Void
    let onRegenerate: () -> Void
    let onEdit: () -> Void

    var body: some View {
        switch item.role {
        case .user:
            HStack {
                Spacer(minLength: 48)
                Text(item.text)
                    .foregroundStyle(.white)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .background(Brand.accent, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                    .textSelection(.enabled)
                    .contextMenu {
                        Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = item.text }
                        if !busy { Button("Edit", systemImage: "pencil", action: onEdit) }
                    }
            }
        case .assistant:
            AnswerView(item: item, isLast: isLastAnswer, busy: busy, onRegenerate: onRegenerate)
        case .action:
            if let action = item.action {
                if action.needsConfirmation {
                    ConfirmCard(action: action, resolved: item.resolved, onResolve: onResolve)
                } else {
                    ToolChip(action: action)
                    if let images = action.images, !images.isEmpty {
                        ChatImages(ids: images, prompt: action.args?.prompt)
                    }
                }
            }
        case .notice:
            Text(item.text)
                .font(.tt(.footnote))
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity)
                .multilineTextAlignment(.center)
        case .error:
            Label(item.text, systemImage: "exclamationmark.triangle.fill")
                .font(.tt(.footnote))
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .card(tint: .red.opacity(0.3), radius: 16)
        }
    }
}

/// An answer: full width (not a bubble -- it's for reading), optional reasoning
/// folded above it, a cursor while it streams, and actions once it's done.
private struct AnswerView: View {
    let item: ChatItem
    let isLast: Bool
    let busy: Bool
    let onRegenerate: () -> Void
    @State private var showReasoning = false
    @State private var copied = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                TTMark().fill(Brand.accent).frame(width: 16, height: 16)
                Text(item.model.map(Brand.shortModel) ?? "Assistant")
                    .font(.tt(.caption).weight(.semibold))
                    .foregroundStyle(.secondary)
            }
            if !item.reasoning.isEmpty {
                DisclosureGroup(isExpanded: $showReasoning) {
                    Text(item.reasoning)
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .padding(.top, 6)
                } label: {
                    Label(item.isStreaming && item.text.isEmpty ? "Thinking…" : "Thought it through",
                          systemImage: "brain")
                        .font(.tt(.caption).weight(.medium))
                        .foregroundStyle(.secondary)
                }
                .padding(10)
                .background(Color.white.opacity(0.04), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
            if item.text.isEmpty && item.isStreaming {
                TypingIndicator()
            } else {
                MarkdownView(text: item.text + (item.isStreaming ? " ▍" : ""))
                    .textSelection(.enabled)
            }
            if !item.isStreaming && !item.text.isEmpty {
                HStack(spacing: 18) {
                    Button {
                        UIPasteboard.general.string = item.text
                        copied = true
                        Task {
                            try? await Task.sleep(for: .seconds(1.5))
                            copied = false
                        }
                    } label: { Image(systemName: copied ? "checkmark" : "doc.on.doc") }
                        .accessibilityLabel("Copy")
                    ShareLink(item: item.text) { Image(systemName: "square.and.arrow.up") }
                    if isLast && !busy {
                        Button(action: onRegenerate) { Image(systemName: "arrow.clockwise") }
                            .accessibilityLabel("Regenerate")
                    }
                    Spacer()
                }
                .font(.tt(.footnote))
                .foregroundStyle(.secondary)
                .buttonStyle(.plain)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Pictures the assistant drew, inline in the chat; tap for the viewer (save, share).
private struct ChatImages: View {
    let ids: [String]
    let prompt: String?
    @Environment(AppModel.self) private var app
    @State private var loaded: [String: Creation] = [:]
    @State private var failed: Set<String> = []
    @State private var viewing: Creation?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(ids, id: \.self) { id in
                Group {
                    if let creation = loaded[id], let image = app.creations.image(for: creation) {
                        Button { viewing = creation } label: {
                            Image(uiImage: image)
                                .resizable()
                                .scaledToFit()
                                .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
                        }
                        .buttonStyle(.plain)
                        .contextMenu {
                            ShareLink(item: Image(uiImage: image), preview: SharePreview(prompt ?? "Image", image: Image(uiImage: image)))
                        }
                    } else if failed.contains(id) {
                        Label("Couldn't load this picture from the box.", systemImage: "photo.badge.exclamationmark")
                            .font(.tt(.footnote))
                            .foregroundStyle(.secondary)
                    } else {
                        RoundedRectangle(cornerRadius: 18, style: .continuous)
                            .fill(Color.white.opacity(0.06))
                            .aspectRatio(1, contentMode: .fit)
                            .overlay(ProgressView())
                    }
                }
                .frame(maxWidth: 360)
                .task(id: id) { await load(id) }
            }
        }
        .padding(.leading, 28)
        .sheet(item: $viewing) { CreationViewer(creation: $0) }
    }

    private func load(_ id: String) async {
        guard loaded[id] == nil, let session = app.session else { return }
        do {
            loaded[id] = try await session.galleryImage(boxImageID: id, prompt: prompt, into: app.creations)
        } catch {
            failed.insert(id)
        }
    }
}

private struct ToolChip: View {
    let action: AssistantAction

    private var icon: String {
        if action.status == "error" { return "xmark.circle" }
        switch action.tool {
        case "search_documents": return "doc.text.magnifyingglass"
        case "web_search": return "globe"
        case "read_webpage": return "safari"
        case "create_image": return "paintbrush.pointed"
        case "remember": return "brain.head.profile"
        case "forget": return "eraser"
        default: return "checkmark.circle"
        }
    }

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: icon)
            Text(action.summary ?? action.tool)
        }
        .font(.tt(.caption))
        .foregroundStyle(action.status == "error" ? Color.red : Color.secondary)
        .padding(.leading, 28)
    }
}

/// A disruptive action waiting for the human: what, what it will cost, Confirm / Cancel.
private struct ConfirmCard: View {
    let action: AssistantAction
    let resolved: String?
    let onResolve: (AssistantAction, Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Image(systemName: icon)
                    .font(.tt(.headline))
                    .foregroundStyle(Brand.accent)
                    .frame(width: 36, height: 36)
                    .glassEffect(.regular.tint(Brand.accent.opacity(0.3)), in: .rect(cornerRadius: 10))
                VStack(alignment: .leading, spacing: 1) {
                    Text(title).font(.tt(.headline))
                    if let model = action.args?.model {
                        Text(Brand.shortModel(model)).font(.ttMono(12, relativeTo: .caption)).foregroundStyle(.secondary)
                    }
                }
            }
            if let summary = action.summary {
                Text(summary).font(.tt(.subheadline)).foregroundStyle(.secondary)
            }
            if let resolved {
                HStack(spacing: 6) {
                    if resolved == "Working…" { ProgressView().controlSize(.small) }
                    Text(resolved)
                }
                .font(.tt(.subheadline).weight(.medium))
            } else {
                GlassEffectContainer(spacing: 10) {
                    HStack(spacing: 10) {
                        Button { onResolve(action, false) } label: {
                            Text("Cancel").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glass)
                        Button { onResolve(action, true) } label: {
                            Text("Confirm").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.glassProminent)
                    }
                    .controlSize(.large)
                }
            }
        }
        .padding(16)
        .card(tint: resolved == nil ? Brand.accent.opacity(0.18) : nil)
    }

    private var title: String {
        switch action.tool {
        case "run_model": return "Start a model"
        case "stop_model": return "Stop the model"
        case "reset_chips": return "Reset the chips"
        default: return action.tool
        }
    }

    private var icon: String {
        switch action.tool {
        case "run_model": return "play.fill"
        case "stop_model": return "stop.fill"
        case "reset_chips": return "arrow.counterclockwise"
        default: return "hand.raised.fill"
        }
    }
}

private struct TypingIndicator: View {
    var body: some View {
        TimelineView(.animation) { context in
            let t = context.date.timeIntervalSinceReferenceDate
            HStack(spacing: 5) {
                ForEach(0..<3, id: \.self) { dot in
                    Circle()
                        .fill(Color.secondary)
                        .frame(width: 7, height: 7)
                        .opacity(0.35 + 0.65 * max(0, sin(t * 5 - Double(dot) * 0.8)))
                }
            }
        }
        .padding(.vertical, 6)
        .accessibilityLabel("Thinking")
    }
}

// MARK: - Composer

/// A floating glass composer: answer-style options, a capsule field, and a send
/// button that becomes stop while an answer streams.
private struct Composer: View {
    @Binding var draft: String
    var focused: FocusState<Bool>.Binding
    let enabled: Bool
    let busy: Bool
    let speaking: Bool
    let placeholder: String
    let onSend: () -> Void
    let onStop: () -> Void
    let onMic: () -> Void
    let onHush: () -> Void
    @AppStorage(BoxSession.detailedKey) private var detailed = true
    @AppStorage(BoxSession.thinkKey) private var think = false

    private var canSend: Bool {
        enabled && !busy && !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// An empty composer offers the microphone instead of a dead send button.
    /// It's there even without a chat model: voice mode offers to switch.
    private var showMic: Bool {
        !busy && draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private var primaryIcon: String {
        if busy { return "stop.fill" }
        if speaking { return "speaker.slash.fill" }
        return showMic ? "mic.fill" : "arrow.up"
    }

    private func primaryAction() {
        if busy { onStop() } else if speaking { onHush() } else if showMic { onMic() } else { onSend() }
    }

    var body: some View {
        GlassEffectContainer(spacing: 8) {
            HStack(alignment: .bottom, spacing: 8) {
                Menu {
                    Picker("Answers", selection: $detailed) {
                        Label("Detailed", systemImage: "text.alignleft").tag(true)
                        Label("Concise", systemImage: "text.word.spacing").tag(false)
                    }
                    Toggle(isOn: $think) { Label("Think first", systemImage: "brain") }
                } label: {
                    Image(systemName: think ? "brain.fill" : "slider.horizontal.3")
                        .font(.tt(.body).weight(.semibold))
                        .foregroundStyle(think ? Brand.accent : .secondary)
                        .frame(width: 46, height: 46)
                }
                .glassEffect(.regular.interactive(), in: .circle)
                .accessibilityLabel("Answer options")

                TextField(placeholder, text: $draft, axis: .vertical)
                    .lineLimit(1...6)
                    .focused(focused)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 12)
                    .glassEffect(.regular.interactive(), in: .rect(cornerRadius: 24))
                    .disabled(!enabled)

                Button(action: primaryAction) {
                    Image(systemName: primaryIcon)
                        .font(.tt(.body).weight(.bold))
                        .foregroundStyle(.white)
                        .frame(width: 46, height: 46)
                        .contentTransition(.symbolEffect(.replace))
                }
                .glassEffect(busy || canSend || speaking || showMic
                             ? .regular.tint(Brand.accent).interactive() : .regular, in: .circle)
                .disabled(!busy && !canSend && !speaking && !showMic)
                .accessibilityLabel(busy ? "Stop" : speaking ? "Stop speaking" : showMic ? "Talk" : "Send")
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
    }
}

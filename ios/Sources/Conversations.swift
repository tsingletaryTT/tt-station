import Foundation

/// One line in a chat transcript. A struct with a role (rather than an enum with
/// payloads) so a streaming answer can be appended to in place, and so the whole
/// transcript is `Codable` for the conversation store.
struct ChatItem: Identifiable, Equatable, Codable {
    enum Role: String, Codable { case user, assistant, action, notice, error }

    var id = UUID()
    var role: Role
    var text: String = ""
    /// The model's thinking (with "Think" on), shown collapsed above the answer.
    var reasoning: String = ""
    /// Still arriving from the box.
    var isStreaming = false
    /// For `.action`: the tool the assistant ran or wants confirmed.
    var action: AssistantAction?
    /// For `.action`: what happened after Confirm/Cancel.
    var resolved: String?
    /// For `.assistant`: the served model that wrote it.
    var model: String?
    var date = Date()

    static func user(_ text: String) -> ChatItem { ChatItem(role: .user, text: text) }
    static func notice(_ text: String) -> ChatItem { ChatItem(role: .notice, text: text) }
    static func error(_ text: String) -> ChatItem { ChatItem(role: .error, text: text) }
}

/// A saved chat with one box.
struct Conversation: Identifiable, Codable, Equatable {
    var id = UUID()
    var boxID: String
    var title: String
    var createdAt = Date()
    var updatedAt = Date()
    var items: [ChatItem] = []

    /// A title from the first thing the user asked.
    static func title(for items: [ChatItem]) -> String {
        guard let first = items.first(where: { $0.role == .user })?.text else { return "New chat" }
        let line = first.split(separator: "\n").first.map(String.init) ?? first
        return line.count > 48 ? String(line.prefix(47)) + "…" : line
    }
}

/// Saved conversations, newest first, in `Documents/Conversations.json`.
@MainActor
@Observable
final class ConversationStore {
    private(set) var all: [Conversation] = []
    private let url: URL
    private static let limit = 200

    init() {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        url = documents.appendingPathComponent("Conversations.json")
        if let data = try? Data(contentsOf: url),
           let saved = try? JSONDecoder().decode([Conversation].self, from: data) {
            all = saved
        }
    }

    func conversations(for boxID: String) -> [Conversation] {
        all.filter { $0.boxID == boxID }
    }

    func conversation(_ id: UUID) -> Conversation? { all.first { $0.id == id } }

    /// Save a transcript (dropping streaming state); creates the conversation the
    /// first time. Empty transcripts aren't saved.
    func save(id: UUID, boxID: String, items: [ChatItem]) {
        let settled = items.map { item -> ChatItem in
            var copy = item
            copy.isStreaming = false
            return copy
        }
        guard settled.contains(where: { $0.role == .user }) else { return }
        if let index = all.firstIndex(where: { $0.id == id }) {
            all[index].items = settled
            all[index].updatedAt = Date()
            if all[index].title == "New chat" { all[index].title = Conversation.title(for: settled) }
            let updated = all.remove(at: index)
            all.insert(updated, at: 0)
        } else {
            all.insert(Conversation(id: id, boxID: boxID, title: Conversation.title(for: settled), items: settled), at: 0)
        }
        if all.count > Self.limit { all.removeLast(all.count - Self.limit) }
        persist()
    }

    func rename(_ id: UUID, to title: String) {
        guard let index = all.firstIndex(where: { $0.id == id }) else { return }
        all[index].title = title
        persist()
    }

    func delete(_ id: UUID) {
        all.removeAll { $0.id == id }
        persist()
    }

    private func persist() {
        if let data = try? JSONEncoder().encode(all) {
            try? data.write(to: url, options: .atomic)
        }
    }
}

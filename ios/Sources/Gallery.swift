import Foundation
import UIKit

/// Something the box made: an image, or a video (which may still be rendering on
/// the box -- `jobID` + `status` let polling resume after the app was closed).
struct Creation: Codable, Identifiable, Hashable {
    enum Kind: String, Codable { case image, video }

    let id: UUID
    let kind: Kind
    let prompt: String
    let model: String
    let seed: Int?
    let steps: Int
    let createdAt: Date
    let boxID: String
    /// Seconds the box took (images: reported by the server; videos: wall clock).
    var seconds: Double?
    /// File in the gallery directory, once the bytes are here.
    var fileName: String?
    /// The box's job id, for videos still rendering.
    var jobID: String?
    /// `done`, or a video job's status: `queued`, `in_progress`, `failed`, `cancelled`, `cancelling`.
    var status: String
    var error: String?

    var isPending: Bool { ["queued", "in_progress", "cancelling", "downloading"].contains(status) }
}

/// The on-device gallery: files in `Documents/Creations`, indexed by `index.json`.
@MainActor
@Observable
final class CreationStore {
    private(set) var items: [Creation] = []
    private let directory: URL
    private var thumbnails: [UUID: UIImage] = [:]

    init() {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        directory = documents.appendingPathComponent("Creations", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        if let data = try? Data(contentsOf: indexURL),
           let saved = try? JSONDecoder().decode([Creation].self, from: data) {
            items = saved
        }
    }

    private var indexURL: URL { directory.appendingPathComponent("index.json") }

    func items(_ kind: Creation.Kind) -> [Creation] { items.filter { $0.kind == kind } }

    func url(for creation: Creation) -> URL? {
        creation.fileName.map { directory.appendingPathComponent($0) }
    }

    func image(for creation: Creation) -> UIImage? {
        if let cached = thumbnails[creation.id] { return cached }
        guard creation.kind == .image, let url = url(for: creation),
              let image = UIImage(contentsOfFile: url.path) else { return nil }
        thumbnails[creation.id] = image
        return image
    }

    /// Save bytes as a new creation's file; returns the file name.
    func write(_ data: Data, ext: String) throws -> String {
        let name = "\(UUID().uuidString).\(ext)"
        try data.write(to: directory.appendingPathComponent(name), options: .atomic)
        return name
    }

    func add(_ creation: Creation) {
        items.insert(creation, at: 0)
        persist()
    }

    func update(_ id: UUID, _ change: (inout Creation) -> Void) {
        guard let index = items.firstIndex(where: { $0.id == id }) else { return }
        change(&items[index])
        persist()
    }

    func delete(_ creation: Creation) {
        if let url = url(for: creation) { try? FileManager.default.removeItem(at: url) }
        thumbnails[creation.id] = nil
        items.removeAll { $0.id == creation.id }
        persist()
    }

    private func persist() {
        if let data = try? JSONEncoder().encode(items) {
            try? data.write(to: indexURL, options: .atomic)
        }
    }
}

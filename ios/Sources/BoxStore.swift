import Foundation
import Security

/// A box this phone knows how to reach. The bearer token lives in the Keychain,
/// keyed by `id`; everything else is plain UserDefaults.
struct SavedBox: Codable, Identifiable, Hashable {
    var name: String
    var host: String
    /// The box's control port (what its id is made of; requests go to `tlsPort`).
    var port: Int
    /// HTTPS port and pinned certificate (SHA-256 of the DER, lowercase hex), from
    /// the pairing QR. nil for a box paired before HTTPS: it must pair again, the
    /// app never talks plain HTTP. (Old saved data may also carry a `remoteHost`
    /// key from the removed remote access; decoding ignores it.)
    var tlsPort: Int? = nil
    var certSHA256: String? = nil
    /// The box's account for SSH, from the pairing link or `/ssh/authorize`.
    var sshUser: String? = nil

    var pin: TLSPin? {
        guard let tlsPort, let certSHA256 else { return nil }
        return TLSPin(port: tlsPort, sha256: certSHA256)
    }

    var id: String { "\(host):\(port)" }
}

@Observable
final class BoxStore {
    private(set) var boxes: [SavedBox] = []
    private let defaultsKey = "tt.mobile.boxes"

    init() {
        if let data = UserDefaults.standard.data(forKey: defaultsKey),
           let saved = try? JSONDecoder().decode([SavedBox].self, from: data) {
            boxes = saved
        }
        // Boxes paired before the notification extension shared credentials.
        for box in boxes { sharePush(box) }
    }

    func save(_ box: SavedBox, token: String) {
        Keychain.set(token, for: box.id)
        boxes.removeAll { $0.id == box.id }
        boxes.insert(box, at: 0)
        persist()
        sharePush(box)
    }

    /// Update a saved box's details in place (keeps its token and position).
    func update(_ box: SavedBox) {
        guard let index = boxes.firstIndex(where: { $0.id == box.id }) else { return }
        boxes[index] = box
        persist()
    }

    func forget(_ box: SavedBox) {
        Keychain.delete(box.id)
        PushCredentials.remove(box.id)
        boxes.removeAll { $0.id == box.id }
        persist()
    }

    func token(for box: SavedBox) -> String? { Keychain.get(box.id) }

    /// What the notification service extension needs to show a push's real
    /// text (pushes carry only an event id): see `PushCredentials`.
    private func sharePush(_ box: SavedBox) {
        guard let pin = box.pin, let token = token(for: box) else { return }
        let credential = PushCredential(id: box.id, name: box.name, host: box.host, tlsPort: pin.port,
                                        certSHA256: pin.sha256, token: token)
        if !PushCredentials.all().contains(credential) { PushCredentials.save(credential) }
    }

    private func persist() {
        if let data = try? JSONEncoder().encode(boxes) {
            UserDefaults.standard.set(data, forKey: defaultsKey)
        }
    }
}

enum Keychain {
    private static let service = "com.tenstorrent.ttstation.mobile"

    static func set(_ value: String, for account: String) {
        delete(account)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueData as String: Data(value.utf8),
        ]
        SecItemAdd(query as CFDictionary, nil)
    }

    static func get(_ account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func delete(_ account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}

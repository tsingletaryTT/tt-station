// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation
import Security

/// What the notification service extension needs to fetch a pushed event from
/// a box: its address, its pinned certificate and this phone's token for it.
/// The app keeps a copy here, in the Keychain group it shares with the
/// extension (`TTKeychainGroup` in each Info.plist, `keychain-access-groups`
/// in each entitlements file); a push itself carries only an event id.
struct PushCredential: Codable, Equatable {
    /// The app's id for the box (`SavedBox.id`, "host:port").
    let id: String
    let name: String
    let host: String
    let tlsPort: Int
    let certSHA256: String
    let token: String
}

enum PushCredentials {
    private static let service = "com.tenstorrent.ttstation.push"

    /// The shared Keychain group, or nil in a build without one (then nothing
    /// is shared and pushes show their generic text).
    private static var group: String? {
        (Bundle.main.object(forInfoDictionaryKey: "TTKeychainGroup") as? String).flatMap { $0.hasPrefix("$(") || $0.isEmpty ? nil : $0 }
    }

    private static func query(_ extra: [String: Any]) -> [String: Any]? {
        guard let group else { return nil }
        return [kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: service,
                kSecAttrAccessGroup as String: group].merging(extra) { $1 }
    }

    static func save(_ credential: PushCredential) {
        guard let data = try? JSONEncoder().encode(credential),
              let match = query([kSecAttrAccount as String: credential.id]),
              let add = query([kSecAttrAccount as String: credential.id,
                               // Readable while the phone is locked, after its first unlock: pushes arrive then.
                               kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
                               kSecValueData as String: data]) else { return }
        SecItemDelete(match as CFDictionary)
        SecItemAdd(add as CFDictionary, nil)
    }

    static func remove(_ id: String) {
        guard let match = query([kSecAttrAccount as String: id]) else { return }
        SecItemDelete(match as CFDictionary)
    }

    static func all() -> [PushCredential] {
        guard let q = query([kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitAll]) else { return [] }
        var items: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &items) == errSecSuccess, let list = items as? [Data] else { return [] }
        return list.compactMap { try? JSONDecoder().decode(PushCredential.self, from: $0) }
    }
}

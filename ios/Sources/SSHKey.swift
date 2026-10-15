// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import CryptoKit
import Foundation
import Security
import UIKit

/// This iPhone's own SSH key: one Ed25519 key per phone, reused for every box.
/// It is made on the phone and its private half stays in this device's Keychain
/// (`WhenUnlockedThisDeviceOnly`: never synced, never in a backup). Boxes only
/// ever get the public half, via `POST /ssh/authorize`.
struct SSHKey {
    let privateKey: Curve25519.Signing.PrivateKey

    private static let service = "com.tenstorrent.ttstation.ssh"
    private static let account = "ed25519"

    /// What this phone's key is called on a box (`authorized_keys` marker) and
    /// its comment: "iphone-<device name>", no whitespace.
    @MainActor static var label: String {
        let lowered = UIDevice.current.name.lowercased()
        let mapped = String(lowered.map { ($0.isASCII && ($0.isLetter || $0.isNumber)) ? $0 : "-" })
        let cleaned = mapped.split(separator: "-").joined(separator: "-")
        return "iphone-" + (cleaned.isEmpty ? "phone" : cleaned)
    }

    /// The phone's key, made (and stored) on first use.
    static func load() throws -> SSHKey {
        if let raw = readRaw() {
            return SSHKey(privateKey: try Curve25519.Signing.PrivateKey(rawRepresentation: raw))
        }
        let key = Curve25519.Signing.PrivateKey()
        let status = storeRaw(key.rawRepresentation)
        if status == errSecDuplicateItem, let raw = readRaw() {
            // Made at the same moment elsewhere: use the one that was stored.
            return SSHKey(privateKey: try Curve25519.Signing.PrivateKey(rawRepresentation: raw))
        }
        guard status == errSecSuccess else { throw SSHKeyError.keychain(status) }
        return SSHKey(privateKey: key)
    }

    // MARK: public half

    /// The SSH wire-format public key: string "ssh-ed25519", string pub32.
    var publicKeyBlob: Data {
        var blob = Data()
        blob.appendSSHString(Data("ssh-ed25519".utf8))
        blob.appendSSHString(privateKey.publicKey.rawRepresentation)
        return blob
    }

    /// The `authorized_keys` line: "ssh-ed25519 <base64> <comment>".
    @MainActor var publicKeyLine: String {
        "ssh-ed25519 \(publicKeyBlob.base64EncodedString()) \(Self.label)"
    }

    /// The fingerprint OpenSSH shows: "SHA256:" + unpadded base64 of SHA-256(blob).
    var fingerprint: String {
        var encoded = Data(SHA256.hash(data: publicKeyBlob)).base64EncodedString()
        while encoded.hasSuffix("=") { encoded.removeLast() }
        return "SHA256:" + encoded
    }

    // MARK: export (the private key leaves TT Station)

    /// The private key in OpenSSH's own format (`openssh-key-v1`, unencrypted),
    /// which SSH apps such as Termius and Blink import. Only for an explicit,
    /// confirmed export.
    @MainActor func openSSHPrivateKey() -> String {
        let publicRaw = privateKey.publicKey.rawRepresentation
        let comment = Self.label

        // The private section: checkint twice, then the one key, then padding.
        var section = Data()
        let check = UInt32.random(in: UInt32.min ... UInt32.max)
        section.appendUInt32(check)
        section.appendUInt32(check)
        section.appendSSHString(Data("ssh-ed25519".utf8))
        section.appendSSHString(publicRaw)
        // OpenSSH's Ed25519 private key: the 32-byte seed followed by the public key.
        section.appendSSHString(privateKey.rawRepresentation + publicRaw)
        section.appendSSHString(Data(comment.utf8))
        // Pad to the cipher block size (8 for "none") with 1, 2, 3, ...
        var pad: UInt8 = 1
        while section.count % 8 != 0 {
            section.append(pad)
            pad += 1
        }

        var body = Data("openssh-key-v1".utf8)
        body.append(0)
        body.appendSSHString(Data("none".utf8)) // cipher
        body.appendSSHString(Data("none".utf8)) // kdf
        body.appendSSHString(Data()) // kdf options
        body.appendUInt32(1) // number of keys
        body.appendSSHString(publicKeyBlob)
        body.appendSSHString(section)

        let encoded = body.base64EncodedString()
        var lines: [String] = []
        var index = encoded.startIndex
        while index < encoded.endIndex {
            let end = encoded.index(index, offsetBy: 70, limitedBy: encoded.endIndex) ?? encoded.endIndex
            lines.append(String(encoded[index..<end]))
            index = end
        }
        return "-----BEGIN OPENSSH PRIVATE KEY-----\n"
            + lines.joined(separator: "\n")
            + "\n-----END OPENSSH PRIVATE KEY-----\n"
    }

    // MARK: Keychain

    private static func readRaw() -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data, data.count == 32 else { return nil }
        return data
    }

    private static func storeRaw(_ raw: Data) -> OSStatus {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            kSecValueData as String: raw,
        ]
        return SecItemAdd(query as CFDictionary, nil)
    }
}

enum SSHKeyError: LocalizedError {
    case keychain(OSStatus)

    var errorDescription: String? {
        switch self {
        case .keychain(let status):
            let reason = SecCopyErrorMessageString(status, nil).map { $0 as String } ?? "error \(status)"
            return "Couldn't keep this iPhone's SSH key in the Keychain (\(reason))."
        }
    }
}

private extension Data {
    /// A big-endian uint32, as the SSH wire format writes integers.
    mutating func appendUInt32(_ value: UInt32) {
        append(contentsOf: [
            UInt8(truncatingIfNeeded: value >> 24),
            UInt8(truncatingIfNeeded: value >> 16),
            UInt8(truncatingIfNeeded: value >> 8),
            UInt8(truncatingIfNeeded: value),
        ])
    }

    /// An SSH `string`: uint32 length, then the bytes.
    mutating func appendSSHString(_ bytes: Data) {
        appendUInt32(UInt32(bytes.count))
        append(bytes)
    }
}

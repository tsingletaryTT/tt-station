import Foundation

/// A scanned pairing link, as the box's QR code spells it:
/// `ttstation://pair?host=…&pair=…&code=…&tls=…&fp=…` (and optionally
/// `port`, `name`, `ssh`). Pure parsing, so it can be tested; `AppModel.handle(url:)`
/// does the pairing.
struct PairLink: Equatable {
    let host: String
    let pairID: String
    let code: String
    /// The box's HTTPS port and certificate fingerprint (lowercase hex).
    let pin: TLSPin
    /// The box's control port (what its saved id is made of).
    let port: Int
    let name: String
    let sshUser: String?

    enum Problem: Error, Equatable {
        /// Not a pairing link at all.
        case notPairing
        /// No host, pair id or code.
        case incomplete
        /// No usable HTTPS port and certificate fingerprint.
        case noCertificate
    }

    static func parse(_ url: URL) -> Result<PairLink, Problem> {
        guard url.scheme == "ttstation", url.host == "pair",
              let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems else {
            return .failure(.notPairing)
        }
        func value(_ key: String) -> String? { items.first { $0.name == key }?.value.flatMap { $0.isEmpty ? nil : $0 } }
        guard let host = value("host"), let pairID = value("pair"), let code = value("code") else {
            return .failure(.incomplete)
        }
        guard let tls = value("tls").flatMap(Int.init), (1...65535).contains(tls),
              let fp = value("fp")?.lowercased(), isSHA256Hex(fp) else {
            return .failure(.noCertificate)
        }
        return .success(PairLink(host: host, pairID: pairID, code: code,
                                 pin: TLSPin(port: tls, sha256: fp),
                                 port: value("port").flatMap(Int.init) ?? 8765,
                                 name: value("name") ?? host,
                                 sshUser: value("ssh")))
    }

    /// 64 lowercase hex digits: a SHA-256 fingerprint.
    static func isSHA256Hex(_ text: String) -> Bool {
        text.count == 64 && text.allSatisfy { $0.isHexDigit && !$0.isUppercase }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import CryptoKit
import Foundation
import UserNotifications

/// Pushes from a box say only what kind of thing happened ("Your research has
/// news") and carry the event's id: what happened stays off Apple's servers.
/// This extension fetches the event from the box (`GET /events`, over the
/// app's pinned HTTPS with the phone's token) and shows its real title and
/// text, with Allow / Don't allow when it asks for an OK. On any failure (the
/// phone is away from the box, it's slow, no credentials) the generic text stays.
final class NotificationService: UNNotificationServiceExtension {
    private let lock = NSLock()
    private var handler: ((UNNotificationContent) -> Void)?
    private var generic: UNNotificationContent?
    private var work: Task<Void, Never>?

    override func didReceive(_ request: UNNotificationRequest,
                             withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void) {
        lock.withLock {
            handler = contentHandler
            generic = request.content
        }
        guard let seq = (request.content.userInfo["event"] as? NSNumber)?.intValue else {
            finish(request.content)
            return
        }
        let original = request.content
        work = Task {
            for box in PushCredentials.all() {
                guard let event = try? await Self.event(seq, from: box) else { continue }
                guard let content = original.mutableCopy() as? UNMutableNotificationContent else { break }
                content.title = event.title
                content.body = event.body
                content.subtitle = box.name
                if let approval = event.data?.approval {
                    // The app's ApprovalActions answer these (same keys and categories).
                    content.categoryIdentifier = approval.canAlways == true ? "TT_APPROVAL_ALWAYS" : "TT_APPROVAL"
                    content.userInfo["tt.approval"] = approval.id
                    content.userInfo["tt.box"] = box.id
                }
                finish(content)
                return
            }
            finish(original)
        }
    }

    override func serviceExtensionTimeWillExpire() {
        work?.cancel()
        if let generic = lock.withLock({ generic }) { finish(generic) }
    }

    /// Hand iOS its content once.
    private func finish(_ content: UNNotificationContent) {
        let done = lock.withLock { () -> ((UNNotificationContent) -> Void)? in
            defer { handler = nil }
            return handler
        }
        done?(content)
    }

    // MARK: the box

    struct Event: Decodable {
        struct Approval: Decodable {
            let id: String
            let canAlways: Bool?
            enum CodingKeys: String, CodingKey { case id; case canAlways = "can_always" }
        }
        struct Details: Decodable { let approval: Approval? }
        let seq: Int
        let title: String
        let body: String
        let data: Details?
    }

    private static func event(_ seq: Int, from box: PushCredential) async throws -> Event? {
        let host = box.host.contains(":") && !box.host.hasPrefix("[") ? "[\(box.host.replacingOccurrences(of: "%", with: "%25"))]" : box.host
        guard var url = URLComponents(string: "https://\(host):\(box.tlsPort)/events") else { return nil }
        url.queryItems = [URLQueryItem(name: "since", value: String(seq - 1))]
        guard let target = url.url else { return nil }
        var request = URLRequest(url: target, timeoutInterval: 8)
        request.setValue("Bearer \(box.token)", forHTTPHeaderField: "Authorization")
        let session = URLSession(configuration: .ephemeral, delegate: Pin(sha256: box.certSHA256), delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }
        let (data, response) = try await session.data(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { return nil }
        struct Feed: Decodable { let events: [Event] }
        return try JSONDecoder().decode(Feed.self, from: data).events.first { $0.seq == seq }
    }

    /// Accepts exactly the box's certificate, pinned at pairing (as the app does).
    private final class Pin: NSObject, URLSessionDelegate {
        let sha256: String
        init(sha256: String) { self.sha256 = sha256.lowercased() }

        func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge)
            async -> (URLSession.AuthChallengeDisposition, URLCredential?) {
            guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
                  let trust = challenge.protectionSpace.serverTrust,
                  let leaf = (SecTrustCopyCertificateChain(trust) as? [SecCertificate])?.first else {
                return (.cancelAuthenticationChallenge, nil)
            }
            let digest = SHA256.hash(data: SecCertificateCopyData(leaf) as Data).map { String(format: "%02x", $0) }.joined()
            return digest == sha256 ? (.useCredential, URLCredential(trust: trust)) : (.cancelAuthenticationChallenge, nil)
        }
    }
}

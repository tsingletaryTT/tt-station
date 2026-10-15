import Foundation
import UIKit

enum AppTab: Hashable {
    case assistant, create, box, models, settings
}

/// App-wide state: paired boxes, LAN discovery, and the one *active* box whose
/// session and live telemetry every tab shows.
@MainActor
@Observable
final class AppModel {
    let store = BoxStore()
    let discovery = Discovery()
    let telemetry = TelemetryFeed()
    /// Images and videos the boxes made, kept on this phone.
    let creations = CreationStore()
    /// Saved chats with every box.
    let conversations = ConversationStore()
    let alerts = AlertCenter()
    var selectedTab: AppTab = .assistant
    /// Why the last pairing link didn't work, for an alert.
    var linkError: String?
    /// A recording opened in TT Station from Files, Voice Memos or the share
    /// sheet: Transcribe picks it up.
    var incomingRecording: URL?
    private(set) var session: BoxSession?
    /// The active box was paired before TT Station used HTTPS (no pinned
    /// certificate): it has no session until it's paired again by QR.
    private(set) var insecureBox: SavedBox?
    /// The last "send/remove this iPhone's SSH key" outcome, per box id.
    var sshResults: [String: SSHResult] = [:]

    struct SSHResult: Equatable {
        let ok: Bool
        let message: String
    }

    private static let activeKey = "tt.mobile.activeBox"

    init() {
        let saved = UserDefaults.standard.string(forKey: Self.activeKey)
        if let box = store.boxes.first(where: { $0.id == saved }) ?? store.boxes.first {
            activate(box)
        }
    }

    var activeBox: SavedBox? { session?.box ?? insecureBox }

    /// Make `box` the active one: tear down the old session and telemetry, start new ones.
    func activate(_ box: SavedBox, force: Bool = false) {
        if !force, session?.box.id == box.id { return }
        session?.stopPolling()
        telemetry.stop()
        insecureBox = nil
        guard box.pin != nil else {
            // No plain-HTTP fallback: this box has to pair again by QR.
            session = nil
            insecureBox = box
            UserDefaults.standard.set(box.id, forKey: Self.activeKey)
            return
        }
        guard let token = store.token(for: box),
              let next = try? BoxSession(box: box, token: token, conversations: conversations),
              let telemetryClient = try? AgentClient(box: box, token: token) else {
            session = nil
            return
        }
        session = next
        next.onLongOperation = { [weak self, weak next] in
            guard let self, let next else { return }
            Task { await self.alerts.offer(for: next) }
        }
        next.startPolling()
        Task { await alerts.registerIfAllowed(with: next) }
        telemetry.start(client: telemetryClient)
        UserDefaults.standard.set(box.id, forKey: Self.activeKey)
    }

    /// A pairing just succeeded: remember the box and switch to it.
    func paired(_ box: SavedBox, token: String) {
        store.save(box, token: token)
        activate(box, force: true)
        selectedTab = .assistant
    }

    /// Handle a `ttstation://pair?...` link (the QR TT Station shows on the box in
    /// Settings → Pairing and security, opened by the Camera app): pair with no typing.
    /// The link carries the box's HTTPS port (`tls`) and certificate fingerprint
    /// (`fp`); pairing happens over HTTPS pinned to that fingerprint, so a link
    /// without them is refused.
    func handle(url: URL) async {
        if url.isFileURL {
            // iOS copied it into our Inbox; move it to Documents/Recordings so
            // it survives until the box has it.
            let folder = URL.documentsDirectory.appendingPathComponent("Recordings", isDirectory: true)
            try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            let dest = folder.appendingPathComponent(url.lastPathComponent)
            try? FileManager.default.removeItem(at: dest)
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            incomingRecording = (try? FileManager.default.moveItem(at: url, to: dest)).map { dest }
                ?? ((try? FileManager.default.copyItem(at: url, to: dest)).map { dest })
                ?? url
            selectedTab = .create
            return
        }
        guard url.scheme == "ttstation", url.host == "pair",
              let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems else { return }
        func value(_ key: String) -> String? { items.first { $0.name == key }?.value.flatMap { $0.isEmpty ? nil : $0 } }
        guard let host = value("host"), let pairID = value("pair"), let code = value("code") else {
            linkError = "That pairing code is incomplete. Show it again in TT Station on the box (Settings → Pairing and security)."
            return
        }
        guard let tls = value("tls").flatMap(Int.init), (1...65535).contains(tls),
              let fp = value("fp")?.lowercased(), Self.isSHA256Hex(fp) else {
            linkError = "That pairing code doesn't carry the box's security certificate, so TT Station can't connect securely. Update TT Station on the box, then scan the code in Settings → Pairing and security."
            return
        }
        let port = value("port").flatMap(Int.init) ?? 8765
        let pin = TLSPin(port: tls, sha256: fp)
        var box = SavedBox(name: value("name") ?? host, host: host, port: port)
        box.tlsPort = pin.port
        box.certSHA256 = pin.sha256
        box.sshUser = value("ssh")
        do {
            // The QR came from the box's own screen, so its fingerprint is the one
            // to trust: this client only accepts a certificate that matches it.
            let client = try AgentClient(host: host, pin: pin)
            // iOS 16+ reports a generic "iPhone" here unless the app has Apple's
            // device-name entitlement: still honest, and the list shows when it paired.
            let result = try await client.pairComplete(pairID: pairID, code: code,
                                                       name: UIDevice.current.name, platform: "iOS")
            paired(box, token: result.token)
            await sendSSHKey(to: box)
        } catch AgentError.http(let status, _) where status == 401 || status == 404 || status == 410 {
            linkError = "That code has expired. Show a new one in TT Station on the box (Settings → Pairing and security)."
        } catch {
            linkError = error.localizedDescription
        }
    }

    /// 64 lowercase hex digits: a SHA-256 fingerprint.
    private static func isSHA256Hex(_ text: String) -> Bool {
        text.count == 64 && text.allSatisfy { $0.isHexDigit && !$0.isUppercase }
    }

    /// Put this iPhone's SSH public key on `box` (`POST /ssh/authorize`). Never
    /// fatal: the outcome lands in `sshResults` for the SSH key page to show.
    func sendSSHKey(to box: SavedBox) async {
        do {
            guard let token = store.token(for: box) else { throw AgentError.notPaired }
            let client = try AgentClient(box: box, token: token)
            let key = try SSHKey.load()
            let result = try await client.authorizeSSH(publicKey: key.publicKeyLine, label: SSHKey.label)
            sshResults[box.id] = SSHResult(ok: true, message: result.alreadyPresent
                ? "\(box.name) already had this iPhone's SSH key."
                : "Added this iPhone's SSH key to \(box.name).")
            if var saved = store.boxes.first(where: { $0.id == box.id }), saved.sshUser != result.sshUser {
                saved.sshUser = result.sshUser
                store.update(saved)
            }
        } catch {
            sshResults[box.id] = SSHResult(ok: false, message: "Couldn't send this iPhone's SSH key to \(box.name): \(error.localizedDescription)")
        }
    }

    /// Where to go once a model of `kind` is starting/serving.
    static func tab(for kind: String) -> AppTab {
        switch kind {
        case "image", "video": return .create
        case "chat": return .assistant
        default: return .box
        }
    }

    func forget(_ box: SavedBox) {
        store.forget(box)
        if insecureBox?.id == box.id {
            insecureBox = nil
            if let next = store.boxes.first { activate(next) }
            return
        }
        guard session?.box.id == box.id else { return }
        session?.stopPolling()
        telemetry.stop()
        session = nil
        if let next = store.boxes.first {
            activate(next)
        }
    }
}

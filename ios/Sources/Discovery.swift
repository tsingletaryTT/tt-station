// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation
import Network

/// A box seen on the LAN via the agent's `_tenstorrent._tcp` mDNS advertisement.
struct DiscoveredBox: Identifiable, Hashable {
    let name: String
    let host: String
    let port: Int
    let status: String?
    let chips: String?
    /// The box's HTTPS port (TXT `tls`). Only informational: connecting needs
    /// the certificate fingerprint, which comes from scanning the pairing QR.
    let tlsPort: Int?

    var id: String { "\(host):\(port)" }
}

/// Browses `_tenstorrent._tcp` and resolves each result to a host + control port.
///
/// `NWBrowser` hands back service endpoints, not addresses; `URLSession` needs an
/// address, so each result is resolved by opening a throwaway `NWConnection` and
/// reading the remote endpoint once it's ready. IPv4 is tried first; if it can't connect
/// within `resolveTimeout` the resolve falls back to any address family, and a
/// link-local IPv6 result is replaced by the box's "<name>.local" mDNS name (see
/// `usableHost`). The TXT record's
/// `ctrl` key wins over the resolved port, matching the CLI's discovery.
@Observable
final class Discovery {
    private(set) var boxes: [DiscoveredBox] = []
    private(set) var isBrowsing = false
    private var browser: NWBrowser?
    private var resolving: [NWConnection] = []

    func start() {
        stop()
        let params = NWParameters.tcp
        params.includePeerToPeer = false
        let browser = NWBrowser(for: .bonjourWithTXTRecord(type: "_tenstorrent._tcp", domain: nil), using: params)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            self?.resolveAll(results)
        }
        browser.start(queue: .main)
        self.browser = browser
        isBrowsing = true
    }

    func stop() {
        browser?.cancel()
        browser = nil
        resolving.forEach { $0.cancel() }
        resolving.removeAll()
        isBrowsing = false
    }

    private func resolveAll(_ results: Set<NWBrowser.Result>) {
        for result in results {
            guard case let .service(serviceName, _, _, _) = result.endpoint else { continue }
            var txt: [String: String] = [:]
            if case let .bonjour(record) = result.metadata {
                txt = record.dictionary
            }
            resolve(result.endpoint, serviceName: serviceName, txt: txt, ipv4Only: true)
        }
    }

    private func resolve(_ endpoint: NWEndpoint, serviceName: String, txt: [String: String], ipv4Only: Bool) {
        let params = NWParameters.tcp
        if ipv4Only, let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let connection = NWConnection(to: endpoint, using: params)
        resolving.append(connection)
        var finished = false
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let self, let connection else { return }
            if case .cancelled = state {
                self.resolving.removeAll { $0 === connection }
                return
            }
            guard !finished else { return }
            switch state {
            case .ready:
                finished = true
                if case let .hostPort(host, port) = connection.currentPath?.remoteEndpoint {
                    let ctrl = txt["ctrl"].flatMap(Int.init) ?? Int(port.rawValue)
                    let name = txt["name"] ?? serviceName
                    let box = DiscoveredBox(name: name, host: Self.usableHost(host, boxName: name),
                                            port: ctrl, status: txt["status"], chips: txt["chips"],
                                            tlsPort: txt["tls"].flatMap(Int.init))
                    self.boxes.removeAll { $0.name == box.name || $0.id == box.id }
                    self.boxes.append(box)
                }
                connection.cancel()
            case .failed:
                finished = true
                connection.cancel()
                if ipv4Only {
                    self.resolve(endpoint, serviceName: serviceName, txt: txt, ipv4Only: false)
                }
            default:
                // `.waiting` is NOT a failure: iOS parks connections there until the user
                // answers the Local Network prompt. The timeout below handles a real
                // dead end.
                break
            }
        }
        connection.start(queue: .main)
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.resolveTimeout) { [weak self] in
            guard !finished else { return }
            finished = true
            connection.cancel()
            if ipv4Only {
                self?.resolve(endpoint, serviceName: serviceName, txt: txt, ipv4Only: false)
            }
        }
    }

    /// How long one resolve attempt may sit in `.preparing`/`.waiting`. Generous, because
    /// the first attempt usually waits on the user answering the Local Network prompt.
    private static let resolveTimeout: TimeInterval = 8

    /// The host string to save for a resolved box. An IPv6 link-local address
    /// ("fe80::…%en0") is only valid on one interface and doesn't survive being put in a
    /// URL, so in that case use the box's mDNS name instead: the agent advertises its
    /// addresses under "<name>.local", which iOS resolves on its own.
    private static func usableHost(_ host: NWEndpoint.Host, boxName: String) -> String {
        let text: String
        switch host {
        case let .name(name, _):
            text = name
        case let .ipv4(address):
            text = "\(address)"
        case let .ipv6(address):
            text = "\(address)"
        @unknown default:
            return "\(boxName).local"
        }
        return usableHost(text, boxName: boxName)
    }

    /// Any address scoped to one interface ("…%en0", link-local fe80::) can't be
    /// saved or put in a URL, however iOS handed it back: use "<name>.local".
    static func usableHost(_ text: String, boxName: String) -> String {
        let lower = text.lowercased()
        if text.contains("%") || lower.hasPrefix("fe80") || lower.hasPrefix("[fe80") {
            return "\(boxName).local"
        }
        return text
    }
}

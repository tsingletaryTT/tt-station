// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation

// The decisions behind the phone's box controls, kept apart from the views so
// they can be tested: is this app too old (or the box), can the box be woken
// from here and how, and the small bits of shaping the lists need.
// Foundation only.

/// Whether this app and the box speak the same API (`/status` → `api_version`,
/// `min_client`; see the agent's `health.rs`).
enum ClientCompat: Equatable {
    case ok
    /// The box no longer serves an app this old.
    case updateApp(String)
    /// The box is too old for this app.
    case updateBox(String)

    /// The API generation this app was built for.
    static let apiVersion = 1
    /// The oldest box API generation this app works with.
    static let minBoxAPI = 1

    static func check(_ status: BoxStatus) -> ClientCompat {
        // Boxes from before versions were reported speak generation 1.
        let boxAPI = status.apiVersion ?? 1
        let minClient = status.minClient ?? 1
        if apiVersion < minClient {
            return .updateApp("\(status.name) needs a newer TT Station app. Update it from the App Store (or TestFlight) to keep controlling the box.")
        }
        if boxAPI < minBoxAPI {
            let version = status.agentVersion.map { " (it runs \($0))" } ?? ""
            return .updateBox("\(status.name) runs an older TT Station\(version) than this app needs. Update the box from TT Station on your computer.")
        }
        return .ok
    }

    var message: String? {
        switch self {
        case .ok: return nil
        case .updateApp(let m), .updateBox(let m): return m
        }
    }
}

/// A Wake-on-LAN magic packet: six `FF` bytes, then the MAC sixteen times.
enum MagicPacket {
    /// `aa:bb:cc:dd:ee:ff` (or `-`-separated) → the six bytes; `nil` otherwise.
    static func macBytes(_ mac: String) -> [UInt8]? {
        let parts = mac.split(whereSeparator: { $0 == ":" || $0 == "-" })
        guard parts.count == 6 else { return nil }
        let bytes = parts.compactMap { $0.count == 2 ? UInt8($0, radix: 16) : nil }
        guard bytes.count == 6, bytes != [0, 0, 0, 0, 0, 0] else { return nil }
        return bytes
    }

    static func make(mac: String) -> Data? {
        guard let bytes = macBytes(mac) else { return nil }
        var data = Data(repeating: 0xFF, count: 6)
        for _ in 0..<16 { data.append(contentsOf: bytes) }
        return data
    }
}

/// What the phone remembers about waking a box, refreshed from
/// `GET /power/info` while the box is on (it can't be asked while it's asleep).
struct WakeTarget: Codable, Equatable {
    /// The wired port's MAC.
    var mac: String
    /// The box's LAN broadcast addresses.
    var broadcast: [String] = []
    /// Everything was in place for waking at the last look.
    var possible: Bool = false
    /// Why waking wouldn't work, at the last look.
    var whyNot: String? = nil

    init(mac: String, broadcast: [String] = [], possible: Bool = false, whyNot: String? = nil) {
        self.mac = mac
        self.broadcast = broadcast
        self.possible = possible
        self.whyNot = whyNot
    }

    /// From the box's answer: its wired port, or `nil` when it has none.
    init?(_ info: PowerInfo) {
        if let wol = info.wakeOnLan {
            guard let mac = wol.mac, MagicPacket.macBytes(mac) != nil else { return nil }
            self.init(mac: mac, broadcast: wol.broadcast, possible: wol.possible, whyNot: wol.whyNot)
        } else {
            // An agent from before `wake_on_lan`: the wired port with a cable in.
            guard let nic = info.wired.first(where: \.connected) ?? info.wired.first else { return nil }
            self.init(mac: nic.mac, possible: info.canWake,
                      whyNot: info.canWake ? nil : "Wake-on-LAN isn't set up on \(nic.name).")
        }
    }
}

/// Deciding whether (and where) to send a magic packet when the box doesn't answer.
enum WakePlan: Equatable {
    /// Send to these IPv4 addresses (UDP port 9).
    case send([String])
    /// Waking can't work from here; say why, and what to do instead.
    case cannot(String)

    /// The IPv4 broadcast address of `ip` with `netmask` (`10.0.0.122`, `255.255.255.0` → `10.0.0.255`).
    static func broadcast(ip: String, netmask: String) -> String? {
        guard let a = octets(ip), let m = octets(netmask) else { return nil }
        return zip(a, m).map { String($0 | ~$1) }.joined(separator: ".")
    }

    static func octets(_ s: String) -> [UInt8]? {
        let parts = s.split(separator: ".")
        guard parts.count == 4 else { return nil }
        let bytes = parts.compactMap { UInt8($0) }
        return bytes.count == 4 ? bytes : nil
    }

    /// What a Wake tap should do for a box at `host`, given what the phone
    /// remembers and the network the phone is on now (`phoneBroadcast`: its
    /// Wi-Fi's broadcast address, `nil` when it isn't on Wi-Fi).
    static func plan(target: WakeTarget?, host: String, phoneBroadcast: String?) -> WakePlan {
        let instead = "To keep the box reachable next time, put just the model to sleep instead of the whole box: it wakes again when you ask for it."
        guard let target else {
            return .cannot("TT Station hasn't learned how to wake this box yet: open Power once while the box is on. \(instead)")
        }
        guard target.possible else {
            let why = target.whyNot ?? "This box isn't set up to be woken over the network."
            return .cannot("\(why) Turn it on with its power button. \(instead)")
        }
        guard let phoneBroadcast else {
            return .cannot("Join the same Wi-Fi network as the box to wake it: the wake signal only travels on the local network.")
        }
        // The box told us its networks; a phone on none of them can't reach it.
        if !target.broadcast.isEmpty && !target.broadcast.contains(phoneBroadcast) {
            return .cannot("This phone isn't on the box's network. Join the same Wi-Fi as the box to wake it.")
        }
        var targets = [phoneBroadcast]
        for b in target.broadcast where !targets.contains(b) { targets.append(b) }
        targets.append("255.255.255.255")
        // The box's last address: delivered while the network still remembers it.
        if octets(host) != nil, !targets.contains(host) { targets.append(host) }
        return .send(targets)
    }
}

/// Folders Code was used in lately, newest first, for starting a new session.
enum RecentFolders {
    static func from(_ sessions: [CodeSession], limit: Int = 6) -> [String] {
        var seen: [String] = []
        for s in sessions.sorted(by: { $0.last > $1.last }) where !seen.contains(s.folder) {
            seen.append(s.folder)
            if seen.count == limit { break }
        }
        return seen
    }

    /// `/home/ttuser/code/app` → `~/code/app`.
    static func short(_ folder: String) -> String {
        folder.replacingOccurrences(of: #"^/home/[^/]+"#, with: "~", options: .regularExpression)
    }
}

/// `GET /code/models` grouped for a picker: one group per model, the one in
/// use first, then the ones that can start, then the rest; setups (profiles)
/// in the box's order inside each.
enum ServeGroups {
    struct Group: Identifiable, Equatable {
        let id: String
        let name: String
        let options: [ServeOption]
        var inUse: Bool { options.contains(where: \.isInUse) }
        var startable: Bool { options.contains(where: \.canStart) }
    }

    static func make(_ options: [ServeOption], query: String = "") -> [Group] {
        let q = query.trimmingCharacters(in: .whitespaces)
        var order: [String] = []
        var by: [String: [ServeOption]] = [:]
        for o in options {
            if !q.isEmpty && !o.name.localizedCaseInsensitiveContains(q) && !o.model.localizedCaseInsensitiveContains(q) {
                continue
            }
            if by[o.group] == nil { order.append(o.group) }
            by[o.group, default: []].append(o)
        }
        let groups = order.map { Group(id: $0, name: by[$0]?.first?.name ?? $0, options: by[$0] ?? []) }
        func rank(_ g: Group) -> Int { g.inUse ? 0 : g.startable ? 1 : 2 }
        return groups.enumerated()
            .sorted { (rank($0.element), $0.offset) < (rank($1.element), $1.offset) }
            .map(\.element)
    }

    /// "Community · 2 chips · Throughput" -- what tells one setup from another.
    static func detail(_ o: ServeOption) -> String {
        var parts: [String] = []
        switch o.source {
        case "community": parts.append("Community")
        case "native": parts.append("Native vLLM")
        case "running": parts.append("Running now")
        default: parts.append("Catalog")
        }
        if let label = o.profileLabel ?? o.profile { parts.append(label) }
        if let chips = o.chips { parts.append("\(chips) chip\(chips == 1 ? "" : "s")") }
        if let context = o.context, context > 0 { parts.append("\(context / 1024)K context") }
        return parts.joined(separator: " · ")
    }
}

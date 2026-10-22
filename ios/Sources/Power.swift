// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Network
import SwiftUI
import UIKit

/// Waking a sleeping or powered-off box: a Wake-on-LAN "magic packet" (6×FF then
/// the wired NIC's MAC 16 times) over UDP to the local network.
///
/// It only works when the box is cabled (Wi-Fi can't wake a computer), set up for
/// it, and the phone is on the same network: `GET /power/info` says which, and the
/// phone remembers the answer (`WakeTarget`) because a sleeping box can't be asked.
/// iOS also reserves broadcast sending for apps Apple has granted the multicast
/// entitlement, so this tries the broadcasts and then the box's last address
/// directly; if nothing leaves the phone, a smart plug is the dependable way.
enum WakeOnLAN {
    private static func key(_ boxID: String) -> String { "tt.wol.target.\(boxID)" }
    /// What older builds stored: the MAC alone.
    private static func legacyKey(_ boxID: String) -> String { "tt.wol.mac.\(boxID)" }

    static func remember(_ target: WakeTarget, for box: SavedBox) {
        if let data = try? JSONEncoder().encode(target) {
            UserDefaults.standard.set(data, forKey: key(box.id))
        }
    }

    static func target(for box: SavedBox) -> WakeTarget? {
        if let data = UserDefaults.standard.data(forKey: key(box.id)),
           let target = try? JSONDecoder().decode(WakeTarget.self, from: data) {
            return target
        }
        // Learned by an older build: the MAC, nothing about whether it can work.
        return UserDefaults.standard.string(forKey: legacyKey(box.id)).map { WakeTarget(mac: $0, possible: true) }
    }

    static func mac(for box: SavedBox) -> String? { target(for: box)?.mac }

    static func packet(mac: String) -> Data? { MagicPacket.make(mac: mac) }

    /// What tapping Wake would do for `box` from where the phone is now.
    static func plan(for box: SavedBox) -> WakePlan {
        WakePlan.plan(target: target(for: box), host: box.host, phoneBroadcast: LocalNetwork.wifiBroadcast())
    }

    /// Send the packet every way that might reach the box; returns how many sends
    /// left the phone. Throws with the plain reason when waking can't work from here.
    static func wake(_ box: SavedBox) async throws -> Int {
        let targets: [String]
        switch plan(for: box) {
        case .cannot(let why): throw WakeError.cannot(why)
        case .send(let list): targets = list
        }
        guard let mac = mac(for: box), let data = packet(mac: mac) else { throw WakeError.unknownMAC }
        var sent = 0
        for target in targets {
            for port: NWEndpoint.Port in [9, 7] {
                if await send(data, to: target, port: port) { sent += 1 }
            }
        }
        if sent == 0 { throw WakeError.blocked }
        return sent
    }

    private static func send(_ data: Data, to host: String, port: NWEndpoint.Port) async -> Bool {
        await withCheckedContinuation { continuation in
            let params = NWParameters.udp
            params.allowLocalEndpointReuse = true
            let connection = NWConnection(host: NWEndpoint.Host(host), port: port, using: params)
            var finished = false
            let finish: (Bool) -> Void = { ok in
                guard !finished else { return }
                finished = true
                connection.cancel()
                continuation.resume(returning: ok)
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    connection.send(content: data, completion: .contentProcessed { error in finish(error == nil) })
                case .failed, .cancelled:
                    finish(false)
                default:
                    break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + 3) { finish(false) }
        }
    }

    enum WakeError: LocalizedError {
        case unknownMAC, blocked, cannot(String)
        var errorDescription: String? {
            switch self {
            case .unknownMAC: "TT Station doesn't know this box's network card yet. Open Power once while the box is on."
            case .blocked: "iOS didn't let the wake signal leave the phone. Use a smart plug to turn the box on (see Power)."
            case .cannot(let why): why
            }
        }
    }
}

/// The network this phone is on.
enum LocalNetwork {
    /// The broadcast address of the phone's Wi-Fi (`en0`), or `nil` off Wi-Fi.
    static func wifiBroadcast() -> String? {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0, let first = list else { return nil }
        defer { freeifaddrs(list) }
        for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let ifa = entry.pointee
            guard String(cString: ifa.ifa_name) == "en0",
                  let addr = ifa.ifa_addr, addr.pointee.sa_family == UInt8(AF_INET),
                  let mask = ifa.ifa_netmask,
                  let ip = numeric(addr), let netmask = numeric(mask) else { continue }
            return WakePlan.broadcast(ip: ip, netmask: netmask)
        }
        return nil
    }

    private static func numeric(_ addr: UnsafeMutablePointer<sockaddr>) -> String? {
        var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        guard getnameinfo(addr, socklen_t(addr.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0
        else { return nil }
        return String(cString: host)
    }
}

/// Box tab → Power: sleep, restart, shut down; wake; and what's needed for each.
/// The machine ops are an owner's (the box refuses members), and each says
/// first what it means for everyone using the box and for waking it again.
struct PowerCard: View {
    let session: BoxSession
    @State private var info: PowerInfo?
    @State private var confirming: String?
    @State private var working: String?
    @State private var message: String?
    @State private var showSetup = false

    private var owner: Bool { session.isOwner }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SectionTitle(text: "The whole box")
            GlassEffectContainer(spacing: 10) {
                HStack(spacing: 10) {
                    button("suspend", title: "Sleep", icon: "moon.zzz.fill", ok: info?.canSuspend)
                    button("reboot", title: "Restart", icon: "arrow.clockwise", ok: info?.canReboot)
                    button("shutdown", title: "Shut down", icon: "power", ok: info?.canPowerOff)
                }
            }
            if session.access != nil && !owner {
                Text("Only owners can sleep, restart or shut down the box. You can put the model to sleep above.")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if session.statusError != nil {
                WakeBoxButton(box: session.box) { message = $0 }
            }
            if let message {
                Text(message).font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if let wol = info?.wakeOnLan {
                WakeOnLANRow(session: session, wol: wol) { info = try? await session.powerInfo() }
            }
            if let info, !info.setup.isEmpty {
                Button { showSetup = true } label: {
                    Label(info.canWake ? "Finish setup for remote power" : "Set up turning it on from your phone",
                          systemImage: "wrench.and.screwdriver")
                        .font(.tt(.footnote))
                }
            }
        }
        .padding(18)
        .card(radius: 26)
        .task(id: session.box.id) { info = try? await session.powerInfo() }
        .confirmationDialog(dialogTitle, isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
                            titleVisibility: .visible, presenting: confirming) { action in
            Button(actionTitle(action), role: .destructive) { Task { await run(action) } }
            // Can't be woken from here: offer what keeps it reachable.
            if action != "reboot", info?.canWake != true, session.isServing {
                Button("Sleep just the model instead") { Task { await sleepModel() } }
            }
        } message: { action in
            Text(dialogMessage(action))
        }
        .sheet(isPresented: $showSetup) {
            if let info { PowerSetupSheet(info: info) }
        }
    }

    private func button(_ action: String, title: String, icon: String, ok: String?) -> some View {
        Button {
            confirming = action
        } label: {
            VStack(spacing: 4) {
                if working == action { ProgressView() } else { Image(systemName: icon) }
                Text(title).font(.tt(.caption))
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glass)
        .controlSize(.large)
        .disabled(working != nil || session.statusError != nil || (ok != nil && ok != "yes")
                  || (session.access != nil && !owner))
    }

    private var dialogTitle: String {
        switch confirming {
        case "suspend": return "Put the whole box to sleep?"
        case "reboot": return "Restart the box?"
        default: return "Shut down the box?"
        }
    }

    private func actionTitle(_ action: String) -> String {
        ["suspend": "Sleep", "reboot": "Restart", "shutdown": "Shut down"][action] ?? action
    }

    private func dialogMessage(_ action: String) -> String {
        let shared = "Everyone using \(session.box.name) loses it: chat, Code, routines and other phones."
        let wake: String
        if info?.canWake == true {
            wake = "Wake it from here (on the same network)."
        } else {
            let why = info?.wakeOnLan?.whyNot.map { " \($0)" } ?? ""
            wake = "This phone can't wake it again:\(why) You'll need its power button or a smart plug."
        }
        switch action {
        case "suspend": return "Stops serving and sleeps, using a few watts. \(shared) \(wake)"
        case "reboot": return "Stops serving and restarts; it's back in a couple of minutes (you'll get an alert). \(shared)"
        default: return "Stops serving and powers off. \(shared) \(wake)"
        }
    }

    private func run(_ action: String) async {
        working = action
        defer { working = nil }
        do {
            try await session.power(action)
            message = action == "reboot" ? "Restarting… you'll get an alert when it's back." : "Done. The box is going down."
        } catch {
            message = error.localizedDescription
        }
    }

    private func sleepModel() async {
        working = "model"
        defer { working = nil }
        do {
            let label = try await session.sleepModel()
            message = "\(label) is asleep. The box stays on and reachable; wake the model above when you need it."
        } catch {
            message = error.localizedDescription
        }
    }
}

/// Wake a box that doesn't answer: sends the magic packet when it can work, and
/// says plainly why not when it can't (Wi-Fi only, not set up, another network).
struct WakeBoxButton: View {
    let box: SavedBox
    let onResult: (String) -> Void
    @State private var sending = false

    var body: some View {
        Button {
            Task {
                sending = true
                defer { sending = false }
                do {
                    let sent = try await WakeOnLAN.wake(box)
                    onResult("Wake signal sent (\(sent)×). If the box was asleep, it'll answer in a minute or two.")
                } catch {
                    onResult(error.localizedDescription)
                }
            }
        } label: {
            HStack {
                if sending { ProgressView() }
                Label("Wake box", systemImage: "sunrise.fill")
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glassProminent)
        .controlSize(.large)
        .disabled(sending)
    }
}

/// Whether this phone can wake the box, and (for owners) turning it on.
private struct WakeOnLANRow: View {
    let session: BoxSession
    let wol: WakeOnLanInfo
    let reload: () async -> Void
    @State private var working = false
    @State private var result: WakeOnLanEnable?
    @State private var problem: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label {
                VStack(alignment: .leading, spacing: 2) {
                    Text(wol.possible ? "Wake from your phone: ready" : "Wake from your phone: not yet")
                        .font(.tt(.subheadline).weight(.semibold))
                    Text(detail).font(.tt(.caption)).foregroundStyle(.secondary)
                }
            } icon: {
                Image(systemName: wol.possible ? "sunrise.fill" : "sunrise").foregroundStyle(wol.possible ? Brand.accent : .secondary)
            }
            if !wol.persistent && wol.supported {
                if session.isOwner {
                    Button {
                        Task { await enable() }
                    } label: {
                        HStack {
                            if working { ProgressView() }
                            Text("Turn on Wake-on-LAN")
                        }
                    }
                    .buttonStyle(.glass)
                    .controlSize(.small)
                    .disabled(working)
                } else if session.access != nil {
                    Text("An owner can turn it on.").font(.tt(.caption)).foregroundStyle(.secondary)
                }
            }
            if let result {
                Text(result.message).font(.tt(.caption))
                if let command = result.command { CopyCommand(command: command) }
            }
            if let problem { Text(problem).font(.tt(.caption)).foregroundStyle(.orange) }
        }
    }

    private var detail: String {
        if wol.possible {
            return "Send the wake signal from the same Wi-Fi as the box. It goes to \(wol.nic ?? "its network port") (\(wol.mac ?? "?"))."
        }
        return wol.whyNot ?? "This box can't be woken over the network."
    }

    private func enable() async {
        working = true
        defer { working = false }
        do {
            result = try await session.enableWakeOnLAN()
            problem = nil
            await reload()
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// A command to run on the box: tap to copy.
struct CopyCommand: View {
    let command: String
    @State private var copied = false

    var body: some View {
        Button {
            UIPasteboard.general.string = command
            copied = true
        } label: {
            HStack(alignment: .top) {
                Text(command).font(.ttMono(12, relativeTo: .footnote)).multilineTextAlignment(.leading)
                Spacer(minLength: 4)
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
            }
            .padding(10)
            .background(.black.opacity(0.25), in: .rect(cornerRadius: 10))
        }
        .buttonStyle(.plain)
    }
}

/// The one-time setup, with commands to copy and run on the box.
private struct PowerSetupSheet: View {
    let info: PowerInfo
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Run these once on the box (in a terminal there). Each makes one more thing work from your phone.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                }
                ForEach(info.setup, id: \.self) { step in
                    Section {
                        Text(step.why)
                        if !step.command.isEmpty {
                            Button {
                                UIPasteboard.general.string = step.command
                            } label: {
                                HStack(alignment: .top) {
                                    Text(step.command).font(.ttMono(13, relativeTo: .footnote)).multilineTextAlignment(.leading)
                                    Spacer()
                                    Image(systemName: "doc.on.doc")
                                }
                            }
                        }
                    }
                }
                Section("Turning it on without a cable") {
                    Text("Put the box on a smart plug (one that works with the Home app) and set its BIOS to power on when power returns (\u{201C}Restore on AC power loss: On\u{201D}). Then: shut down from here, and later switch the plug off and on to start it. With Shortcuts you can make one \u{201C}Start my box\u{201D} button that does it.")
                        .font(.tt(.subheadline))
                }
            }
            .navigationTitle("Remote power setup")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
    }
}

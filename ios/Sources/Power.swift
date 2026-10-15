import Network
import SwiftUI
import UIKit

/// Waking a sleeping or powered-off box: a Wake-on-LAN "magic packet" (6×FF then
/// the wired NIC's MAC 16 times) over UDP to the local network.
///
/// It only works when the box is cabled (Wi-Fi can't wake a computer) and set up
/// for it (`GET /power/info` says what's missing). iOS also reserves broadcast
/// sending for apps Apple has granted the multicast entitlement, so this tries
/// broadcast and then the box's last address directly; if neither leaves the
/// phone, a smart plug (below) is the dependable way to turn it on.
enum WakeOnLAN {
    private static func key(_ boxID: String) -> String { "tt.wol.mac.\(boxID)" }

    static func remember(mac: String, for box: SavedBox) {
        UserDefaults.standard.set(mac, forKey: key(box.id))
    }

    static func mac(for box: SavedBox) -> String? {
        UserDefaults.standard.string(forKey: key(box.id))
    }

    static func packet(mac: String) -> Data? {
        let bytes = mac.split(whereSeparator: { $0 == ":" || $0 == "-" }).compactMap { UInt8($0, radix: 16) }
        guard bytes.count == 6 else { return nil }
        var data = Data(repeating: 0xFF, count: 6)
        for _ in 0..<16 { data.append(contentsOf: bytes) }
        return data
    }

    /// Send the packet every way that might reach the box; returns how many sends
    /// left the phone.
    static func wake(_ box: SavedBox) async throws -> Int {
        guard let mac = mac(for: box), let data = packet(mac: mac) else { throw WakeError.unknownMAC }
        var targets = ["255.255.255.255"]
        if let ip = IPv4Address(box.host) {
            let parts = ip.rawValue.map { $0 }
            targets.append("\(parts[0]).\(parts[1]).\(parts[2]).255")
            targets.append(box.host)
        }
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
        case unknownMAC, blocked
        var errorDescription: String? {
            switch self {
            case .unknownMAC: "TT Station doesn't know this box's network card yet. Open the Power card once while the box is on."
            case .blocked: "iOS didn't let the wake signal leave the phone. Use a smart plug to turn the box on (see Power)."
            }
        }
    }
}

/// Box tab → Power: sleep, restart, shut down; wake; and what's needed for each.
struct PowerCard: View {
    let session: BoxSession
    @State private var info: PowerInfo?
    @State private var confirming: String?
    @State private var working: String?
    @State private var message: String?
    @State private var showSetup = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SectionTitle(text: "Power")
            GlassEffectContainer(spacing: 10) {
                HStack(spacing: 10) {
                    button("suspend", title: "Sleep", icon: "moon.zzz.fill", ok: info?.canSuspend)
                    button("reboot", title: "Restart", icon: "arrow.clockwise", ok: info?.canReboot)
                    button("shutdown", title: "Shut down", icon: "power", ok: info?.canPowerOff)
                }
            }
            if session.statusError != nil {
                Button {
                    Task { await wake() }
                } label: {
                    Label("Wake box", systemImage: "sunrise.fill").frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.large)
                .disabled(WakeOnLAN.mac(for: session.box) == nil)
            }
            if let message {
                Text(message).font(.tt(.footnote)).foregroundStyle(.secondary)
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
        .disabled(working != nil || session.statusError != nil || (ok != nil && ok != "yes"))
    }

    private var dialogTitle: String {
        switch confirming {
        case "suspend": return "Put the box to sleep?"
        case "reboot": return "Restart the box?"
        default: return "Shut down the box?"
        }
    }

    private func actionTitle(_ action: String) -> String {
        ["suspend": "Sleep", "reboot": "Restart", "shutdown": "Shut down"][action] ?? action
    }

    private func dialogMessage(_ action: String) -> String {
        let wake = info?.canWake == true
            ? "Wake it from here."
            : "To turn it on again you'll need the power button or a smart plug, until Wake-on-LAN is set up."
        switch action {
        case "suspend": return "Stops serving and sleeps, using a few watts. \(wake)"
        case "reboot": return "Stops serving and restarts; it's back in a couple of minutes (you'll get an alert)."
        default: return "Stops serving and powers off. \(wake)"
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

    private func wake() async {
        do {
            let sent = try await WakeOnLAN.wake(session.box)
            message = "Wake signal sent (\(sent)×). If the box is set up for it, it'll be back in a minute or two."
        } catch {
            message = error.localizedDescription
        }
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

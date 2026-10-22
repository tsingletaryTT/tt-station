// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// Settings → Devices and security: who is paired with the box and as what,
/// for owners promoting, demoting and removing them; the activity log; and who
/// may start and stop models. A member sees itself and can sign out.
struct DevicesView: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @State private var list: DeviceList?
    @State private var problem: String?
    @State private var removing: PairedDevice?
    @State private var working: String?

    var body: some View {
        List {
            if let list {
                Section {
                    ForEach(list.devices.filter(\.thisDevice)) { device in
                        DeviceRow(device: device)
                    }
                } header: {
                    Text("This phone")
                } footer: {
                    Text(list.owner
                         ? "You're an owner: you manage devices, keys, power and updates."
                         : "You're a member: you can use the box, but an owner manages devices, keys, power and updates.")
                }
                if list.owner {
                    Section("Other devices") {
                        let others = list.devices.filter { !$0.thisDevice }
                        if others.isEmpty { Text("No other devices are paired.").foregroundStyle(.secondary) }
                        ForEach(others) { device in
                            DeviceRow(device: device, busy: working == device.id)
                                .contextMenu { actions(for: device) }
                                .swipeActions { actions(for: device) }
                        }
                    }
                    Section {
                        ModelControlPicker(session: session)
                    } header: {
                        Text("Starting and stopping models")
                    } footer: {
                        Text("Starting, stopping or switching the model affects everyone using the box. With \u{201C}Owners only\u{201D}, members can still chat and use whatever is serving.")
                    }
                    Section {
                        NavigationLink { ActivityLogView(session: session) } label: {
                            Label("Activity log", systemImage: "list.bullet.rectangle")
                        }
                    } footer: {
                        Text("Who paired, changed roles, powered the box down, approved commands and more.")
                    }
                }
                Section {
                    Button("Sign this phone out of \(session.box.name)", role: .destructive) {
                        removing = list.devices.first(where: \.thisDevice)
                    }
                }
            } else if problem == nil {
                ProgressView()
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.orange) }
            }
        }
        .navigationTitle("Devices and security")
        .refreshable { await load() }
        .task { await load() }
        .confirmationDialog(removing?.thisDevice == true ? "Sign this phone out?" : "Remove \(removing?.name ?? "")?",
                            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
                            titleVisibility: .visible, presenting: removing) { device in
            Button(device.thisDevice ? "Sign out" : "Remove", role: .destructive) { Task { await remove(device) } }
        } message: { device in
            Text(device.thisDevice
                 ? "This phone stops working with \(session.box.name) until it pairs again."
                 : "\(device.name) stops working with \(session.box.name) until it pairs again.")
        }
    }

    @ViewBuilder
    private func actions(for device: PairedDevice) -> some View {
        Button(device.owner ? "Make a member" : "Make an owner", systemImage: device.owner ? "person" : "crown") {
            Task { await setOwner(device, !device.owner) }
        }
        Button("Remove", systemImage: "trash", role: .destructive) { removing = device }
    }

    private func load() async {
        do {
            list = try await session.devices()
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }

    private func setOwner(_ device: PairedDevice, _ owner: Bool) async {
        working = device.id
        defer { working = nil }
        do {
            try await session.setOwner(device: device.id, owner: owner)
            await load()
        } catch {
            problem = error.localizedDescription
        }
    }

    private func remove(_ device: PairedDevice) async {
        working = device.id
        defer { working = nil }
        do {
            try await session.removeDevice(device.id)
            if device.thisDevice {
                app.forget(session.box)
            } else {
                await load()
            }
        } catch {
            problem = error.localizedDescription
        }
    }
}

private struct DeviceRow: View {
    let device: PairedDevice
    var busy = false

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: icon).foregroundStyle(Brand.accent).frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(device.name).font(.tt(.body).weight(.semibold)).lineLimit(1)
                Text(detail).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 0)
            if busy { ProgressView().controlSize(.small) }
            Text(device.owner ? "Owner" : "Member")
                .font(.ttMono(10, .medium))
                .foregroundStyle(device.owner ? Brand.accent : .secondary)
        }
    }

    private var icon: String {
        switch device.platform?.lowercased() {
        case "ios": return "iphone"
        case "macos", "mac": return "laptopcomputer"
        case "linux": return "desktopcomputer"
        default: return "network"
        }
    }

    private var detail: String {
        var parts: [String] = []
        if let platform = device.platform { parts.append(platform) }
        parts.append("used " + Date(timeIntervalSince1970: device.lastUsed).formatted(.relative(presentation: .named)))
        return parts.joined(separator: " · ")
    }
}

/// Who may start, stop and switch the served model: anyone paired, or owners only.
private struct ModelControlPicker: View {
    let session: BoxSession
    @State private var problem: String?

    var body: some View {
        Picker("Who can", selection: Binding(
            get: { session.access?.modelControl ?? "anyone" },
            set: { rule in Task { await set(rule) } })) {
            Text("Anyone paired").tag("anyone")
            Text("Owners only").tag("owners")
        }
        if let problem { Text(problem).font(.tt(.caption)).foregroundStyle(.orange) }
    }

    private func set(_ rule: String) async {
        do { try await session.setModelControl(rule); problem = nil } catch { problem = error.localizedDescription }
    }
}

/// The box's activity log (owners), newest first.
struct ActivityLogView: View {
    let session: BoxSession
    @State private var entries: [AuditEntry] = []
    @State private var more = false
    @State private var problem: String?

    var body: some View {
        List {
            ForEach(entries, id: \.self) { entry in
                VStack(alignment: .leading, spacing: 3) {
                    HStack {
                        Text(Self.words(entry.action)).font(.tt(.subheadline).weight(.semibold))
                        Spacer(minLength: 4)
                        Text(entry.result).font(.ttMono(10, .medium))
                            .foregroundStyle(entry.result == "ok" ? Brand.accent : entry.result == "denied" || entry.result == "failed" ? .orange : .secondary)
                    }
                    if !entry.target.isEmpty {
                        Text(entry.target).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
                    }
                    Text("\(entry.deviceName) · \(Date(timeIntervalSince1970: entry.ts).formatted(date: .abbreviated, time: .shortened))")
                        .font(.tt(.caption2)).foregroundStyle(.tertiary)
                }
            }
            if more {
                Button("Older") { Task { await load(from: entries.count) } }
            }
            if let problem { Text(problem).foregroundStyle(.orange) }
        }
        .navigationTitle("Activity log")
        .refreshable { await load(from: 0) }
        .task { await load(from: 0) }
    }

    private func load(from offset: Int) async {
        do {
            let page = try await session.audit(offset: offset)
            entries = offset == 0 ? page.entries : entries + page.entries
            more = page.more
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }

    /// `power` → "Power", `model_sleep` → "Model sleep".
    static func words(_ action: String) -> String {
        let spaced = action.replacingOccurrences(of: "_", with: " ")
        return spaced.prefix(1).uppercased() + spaced.dropFirst()
    }
}

/// Settings → Updates: this box's TT Station version, and installing a newer
/// signed release (owners).
struct UpdatesView: View {
    let session: BoxSession
    @State private var info: UpdatesInfo?
    @State private var problem: String?
    @State private var confirming = false
    @State private var installing = false

    var body: some View {
        List {
            if let info {
                Section {
                    LabeledContent("On the box", value: info.version)
                    if let update = info.update {
                        LabeledContent("Available", value: update.version)
                    } else if info.problem == nil {
                        Text("It's up to date.").foregroundStyle(.secondary)
                    }
                    if let working = info.working {
                        HStack { ProgressView(); Text(working) }
                    }
                }
                if let update = info.update {
                    Section(update.name) {
                        Text(update.notes).font(.tt(.footnote)).lineLimit(12)
                        if session.isOwner {
                            Button {
                                confirming = true
                            } label: {
                                HStack {
                                    if installing { ProgressView() }
                                    Text("Install \(update.version)")
                                }
                            }
                            .disabled(installing || info.working != nil)
                        } else {
                            Text("An owner can install it.").foregroundStyle(.secondary)
                        }
                    }
                }
                if let message = info.problem {
                    Section { Text(message).font(.tt(.footnote)).foregroundStyle(.secondary) }
                }
            } else if problem == nil {
                ProgressView()
            }
            if let problem { Section { Text(problem).foregroundStyle(.orange) } }
        }
        .navigationTitle("Updates")
        .refreshable { await load() }
        .task {
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(info?.working == nil ? 120 : 3))
            }
        }
        .confirmationDialog("Install \(info?.update?.version ?? "")?", isPresented: $confirming, titleVisibility: .visible) {
            Button("Install") { Task { await install() } }
        } message: {
            Text("The box checks the release's signature, installs it and restarts TT Station on the box: everyone using it is disconnected for a minute. If the new version doesn't come up, the box goes back to this one by itself.")
        }
    }

    private func load() async {
        do { info = try await session.updates(); problem = nil } catch { problem = error.localizedDescription }
    }

    private func install() async {
        guard let version = info?.update?.version else { return }
        installing = true
        defer { installing = false }
        do {
            try await session.installUpdate(version: version)
            await load()
        } catch {
            problem = error.localizedDescription
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Charts
import SwiftUI

// MARK: - You and your household

/// Who this phone is on the box. The assistant greets each person by name and
/// keeps their memories to themselves; the household's memories are shared.
struct HouseholdView: View {
    let session: BoxSession
    @State private var list: PeopleList?
    @State private var newName = ""
    @State private var renaming: Person?
    @State private var renameText = ""
    @State private var error: String?

    var body: some View {
        List {
            Section {
                Text("Give each person in your home their own memories with the assistant. No accounts: a person is just a name on your box, and each phone says who it belongs to.")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if let error { Section { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(Vesper.warning) } }
            if let list {
                Section("This phone is") {
                    Button { Task { await set(nil) } } label: {
                        choice("Anyone (shared)", selected: list.me == nil, detail: "Sees only the household's memories")
                    }
                    .buttonStyle(.plain)
                    ForEach(list.people) { person in
                        Button { Task { await set(person.id) } } label: {
                            choice(person.name, selected: list.me == person.id, detail: nil)
                        }
                        .buttonStyle(.plain)
                        .swipeActions {
                            Button("Remove", role: .destructive) { Task { await remove(person) } }
                            Button("Rename") { renaming = person; renameText = person.name }
                        }
                    }
                }
                Section {
                    HStack {
                        TextField("Add someone", text: $newName)
                            .submitLabel(.done)
                            .onSubmit { Task { await add(me: list.me == nil) } }
                        Button("Add") { Task { await add(me: list.me == nil) } }
                            .disabled(newName.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                } footer: {
                    Text(list.me == nil ? "The first person you add becomes this phone's." : "Swipe a name to rename or remove it. Removing someone forgets their memories.")
                }
            } else {
                Section { ProgressView() }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Brand.backdrop)
        .navigationTitle("You and your household")
        .navigationBarTitleDisplayMode(.inline)
        .task { await load() }
        .alert("Rename", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
            TextField("Name", text: $renameText)
            Button("Save") {
                if let person = renaming { Task { await rename(person) } }
            }
            Button("Cancel", role: .cancel) {}
        }
    }

    private func choice(_ title: String, selected: Bool, detail: String?) -> some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                if let detail { Text(detail).font(.tt(.caption)).foregroundStyle(.secondary) }
            }
            Spacer()
            if selected { Image(systemName: "checkmark").foregroundStyle(Brand.accent) }
        }
        .contentShape(Rectangle())
    }

    private func run(_ op: () async throws -> PeopleList) async {
        do { list = try await op(); error = nil } catch { self.error = error.localizedDescription }
    }
    private func load() async { await run { try await session.people() } }
    private func set(_ id: String?) async { await run { try await session.setMe(id) } }
    private func add(me: Bool) async {
        let name = newName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        newName = ""
        await run { try await session.addPerson(name: name, me: me) }
    }
    private func rename(_ person: Person) async { await run { try await session.renamePerson(person.id, to: renameText) } }
    private func remove(_ person: Person) async { await run { try await session.removePerson(person.id) } }
}

// MARK: - Schedules

/// The box switches modes on its own: images by day, chat in the evening.
struct SchedulesView: View {
    let session: BoxSession
    @State private var schedules: [ModeSchedule] = []
    @State private var modes: [BoxMode] = []
    @State private var adding = false
    @State private var error: String?

    static let dayLetters = ["S", "M", "T", "W", "T", "F", "S"]

    var body: some View {
        List {
            Section {
                Text("Switch the box to a mode at set times. A schedule never interrupts a transcription: it skips that day and tells you.")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if let error { Section { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(Vesper.warning) } }
            Section {
                if schedules.isEmpty {
                    Text("No schedules yet.").foregroundStyle(.secondary)
                }
                ForEach(schedules) { s in
                    HStack(spacing: 14) {
                        Text(s.time).font(.ttMono(20, .medium, relativeTo: .title3))
                        VStack(alignment: .leading, spacing: 4) {
                            Text(title(of: s.mode)).font(.tt(.headline))
                            HStack(spacing: 3) {
                                ForEach(0..<7, id: \.self) { d in
                                    Text(Self.dayLetters[d])
                                        .font(.ttMono(10, .medium))
                                        .frame(width: 16, height: 16)
                                        .background(s.days.contains(d) ? Brand.accentFill.opacity(0.35) : .clear, in: .circle)
                                        .foregroundStyle(s.days.contains(d) ? Vesper.textPrimary : Vesper.textTertiary)
                                }
                            }
                        }
                        Spacer()
                        Toggle("", isOn: Binding(get: { s.enabled }, set: { on in
                            Task { await run { try await session.setSchedule(s.id, enabled: on) } }
                        }))
                        .labelsHidden()
                    }
                    .swipeActions {
                        Button("Delete", role: .destructive) { Task { await run { try await session.deleteSchedule(s.id) } } }
                    }
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Brand.backdrop)
        .navigationTitle("Schedules")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button { adding = true } label: { Image(systemName: "plus") }.accessibilityLabel("Add a schedule")
            }
        }
        .task {
            await run { try await session.schedules() }
            modes = (try? await session.modes()) ?? []
        }
        .sheet(isPresented: $adding) {
            AddScheduleSheet(modes: modes) { mode, time, days in
                Task { await run { try await session.addSchedule(mode: mode, time: time, days: days) } }
            }
            .presentationDetents([.medium, .large])
        }
    }

    private func title(of mode: String) -> String {
        modes.first { $0.id == mode }?.title ?? mode.replacingOccurrences(of: "_", with: " ").capitalized
    }

    private func run(_ op: () async throws -> [ModeSchedule]) async {
        do { schedules = try await op(); error = nil } catch { self.error = error.localizedDescription }
    }
}

private struct AddScheduleSheet: View {
    let modes: [BoxMode]
    let onAdd: (String, String, [Int]) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var mode = "chat"
    @State private var time = Calendar.current.date(bySettingHour: 8, minute: 0, second: 0, of: .now) ?? .now
    @State private var days: Set<Int> = [1, 2, 3, 4, 5]

    var body: some View {
        NavigationStack {
            Form {
                Picker("Mode", selection: $mode) {
                    ForEach(modes.isEmpty ? [] : modes) { m in Text(m.title).tag(m.id) }
                    if modes.isEmpty { Text("Chat").tag("chat") }
                }
                DatePicker("Time", selection: $time, displayedComponents: .hourAndMinute)
                HStack {
                    ForEach(0..<7, id: \.self) { d in
                        Button {
                            if days.contains(d) { days.remove(d) } else { days.insert(d) }
                        } label: {
                            Text(SchedulesView.dayLetters[d])
                                .font(.ttMono(13, .medium))
                                .frame(width: 34, height: 34)
                                .background(days.contains(d) ? Brand.accentFill : Vesper.stone200, in: .circle)
                                .foregroundStyle(Vesper.textPrimary)
                        }
                        .buttonStyle(.plain)
                        .frame(maxWidth: .infinity)
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            .navigationTitle("New schedule")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add") {
                        let c = Calendar.current.dateComponents([.hour, .minute], from: time)
                        onAdd(mode, String(format: "%02d:%02d", c.hour ?? 8, c.minute ?? 0), days.sorted())
                        dismiss()
                    }
                    .disabled(days.isEmpty)
                }
            }
        }
    }
}

// MARK: - SSH key

/// This iPhone's own SSH key (see `SSHKey`): its fingerprint, the command to
/// reach this box, and putting the public half on the box or taking it off.
/// The private half stays on the phone unless you export it, behind a warning.
struct SSHKeyView: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @State private var key: SSHKey?
    @State private var keyError: String?
    @State private var info: SSHInfo?
    @State private var working = false
    @State private var confirmExport = false
    @State private var exportedText: String?
    @State private var exportedFile: URL?

    /// The saved box (it picks up the SSH user once the box reports it).
    private var box: SavedBox { app.store.boxes.first { $0.id == session.box.id } ?? session.box }
    private var command: String? { (info?.user ?? box.sshUser).map { "ssh \($0)@\(box.host)" } }
    /// Whether the box lists a key under this phone's label.
    private var keyOnBox: Bool? {
        guard let info else { return nil }
        let label = SSHKey.label
        return info.keys.contains { $0.raw == label }
    }

    var body: some View {
        List {
            Section {
                Text("This iPhone has its own SSH key, made on the phone. The box only gets its public half; the private half never leaves this iPhone unless you export it.")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if let keyError {
                Section { Label(keyError, systemImage: "exclamationmark.triangle").foregroundStyle(Vesper.warning) }
            }
            if let key {
                Section("This iPhone's key") {
                    LabeledContent("Type", value: "Ed25519")
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Fingerprint").font(.tt(.caption)).foregroundStyle(.secondary)
                        Text(key.fingerprint)
                            .font(.ttMono(12, relativeTo: .caption))
                            .textSelection(.enabled)
                    }
                }
                Section {
                    if let command {
                        Button { UIPasteboard.general.string = command } label: {
                            Label(command, systemImage: "doc.on.doc")
                                .font(.ttMono(13, relativeTo: .footnote)).lineLimit(2).multilineTextAlignment(.leading)
                        }
                    }
                    if let keyOnBox {
                        Label(keyOnBox ? "This iPhone's key is on \(box.name)." : "This iPhone's key isn't on \(box.name).",
                              systemImage: keyOnBox ? "checkmark.circle" : "circle")
                            .font(.tt(.caption))
                            .foregroundStyle(keyOnBox ? Vesper.success : Vesper.textPrimary)
                    }
                    if let info, !info.listening {
                        Label("The box's SSH server isn't running, so SSH apps can't connect yet.", systemImage: "exclamationmark.triangle")
                            .font(.tt(.caption)).foregroundStyle(Vesper.warning)
                    }
                    if let result = app.sshResults[box.id] {
                        Label(result.message, systemImage: result.ok ? "checkmark.circle" : "exclamationmark.triangle")
                            .font(.tt(.caption))
                            .foregroundStyle(result.ok ? Vesper.success : Vesper.warning)
                    }
                    Button("Send key to this box") { Task { await send() } }
                        .disabled(working)
                    Button("Remove from this box", role: .destructive) { Task { await remove() } }
                        .disabled(working)
                } header: {
                    Text("On \(box.name)")
                } footer: {
                    Text("TT Station sends the key when you pair. Tap the command to copy it.")
                }
                Section {
                    if let exportedText, let exportedFile {
                        ShareLink(item: exportedFile) {
                            Label("Share the private key file", systemImage: "square.and.arrow.up")
                        }
                        Button {
                            // Only on this device, and gone from the clipboard in two minutes.
                            UIPasteboard.general.setItems([["public.utf8-plain-text": exportedText]],
                                                          options: [.localOnly: true,
                                                                    .expirationDate: Date().addingTimeInterval(120)])
                        } label: {
                            Label("Copy the private key", systemImage: "doc.on.doc")
                        }
                    } else {
                        Button("Export private key for another SSH app") { confirmExport = true }
                    }
                } header: {
                    Text("Other SSH apps")
                } footer: {
                    Text("For apps like Termius or Blink. Anyone with a copy of the private key can sign in to every box that has this iPhone's key.")
                }
            } else if keyError == nil {
                Section { ProgressView() }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Brand.backdrop)
        .navigationTitle("SSH key")
        .navigationBarTitleDisplayMode(.inline)
        .task { await load() }
        .onDisappear { discardExport() }
        .alert("Export the private key?", isPresented: $confirmExport) {
            Button("Export", role: .destructive) { export() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The key will leave TT Station. Anyone who gets a copy can sign in to every box that has this iPhone's key, until you remove it there. Only give it to an SSH app you trust.")
        }
    }

    private func load() async {
        do {
            key = try SSHKey.load()
        } catch {
            keyError = error.localizedDescription
            return
        }
        info = try? await session.sshInfo()
    }

    private func send() async {
        working = true
        defer { working = false }
        await app.sendSSHKey(to: box)
        info = try? await session.sshInfo()
    }

    private func remove() async {
        guard let key else { return }
        working = true
        defer { working = false }
        do {
            try await session.revokeSSH(publicKey: key.publicKeyLine)
            app.sshResults[box.id] = AppModel.SSHResult(ok: true, message: "Removed this iPhone's SSH key from \(box.name).")
        } catch {
            app.sshResults[box.id] = AppModel.SSHResult(ok: false, message: "Couldn't remove the key: \(error.localizedDescription)")
        }
        info = try? await session.sshInfo()
    }

    private func export() {
        guard let key else { return }
        let text = key.openSSHPrivateKey()
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("ttstation_iphone_ed25519")
        do {
            try Data(text.utf8).write(to: url, options: [.atomic, .completeFileProtection])
        } catch {
            keyError = error.localizedDescription
            return
        }
        exportedText = text
        exportedFile = url
    }

    /// Don't leave an exported copy lying around in the app's temporary files.
    private func discardExport() {
        if let exportedFile { try? FileManager.default.removeItem(at: exportedFile) }
        exportedFile = nil
        exportedText = nil
    }
}

// MARK: - Energy

/// What the box used: per day for the last month, per month since it started counting.
struct EnergyView: View {
    let session: BoxSession
    @State private var report: EnergyReport?
    @State private var error: String?
    @AppStorage("tt.energy.pricePerKWh") private var price = 0.15

    var body: some View {
        List {
            if let error { Section { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(Vesper.warning) } }
            if let report {
                let thisMonth = report.months.last
                Section {
                    HStack(alignment: .firstTextBaseline) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text("THIS MONTH").font(.ttMono(11, .medium)).foregroundStyle(Vesper.textTertiary)
                            Text(String(format: "%.1f kWh", thisMonth?.energyKwh ?? 0)).font(.tt(.largeTitle))
                            Text(String(format: "≈ $%.2f · %@ tokens", (thisMonth?.energyKwh ?? 0) * price,
                                        compact(thisMonth?.tokens ?? 0)))
                                .font(.tt(.subheadline)).foregroundStyle(.secondary)
                        }
                        Spacer()
                    }
                    .padding(.vertical, 4)
                }
                Section("Last 31 days") {
                    Chart(report.days) { d in
                        BarMark(x: .value("Day", String(d.day.suffix(2))), y: .value("kWh", d.energyKwh))
                            .foregroundStyle(Brand.accentFill.gradient)
                    }
                    .chartXAxis { AxisMarks(values: .automatic(desiredCount: 6)) }
                    .frame(height: 160)
                }
                Section("By month") {
                    ForEach(report.months.reversed()) { m in
                        HStack {
                            Text(monthName(m.month))
                            Spacer()
                            Text(String(format: "%.1f kWh · $%.2f", m.energyKwh, m.energyKwh * price))
                                .font(.ttMono(13, relativeTo: .footnote)).foregroundStyle(.secondary)
                        }
                    }
                }
                Section {
                    Stepper(value: $price, in: 0...1, step: 0.01) {
                        Text(String(format: "Electricity: $%.2f per kWh", price))
                    }
                } footer: {
                    Text("Measured from the chips' own power readings, so the rest of the computer isn't included. Counting started when this version of the agent did.")
                }
            } else {
                Section { ProgressView() }
            }
        }
        .scrollContentBackground(.hidden)
        .background(Brand.backdrop)
        .navigationTitle("Energy")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            do { report = try await session.energy() } catch { self.error = error.localizedDescription }
        }
    }

    private func compact(_ n: Double) -> String {
        n >= 1e9 ? String(format: "%.1fB", n / 1e9) : n >= 1e6 ? String(format: "%.1fM", n / 1e6)
            : n >= 1e3 ? String(format: "%.0fk", n / 1e3) : String(format: "%.0f", n)
    }

    private func monthName(_ ym: String) -> String {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM"
        guard let d = f.date(from: ym) else { return ym }
        return d.formatted(.dateTime.month(.wide).year())
    }
}

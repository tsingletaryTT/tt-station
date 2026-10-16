import SwiftUI

/// Settings: everything you set once and then forget, out of the way of the
/// everyday tabs. Your boxes, how this box connects and tells you things,
/// power, the assistant's memory and documents, and how the app looks.
struct SettingsView: View {
    @Environment(AppModel.self) private var app
    @AppStorage(Temp.key) private var tempUnit = "c"
    @State private var open: Sheet?

    enum Sheet: String, Identifiable {
        case connect, notifications, power, software, doctor, memories, documents
        var id: String { rawValue }
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    NavigationLink {
                        BoxesList()
                    } label: {
                        row("server.rack", "Boxes",
                            detail: app.activeBox.map { "Using \($0.name)" } ?? "Pair a box",
                            value: "\(app.store.boxes.count)")
                    }
                }

                if let session = app.session {
                    Section {
                        NavigationLink { HouseholdView(session: session) } label: {
                            row("person.2", "You and your household", detail: "Who this phone is, for the assistant's memory")
                        }
                        NavigationLink { SchedulesView(session: session) } label: {
                            row("calendar.badge.clock", "Schedules", detail: "Switch modes at set times")
                        }
                        NavigationLink { SSHKeyView(session: session) } label: {
                            row("key", "SSH key", detail: "This iPhone's own key for SSH into the box")
                        }
                        NavigationLink { EnergyView(session: session) } label: {
                            row("bolt", "Energy", detail: "kWh and cost by day and month")
                        }
                        NavigationLink { DevicesView(session: session) } label: {
                            row("lock.shield", "Devices and security",
                                detail: session.access.map { $0.owner ? "You're an owner" : "You're a member" }
                                    ?? "Paired devices, roles, activity")
                        }
                    }
                    Section {
                        sheetRow(.connect, "point.3.connected.trianglepath.dotted", "Connect apps",
                                 detail: "Use the box as a private OpenAI API from your laptop")
                        sheetRow(.notifications, "bell.badge", "Notifications",
                                 detail: "Downloads, models ready, bring-ups, transcripts")
                        sheetRow(.power, "power", "Sleep and wake",
                                 detail: "Sleep the model or the box, wake it, restart, shut down")
                    } header: {
                        Text(session.box.name)
                    }

                    Section("Box care") {
                        sheetRow(.doctor, "stethoscope", "Box Doctor", detail: "Health checks and one-tap fixes")
                        sheetRow(.software, "shippingbox", "Software",
                                 detail: "tt, tt-smi, firmware and driver versions")
                        NavigationLink { UpdatesView(session: session) } label: {
                            row("arrow.down.circle", "Updates", detail: "TT Station on the box")
                        }
                    }

                    Section("Assistant") {
                        sheetRow(.memories, "brain", "Memories", detail: "What the assistant remembers about you")
                        sheetRow(.documents, "doc.text.magnifyingglass", "Documents",
                                 detail: "Files and transcripts it can search")
                    }
                }

                Section {
                    Picker(selection: $tempUnit) {
                        Text("Celsius").tag("c")
                        Text("Fahrenheit").tag("f")
                    } label: {
                        row("thermometer.medium", "Temperature")
                    }
                } header: {
                    Text("Appearance")
                } footer: {
                    Text("The box reports Celsius; Fahrenheit is converted on the phone.")
                }

                Section("About") {
                    LabeledContent("TT Station", value: version)
                    if let mesh = app.session?.status?.deviceMesh {
                        LabeledContent("Hardware", value: mesh.uppercased())
                    }
                    if let address = app.session?.address {
                        LabeledContent("Address", value: address)
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            .navigationTitle("Settings")
            .sheet(item: $open) { sheet in
                if let session = app.session {
                    switch sheet {
                    case .connect: ConnectAppsView(session: session)
                    case .notifications: AlertsView(session: session)
                    case .power: PowerSheet(session: session)
                    case .software: SoftwareView(session: session)
                    case .doctor: DoctorSheet(session: session)
                    case .memories: MemoriesSheet(session: session)
                    case .documents: DocumentsSheet(session: session)
                    }
                }
            }
        }
    }

    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }

    private func sheetRow(_ sheet: Sheet, _ icon: String, _ title: String, detail: String) -> some View {
        Button { open = sheet } label: {
            HStack {
                row(icon, title, detail: detail)
                Spacer(minLength: 0)
                Image(systemName: "chevron.right").font(.tt(.footnote).weight(.semibold)).foregroundStyle(.tertiary)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func row(_ icon: String, _ title: String, detail: String? = nil, value: String? = nil) -> some View {
        HStack(spacing: 12) {
            Image(systemName: icon)
                .font(.tt(.body).weight(.medium))
                .foregroundStyle(Brand.accent)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                if let detail {
                    Text(detail).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            if let value {
                Spacer(minLength: 8)
                Text(value).foregroundStyle(.secondary)
            }
        }
    }
}

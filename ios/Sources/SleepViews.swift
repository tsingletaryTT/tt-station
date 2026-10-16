import SwiftUI

/// Power → The model: put the served model to sleep now, wake it, and the
/// idle-sleep rule. The machine stays on and reachable throughout, so this is
/// the sleep that works over Wi-Fi and from anywhere the phone can reach the box.
/// Starting and stopping the model follows the owners' model-control rule.
struct ModelSleepCard: View {
    let session: BoxSession
    @State private var working = false
    @State private var confirming = false
    @State private var message: String?

    private var allowed: Bool { session.access?.mayControlModels ?? true }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SectionTitle(text: "The model")
            if let asleep = session.idle?.asleep {
                Label {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("\(Brand.shortModel(asleep.label)) is asleep").font(.tt(.headline))
                        Text("Since \(Date(timeIntervalSince1970: asleep.at).formatted(.relative(presentation: .named))). The chips are idle; the box is on.")
                            .font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: "moon.zzz.fill").foregroundStyle(Brand.accent)
                }
                actionButton(session.idle?.waking == true ? "Waking…" : "Wake \(Brand.shortModel(asleep.label))",
                             icon: "sunrise.fill", prominent: true) { await wake() }
                    .disabled(session.idle?.waking == true || session.startingModel != nil)
            } else if session.isServing {
                Text("Stop the model to save power, keeping the box on and reachable. It loads again when you wake it\(session.idle?.settings.wakeOnRequest == false ? "" : ", or when chat, Code or an app asks for it") (a few minutes).")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
                actionButton("Put the model to sleep", icon: "moon.zzz", prominent: false) { confirming = true }
            } else {
                Text(session.startingModel != nil ? "A model is starting." : "Nothing is serving, so the chips are already idle.")
                    .font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if let note = session.access?.modelsNote {
                Label(note, systemImage: "lock").font(.tt(.caption)).foregroundStyle(.secondary)
            }
            if let message {
                Text(message).font(.tt(.footnote)).foregroundStyle(.secondary)
            }
            if let settings = session.idle?.settings {
                Divider().opacity(0.3)
                IdleRuleEditor(session: session, settings: settings)
                    .disabled(!allowed)
            }
        }
        .padding(18)
        .card(radius: 26)
        .task(id: session.box.id) { await session.refreshAccess() }
        .confirmationDialog("Put \(session.serve?.displayModel ?? "the model") to sleep?", isPresented: $confirming,
                            titleVisibility: .visible) {
            Button("Put it to sleep") { Task { await sleep() } }
        } message: {
            Text(sleepMessage)
        }
    }

    private var sleepMessage: String {
        let wakes = session.idle?.settings.wakeOnRequest == false
            ? "It stays asleep until someone wakes it here or on the box."
            : "The next chat, Code or app request wakes it, and waits a few minutes while it loads."
        return "Everyone using \(session.box.name) loses chat, Code and the API until it's back. \(wakes)"
    }

    @ViewBuilder
    private func actionButton(_ title: String, icon: String, prominent: Bool,
                              _ work: @escaping () async -> Void) -> some View {
        let button = Button {
            Task { await work() }
        } label: {
            HStack {
                if working { ProgressView() }
                Label(title, systemImage: icon)
            }
            .frame(maxWidth: .infinity)
        }
        if prominent {
            button.buttonStyle(.glassProminent).controlSize(.large).disabled(working || !allowed)
        } else {
            button.buttonStyle(.glass).controlSize(.large).disabled(working || !allowed)
        }
    }

    private func sleep() async {
        working = true
        defer { working = false }
        do {
            let label = try await session.sleepModel()
            message = "\(Brand.shortModel(label)) is asleep."
        } catch {
            message = error.localizedDescription
        }
    }

    private func wake() async {
        working = true
        defer { working = false }
        do {
            let label = try await session.wakeModel()
            message = "Waking \(Brand.shortModel(label)). It's ready in a few minutes; you'll get an alert."
        } catch {
            message = error.localizedDescription
        }
    }
}

/// Idle sleep: put the model to sleep after a quiet spell, and wake it on use.
private struct IdleRuleEditor: View {
    let session: BoxSession
    @State var settings: IdleSettings
    @State private var problem: String?

    private static let choices = [15, 30, 60, 120, 240, 480]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Toggle(isOn: $settings.enabled) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Sleep when nobody uses it").font(.tt(.subheadline))
                    Text("Counts every app, phone, routine and Code session.").font(.tt(.caption)).foregroundStyle(.secondary)
                }
            }
            if settings.enabled {
                Picker("After", selection: $settings.afterMinutes) {
                    ForEach(Self.choices + (Self.choices.contains(settings.afterMinutes) ? [] : [settings.afterMinutes]), id: \.self) { m in
                        Text(m < 60 ? "\(m) minutes" : m % 60 == 0 ? "\(m / 60) hour\(m == 60 ? "" : "s")" : "\(m) minutes").tag(m)
                    }
                }
                .font(.tt(.subheadline))
            }
            Toggle(isOn: $settings.wakeOnRequest) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Wake when something asks").font(.tt(.subheadline))
                    Text("Chat, Tori, Code and apps start it again instead of failing.").font(.tt(.caption)).foregroundStyle(.secondary)
                }
            }
            if let problem { Text(problem).font(.tt(.caption)).foregroundStyle(.orange) }
        }
        .onChange(of: settings) { _, new in
            Task {
                do { try await session.setIdle(new); problem = nil } catch { problem = error.localizedDescription }
            }
        }
    }
}

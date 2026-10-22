// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// Models → Everything this box can serve: its catalog and what's running now
/// (`GET /code/models`), started the way the box says to. Shows the load while it happens, and the
/// owners' rule on who may start and stop models.
struct ServeAnyView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var options: ServeOptions?
    @State private var error: String?
    @State private var query = ""
    @State private var pending: ServeOption?
    @State private var stopping = false
    @State private var confirmStop = false

    private var allowed: Bool { session.access?.mayControlModels ?? true }

    var body: some View {
        NavigationStack {
            List {
                if let note = session.access?.modelsNote {
                    Section { Label(note, systemImage: "lock").font(.tt(.footnote)) }
                }
                if let serve = options?.serve, serve.phase == "starting" {
                    Section { LoadProgress(model: serve.model, since: serve.since, eta: serve.etaS) }
                } else if let failure = session.startFailure {
                    Section { Label(failure, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange) }
                }
                if let catalogError = options?.catalogError {
                    Section { Text(catalogError).font(.tt(.footnote)).foregroundStyle(.secondary) }
                }
                ForEach(ServeGroups.make(options?.models ?? [], query: query)) { group in
                    Section(group.name) {
                        ForEach(group.options) { option in
                            ServeOptionRow(option: option, allowed: allowed, busy: session.startingModel != nil) {
                                pending = option
                            }
                        }
                    }
                }
                if let error {
                    Section { Text(error).foregroundStyle(.orange) }
                }
                if options == nil && error == nil {
                    Section { ProgressView("Asking the box…") }
                }
            }
            .navigationTitle("Everything it can serve")
            .navigationBarTitleDisplayMode(.inline)
            .searchable(text: $query, prompt: "Search models")
            .refreshable { await load() }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                if session.isServing {
                    ToolbarItem(placement: .primaryAction) {
                        Button("Stop", systemImage: "stop.fill") { confirmStop = true }
                            .disabled(!allowed || stopping)
                    }
                }
            }
            .task(id: session.box.id) {
                while !Task.isCancelled {
                    await load()
                    try? await Task.sleep(for: .seconds(options?.serve?.phase == "starting" ? 4 : 20))
                }
            }
            .confirmationDialog("Start \(pending?.name ?? "")?", isPresented: Binding(
                get: { pending != nil }, set: { if !$0 { pending = nil } }), titleVisibility: .visible,
                presenting: pending) { option in
                Button("Start \(option.name)") { Task { await start(option) } }
            } message: { option in
                Text(startMessage(option))
            }
            .confirmationDialog("Stop \(session.serve?.displayModel ?? "the model")?", isPresented: $confirmStop,
                                titleVisibility: .visible) {
                Button("Stop", role: .destructive) { Task { await stop() } }
            } message: {
                Text("Everyone using \(session.box.name) loses it: chat, Code and apps go offline until a model is started again.")
            }
        }
    }

    private func startMessage(_ option: ServeOption) -> String {
        var parts: [String] = []
        if let current = session.serve?.displayModel ?? session.status?.servingModel.map(Brand.shortModel), session.isServing {
            parts.append("This replaces \(current) for everyone using \(session.box.name).")
        }
        if let note = option.profileNote { parts.append(note) }
        parts.append(option.etaS.map { "It usually takes about \(max(1, $0 / 60)) min to load here." } ?? "Loading takes a few minutes.")
        return parts.joined(separator: " ")
    }

    private func load() async {
        do {
            options = try await session.serveOptions()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func start(_ option: ServeOption) async {
        do {
            try await session.start(option)
            error = nil
            await load()
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func stop() async {
        stopping = true
        defer { stopping = false }
        do {
            try await session.stopServing()
            await load()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct ServeOptionRow: View {
    let option: ServeOption
    let allowed: Bool
    let busy: Bool
    let onStart: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text(option.profileLabel ?? option.name).font(.tt(.body).weight(.semibold)).lineLimit(1)
                Text(ServeGroups.detail(option)).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
                if let tools = option.tools {
                    Label(tools.verified ? "Tool calls work" : "Tool calls unverified",
                          systemImage: tools.verified ? "checkmark.seal" : "questionmark.circle")
                        .font(.tt(.caption2)).foregroundStyle(tools.verified ? Brand.accent : .secondary)
                }
                if let blocked = option.blocked {
                    Text(blocked).font(.tt(.caption2)).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 8)
            if option.isInUse {
                StatusPill(state: .serving, text: "Serving")
            } else if option.canStart {
                Button("Start", action: onStart)
                    .buttonStyle(.glass)
                    .controlSize(.small)
                    .disabled(!allowed || busy)
            }
        }
        .padding(.vertical, 2)
    }
}

/// A model loading: how long so far, and roughly how long to go.
struct LoadProgress: View {
    let model: String?
    let since: Double?
    let eta: Int?

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            let elapsed = since.map { max(0, context.date.timeIntervalSince1970 - $0) } ?? 0
            VStack(alignment: .leading, spacing: 8) {
                Text("Loading \(model.map(Brand.shortModel) ?? "the model")").font(.tt(.subheadline).weight(.semibold))
                if let eta, eta > 0 {
                    ProgressView(value: min(elapsed / Double(eta), 0.97)).tint(Brand.accent)
                    Text("\(Int(elapsed) / 60) min so far · usually about \(max(1, eta / 60)) min")
                        .font(.tt(.caption)).foregroundStyle(.secondary)
                } else {
                    ProgressView().progressViewStyle(.linear)
                    Text("\(Int(elapsed) / 60) min so far").font(.tt(.caption)).foregroundStyle(.secondary)
                }
            }
        }
    }
}

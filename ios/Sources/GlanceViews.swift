// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// The Box tab's "right now": the model's state (serving, not answering,
/// asleep, starting), the chips, and what's running on the box and who
/// started it -- Code sessions, research, a routine.
struct BoxGlanceCard: View {
    let session: BoxSession
    let telemetry: TelemetryFeed
    @State private var code: [CodeSession] = []
    @State private var research: [ResearchJob] = []
    @State private var routine: RoutineRun?
    @State private var restarting = false
    @State private var problem: String?
    @State private var showPower = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                StatusPill(state: pill.0, text: pill.1)
                if let hottest = telemetry.hottest, telemetry.isLive() {
                    Label("\(telemetry.chips.count) chips · \(Temp.string(hottest))", systemImage: "thermometer.medium")
                        .font(.tt(.caption).monospacedDigit())
                        .foregroundStyle(Brand.tempColor(hottest))
                }
                Spacer(minLength: 0)
            }
            if let health = session.status?.modelHealth, health.isDegraded, session.isServing {
                VStack(alignment: .leading, spacing: 8) {
                    Text("\(session.serve?.displayModel ?? "The model") isn't answering\(health.reason.map { ": \($0)" } ?? ".")")
                        .font(.tt(.footnote))
                    Button {
                        Task { await restart() }
                    } label: {
                        HStack {
                            if restarting { ProgressView() }
                            Label("Restart the model", systemImage: "arrow.clockwise")
                        }
                    }
                    .buttonStyle(.glass)
                    .controlSize(.small)
                    .disabled(restarting || session.access?.mayControlModels == false)
                }
            }
            if let asleep = session.idle?.asleep, !session.isServing {
                Button { showPower = true } label: {
                    Label("\(Brand.shortModel(asleep.label)) is asleep to save power. Tap to wake it.", systemImage: "moon.zzz.fill")
                        .font(.tt(.footnote))
                        .multilineTextAlignment(.leading)
                }
                .buttonStyle(.plain)
            }
            ForEach(activity, id: \.self) { line in
                Label(line.text, systemImage: line.icon)
                    .font(.tt(.footnote))
            }
            if let problem { Text(problem).font(.tt(.caption)).foregroundStyle(.orange) }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .card(radius: 22)
        .task(id: session.box.id) {
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(activity.isEmpty ? 20 : 8))
            }
        }
        .sheet(isPresented: $showPower) { PowerSheet(session: session) }
    }

    private var pill: (StatusPill.Kind, String) {
        if session.statusError != nil { return (.offline, "Unreachable") }
        if let starting = session.startingModel { return (.starting, "Starting \(Brand.shortModel(starting))") }
        if session.startFailure != nil { return (.offline, "Failed to start") }
        if session.isServing {
            if session.status?.modelHealth?.isDegraded == true { return (.offline, "Not answering") }
            return (.serving, "Serving")
        }
        if session.idle?.asleep != nil { return (.idle, "Model asleep") }
        return session.status == nil ? (.checking, "Connecting") : (.idle, "Idle")
    }

    struct Line: Hashable {
        let icon: String
        let text: String
    }

    private var activity: [Line] {
        var lines: [Line] = []
        let busy = code.filter(\.busy)
        if !busy.isEmpty {
            let waiting = code.reduce(0) { $0 + $1.waiting }
            lines.append(Line(icon: "chevron.left.forwardslash.chevron.right",
                              text: "Code: \(busy.map(\.title).joined(separator: ", "))"
                                  + (waiting > 0 ? " · \(waiting) waiting for an OK" : "")))
        }
        for job in research.filter(\.isActive).prefix(2) {
            let who = job.startedByName.map { " (\($0))" } ?? ""
            lines.append(Line(icon: "magnifyingglass", text: "Researching \u{201C}\(job.displayTitle)\u{201D}\(who)"))
        }
        if let routine {
            lines.append(Line(icon: "clock.arrow.circlepath",
                              text: "Routine \(routine.name): \(routine.status == "waiting" ? "needs your OK" : routine.activity ?? "running")"))
        }
        return lines
    }

    private func load() async {
        guard session.statusError == nil else { return }
        code = (try? await session.codeSessions()) ?? []
        research = (try? await session.research())?.jobs ?? []
        routine = (try? await session.routines())?.running
    }

    private func restart() async {
        restarting = true
        defer { restarting = false }
        do {
            _ = try await session.applyFix("restart_model")
            problem = nil
            await session.refreshStatus()
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// Above every tab when the box doesn't answer: say so, retry, and offer Wake
/// (or say plainly why waking can't work from here).
struct OfflineBanner: View {
    let session: BoxSession
    @State private var retrying = false
    @State private var note: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Image(systemName: "wifi.exclamationmark").foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Can't reach \(session.box.name)").font(.tt(.subheadline).weight(.semibold))
                    Text(note ?? "It may be asleep, off, or on another network.")
                        .font(.tt(.caption)).foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            HStack(spacing: 8) {
                Button {
                    Task {
                        retrying = true
                        await session.refreshStatus()
                        retrying = false
                    }
                } label: {
                    HStack {
                        if retrying { ProgressView().controlSize(.small) }
                        Text("Retry")
                    }
                }
                .buttonStyle(.glass)
                if case .send = WakeOnLAN.plan(for: session.box) {
                    WakeBoxButton(box: session.box) { note = $0 }
                        .controlSize(.small)
                } else if case .cannot(let why) = WakeOnLAN.plan(for: session.box) {
                    Button("Wake?") { note = why }.buttonStyle(.glass)
                }
            }
            .controlSize(.small)
        }
        .padding(12)
        .card(tint: .orange.opacity(0.18), radius: 18)
        .padding(.horizontal, 12)
    }
}

/// The box and this app no longer speak the same API: say which to update.
struct UpdateNeededBanner: View {
    let message: String

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "arrow.down.app").foregroundStyle(.orange)
            Text(message).font(.tt(.footnote))
            Spacer(minLength: 0)
        }
        .padding(12)
        .card(tint: .orange.opacity(0.2), radius: 18)
        .padding(.horizontal, 12)
    }
}

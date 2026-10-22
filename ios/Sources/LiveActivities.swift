// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import ActivityKit
import SwiftUI

/// Keeps a Live Activity per kind of long work (a model loading, a
/// transcription) in step with the box while the app runs. Between
/// updates the bar runs on from its estimate, so the Lock Screen keeps moving
/// after you put the phone down. No push needed: nothing leaves the phone.
@MainActor
final class LiveActivityCenter {
    static let shared = LiveActivityCenter()
    private var current: [String: Activity<BoxActivityAttributes>] = [:]

    var enabled: Bool { ActivityAuthorizationInfo().areActivitiesEnabled }

    /// Show or update `kind`'s activity.
    func show(kind: String, box: String, _ state: BoxActivityAttributes.ContentState) {
        guard enabled else { return }
        let content = ActivityContent(state: state, staleDate: state.estimatedEnd?.addingTimeInterval(600))
        if let activity = current[kind] ?? Activity<BoxActivityAttributes>.activities.first(where: { $0.attributes.kind == kind }) {
            current[kind] = activity
            Task { await activity.update(content) }
        } else {
            current[kind] = try? Activity.request(
                attributes: BoxActivityAttributes(boxName: box, kind: kind), content: content)
        }
    }

    /// `kind` is over: show it done for a few minutes, then let it go.
    func finish(kind: String, title: String, detail: String) {
        let activity = current.removeValue(forKey: kind)
            ?? Activity<BoxActivityAttributes>.activities.first(where: { $0.attributes.kind == kind })
        guard let activity else { return }
        let done = BoxActivityAttributes.ContentState(title: title, detail: detail, progress: 1,
                                                      estimatedStart: nil, estimatedEnd: nil,
                                                      symbol: activity.content.state.symbol, done: true)
        Task { await activity.end(ActivityContent(state: done, staleDate: nil), dismissalPolicy: .after(.now + 300)) }
    }

    func isActive(_ kind: String) -> Bool {
        current[kind] != nil || Activity<BoxActivityAttributes>.activities.contains { $0.attributes.kind == kind }
    }
}

/// Watches the active box and feeds `LiveActivityCenter`.
struct LiveActivityDriver: ViewModifier {
    @Environment(AppModel.self) private var app
    @Environment(\.scenePhase) private var scenePhase

    func body(content: Content) -> some View {
        content.task(id: "\(app.session?.box.id ?? "")-\(scenePhase == .active)") {
            guard scenePhase == .active else { return }
            while !Task.isCancelled {
                await tick()
                try? await Task.sleep(for: .seconds(8))
            }
        }
    }

    private func tick() async {
        guard let session = app.session else { return }
        let center = LiveActivityCenter.shared
        let box = session.box.name

        // A model loading.
        if let starting = session.startingModel {
            let since = session.serve.map { Date(timeIntervalSince1970: TimeInterval($0.since)) } ?? .now
            let end = session.serve?.etaS.map { since.addingTimeInterval(TimeInterval($0)) }
            center.show(kind: "model", box: box, .init(
                title: "Loading \(Brand.shortModel(starting))", detail: "On \(box)'s chips",
                progress: nil, estimatedStart: since, estimatedEnd: end, symbol: "cpu", done: false))
        } else if center.isActive("model") {
            if session.isServing, let model = session.serve?.displayModel ?? session.status?.servingModel {
                center.finish(kind: "model", title: "\(Brand.shortModel(model)) is ready", detail: "Open TT Station to use it")
            } else {
                center.finish(kind: "model", title: "Model didn't start", detail: session.startFailure ?? "")
            }
        }

        // A transcription.
        let jobs = (try? await session.jobs()) ?? []
        if let job = jobs.first(where: { $0.status == "running" || $0.status == "switching" }) {
            center.show(kind: "transcribe", box: box, .init(
                title: "Transcribing \(job.title)",
                detail: job.status == "switching" ? "Loading Whisper" : "Part \(job.done) of \(job.total)",
                progress: job.total > 0 ? Double(job.done) / Double(job.total) : nil,
                estimatedStart: nil, estimatedEnd: nil, symbol: "waveform", done: false))
        } else if center.isActive("transcribe") {
            let last = jobs.first
            center.finish(kind: "transcribe", title: last?.status == "done" ? "Transcript ready" : "Transcription stopped",
                          detail: last?.title ?? "")
        }
    }
}

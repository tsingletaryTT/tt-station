import Foundation

/// What a box's popover row should show at a glance: idle, spinning up a
/// model, or actively serving one (plus how many other models are also
/// serving alongside it). Pure derivation over `BoxViewModel`'s existing
/// `serving`/`status`/`starting` fields — no I/O, so it's fully unit-tested
/// here and the view (`BoxRowView`) just renders whatever this produces.
public enum RunningState: Equatable {
    case idle
    case starting
    /// `primary` is the model to headline; `others` (>= 0) is how many
    /// additional distinct models are also serving on this box.
    case serving(primary: String, others: Int)

    /// Derives the row's running state from the three signals `BoxViewModel`
    /// already tracks:
    /// - `starting`: true from the moment `run()` is invoked until its
    ///   endpoint resolves. Takes precedence over everything else so the
    ///   spin-up state is visible even before `status`/`serving` catch up.
    /// - `serving`: the unauthed `/serving` list — the most complete signal,
    ///   since it includes models this box's agent didn't launch itself
    ///   (`source == "external"`, e.g. tt-studio). Agent-launched entries are
    ///   listed first (they're "this box's own" model, most relevant to an
    ///   owner glancing at the popover), then external ones, in each
    ///   group's original relative order. Duplicate model names collapse to
    ///   their first occurrence.
    /// - `status`: the authed single-model status, used only when `serving`
    ///   came back empty (e.g. the `/serving` read failed, or ran before
    ///   pairing) — never fatal, just a smaller, best-effort signal.
    public static func runningState(
        serving: [ServingEntry],
        status: ServingStatus?,
        starting: Bool
    ) -> RunningState {
        if starting { return .starting }

        let models: [String]
        if !serving.isEmpty {
            let agentModels = serving.filter { $0.source == "agent" }.map(\.model)
            let externalModels = serving.filter { $0.source != "agent" }.map(\.model)
            models = dedupPreservingOrder(agentModels + externalModels)
        } else if case let .serving(model) = status {
            models = [model]
        } else {
            models = []
        }

        guard let primary = models.first else { return .idle }
        return .serving(primary: primary, others: models.count - 1)
    }

    /// Collapses duplicates to their first occurrence, preserving relative order.
    private static func dedupPreservingOrder(_ items: [String]) -> [String] {
        var seen = Set<String>()
        var result: [String] = []
        for item in items where seen.insert(item).inserted {
            result.append(item)
        }
        return result
    }
}

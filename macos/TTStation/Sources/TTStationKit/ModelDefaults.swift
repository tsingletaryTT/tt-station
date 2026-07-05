import Foundation

/// Pure, side-effect-free heuristics for the model-browsing UX:
///   * `pickDefaultModel` — the "just works" smart default the view seeds
///     `selectedModel` with, so a freshly-paired box already has a sensible
///     model chosen without the user touching anything.
///   * `groupModelsByFamily` — buckets a flat `[ModelInfo]` into readable,
///     sorted vendor/family sections for the searchable browser.
///
/// Everything here is deterministic and testable in isolation (no I/O, no
/// UserDefaults, no clock) — the view and view-model call in but hold no
/// logic of their own. Keep it that way.
public enum ModelDefaults {

    // MARK: Smart default selection

    /// Pick the model a paired box should default to.
    ///
    /// Hardware-compatible-first: a pre-selected model that can't actually
    /// run on this box's mesh is worse than useless, so compatibility gates
    /// the choice before the name-based score ever gets a vote.
    ///
    /// - If `lastUsed` names a model still present in `models` *and* that
    ///   model is compatible with `boxMesh` (or `boxMesh` is `nil`, i.e.
    ///   unknown — nothing to gate on), honour it: the user's explicit last
    ///   choice wins over any heuristic. A last-used model that no longer
    ///   fits this box's mesh is treated the same as an absent one.
    /// - Otherwise, among the models compatible with `boxMesh`
    ///   (`ModelRanking.meshMatches`), score every candidate by name
    ///   (chat-tuned + a mid-small parameter count score best) and return
    ///   the top one, breaking ties deterministically by name so the result
    ///   is stable across runs.
    /// - If none are compatible (or `boxMesh` is `nil`), fall back to the
    ///   same scoring over *all* models, unfiltered — a best guess beats no
    ///   default at all.
    /// - Returns `nil` only when `models` is empty.
    public static func pickDefaultModel(
        from models: [ModelInfo], lastUsed: String?, boxMesh: String? = nil
    ) -> String? {
        if let lastUsed,
           let match = models.first(where: { $0.name == lastUsed }),
           boxMesh == nil || ModelRanking.meshMatches(match.devices, boxMesh: boxMesh) {
            return lastUsed
        }
        guard !models.isEmpty else { return nil }
        let compatible = models.filter { ModelRanking.meshMatches($0.devices, boxMesh: boxMesh) }
        return bestByScore(compatible.isEmpty ? models : compatible)
    }

    /// The top-scoring model by name, breaking ties deterministically by
    /// name (alphabetically-earliest wins) so the result is stable across
    /// runs. `nil` only when `candidates` is empty.
    private static func bestByScore(_ candidates: [ModelInfo]) -> String? {
        candidates
            .max { lhs, rhs in
                let ls = score(lhs.name), rs = score(rhs.name)
                if ls != rs { return ls < rs }
                // Deterministic tie-break: smaller name sorts first, so the
                // "max" of equal scores is the alphabetically-earliest name.
                return lhs.name > rhs.name
            }?
            .name
    }

    /// Higher is more desirable. Chat-tuned (instruct) models dominate base
    /// models regardless of size; within a tuning class a ~7–9B model is the
    /// sweet spot, huge (≥70B, slow/OOM-prone) and tiny (<2B, weak) models
    /// are down-ranked (tiny worst of all, so mid > huge > tiny).
    static func score(_ name: String) -> Int {
        var total = 0
        // Instruct bonus is larger than the whole size-score range so an
        // instruct model always outranks a base one (the spec's
        // "base below instruct" invariant).
        if isInstruct(name) { total += 100 }
        total += sizeScore(paramCountB(name))
        return total
    }

    /// Whether the name marks a chat/instruction-tuned checkpoint.
    static func isInstruct(_ name: String) -> Bool {
        let lower = name.lowercased()
        return lower.contains("instruct")
            || lower.contains("-it")
            || lower.hasSuffix("-it")
            || lower.contains("chat")
    }

    /// Bucketed desirability of a parameter count (in billions). Unknown
    /// size is neutral (0). Ordering enforces mid (≈7–9B) > huge (≥70B) >
    /// tiny (<2B).
    static func sizeScore(_ billions: Double?) -> Int {
        guard let b = billions else { return 0 }
        if b < 2   { return -30 }          // weak
        if b < 6   { return  10 }
        if b <= 10 { return  30 }          // sweet spot (~7–9B)
        if b <= 34 { return  15 }
        if b < 70  { return   0 }
        return -20                         // ≥70B: slow / OOM-prone
    }

    /// Parse a `\d+(\.\d+)?B` parameter hint out of a model name, returning
    /// the count in billions (`Qwen3-8B` → 8, `Llama-3.3-70B` → 70,
    /// `Qwen2.5-0.5B` → 0.5). The `2.5` in `Qwen2.5` is ignored because it
    /// is not followed by a `B`. Returns `nil` when there is no size hint.
    static func paramCountB(_ name: String) -> Double? {
        guard let r = name.range(of: "[0-9]+(\\.[0-9]+)?[Bb]", options: .regularExpression) else {
            return nil
        }
        var s = String(name[r])
        s.removeLast() // drop the trailing B / b
        return Double(s)
    }

    // MARK: Family grouping

    /// A readable vendor/family label for a model name:
    ///   * `Qwen/Qwen3-32B`                       → `Qwen`
    ///   * `meta-llama/Llama-3.1-8B-Instruct`     → `Llama`
    ///   * `Qwen2.5-7B-Instruct`                  → `Qwen`
    ///
    /// Strategy: take the model id (the part after the last `/`), then the
    /// leading run of letters before the first digit/`-`. This reads better
    /// than the raw org (`meta-llama` → `Llama`). Falls back to the token
    /// before the first `-`, then the whole id.
    public static func familyName(for name: String) -> String {
        let modelPart = name.split(separator: "/").last.map(String.init) ?? name
        let letters = modelPart.prefix { $0.isLetter }
        if !letters.isEmpty { return String(letters) }
        return modelPart.split(separator: "-").first.map(String.init) ?? modelPart
    }

    /// Group models into readable families, families sorted case-insensitively
    /// (with an exact-string tie-break for determinism) and models within a
    /// family sorted by name.
    public static func groupModelsByFamily(_ models: [ModelInfo]) -> [(family: String, models: [ModelInfo])] {
        var groups: [String: [ModelInfo]] = [:]
        for m in models { groups[familyName(for: m.name), default: []].append(m) }
        return groups
            .map { (family: $0.key, models: $0.value.sorted { $0.name < $1.name }) }
            .sorted { a, b in
                let la = a.family.lowercased(), lb = b.family.lowercased()
                return la != lb ? la < lb : a.family < b.family
            }
    }
}

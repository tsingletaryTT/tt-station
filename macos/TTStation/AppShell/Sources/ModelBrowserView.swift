import SwiftUI
import TTStationKit

/// Hardware-aware model browser for the window's detail pane.
///
/// Two rendering modes, chosen by whether `box.catalog` (Task 6's curated
/// three-tier catalog, from `tt catalog`) is present:
///
/// - **Catalog present** — the common case once a box has a compatibility
///   catalog to classify against: three tiers straight off `BoxCatalog`.
///   **Runs on this box** (`catalog.runsHere`, family-grouped) is the only
///   selectable/runnable tier — tapping a row sets `box.selectedModel`.
///   **Experimental** and **Needs other hardware** are informational only:
///   their rows are plain (not buttons), because the box genuinely cannot
///   serve those models yet, and both point at the Workbench (VS Code +
///   tt-vscode-toolkit, Terminal, tt-inference-server) as the way to go
///   beyond the paved path — a "Set up in Workbench →" affordance sits next
///   to the Experimental header.
/// - **No catalog** (`box.catalog == nil` — an older agent that predates
///   `tt catalog`, or a fetch that failed) — falls back to the original
///   two-tier `ModelRanking.rankForHardware` view over the live `/models`
///   list, unchanged from before this feature existed.
///
/// Selection still funnels into `box.selectedModel`; the search field
/// filters every tier (catalog or fallback) independently.
///
/// A plain `TextField` (not `.searchable`) for the same reason
/// `ModelPickerView` uses one: kept consistent across both surfaces rather
/// than mixing search idioms.
struct ModelBrowserView: View {
    @Bindable var box: BoxViewModel
    /// Max height of the scrollable list. `nil` = uncapped (the window);
    /// a non-nil value is there for a future popover reuse, mirroring
    /// `ModelPickerView.maxListHeight`.
    var maxListHeight: CGFloat? = nil
    /// Invoked by the "Set up in Workbench →" affordance next to the
    /// Experimental header. `BoxWorkspaceView` wires this to the same
    /// `LaunchController.openVSCode` the Workbench card's own VS Code button
    /// calls, so "go beyond the paved path" and "open the Workbench" are the
    /// literal same action. Defaults to a no-op so a caller that has no
    /// Workbench to open (there is none today, but a trimmed future reuse
    /// might not) isn't forced to wire it.
    var onOpenWorkbench: () -> Void = {}

    @State private var query = ""
    // Fallback (no-catalog) tier collapse state — unchanged from before.
    @State private var incompatibleExpanded = false
    // Catalog-mode tier collapse state. Both default collapsed: these tiers
    // are informational, not the primary "what do I run" list, so they stay
    // out of the way until the user asks to see them (same idiom the
    // fallback's "Needs other hardware" already used).
    @State private var experimentalExpanded = false
    @State private var otherHardwareExpanded = false

    // MARK: - Fallback (no catalog) data

    private var ranked: ModelRanking.RankedModels {
        ModelRanking.rankForHardware(box.models, boxMesh: box.record.deviceMesh)
    }

    private var compatibleGroups: [(family: String, models: [ModelInfo])] {
        guard !query.isEmpty else { return ranked.compatible }
        return ranked.compatible
            .map { (family: $0.family, models: $0.models.filter { $0.name.localizedCaseInsensitiveContains(query) }) }
            .filter { !$0.models.isEmpty }
    }

    private var incompatibleModels: [ModelInfo] {
        query.isEmpty
            ? ranked.incompatible
            : ranked.incompatible.filter { $0.name.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            searchField

            if let catalog = box.catalog {
                catalogBody(catalog)
            } else {
                fallbackBody
            }
        }
    }

    private var searchField: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass")
                .font(.caption).foregroundStyle(.secondary)
            TextField("Search models", text: $query)
                .textFieldStyle(.plain).font(.caption)
            if !query.isEmpty {
                Button { query = "" } label: {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help("Clear search")
            }
        }
        .padding(.horizontal, 6).padding(.vertical, 4)
        .background(Color.secondary.opacity(0.12))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    // MARK: - Catalog (3-tier) rendering

    @ViewBuilder
    private func catalogBody(_ catalog: BoxCatalog) -> some View {
        let runsHere = filteredEntries(catalog.runsHere)
        let experimental = filteredEntries(catalog.experimental)
        let otherHardware = filteredEntries(catalog.otherHardware)

        if runsHere.isEmpty && experimental.isEmpty && otherHardware.isEmpty {
            Text(catalog.runsHere.isEmpty && catalog.experimental.isEmpty && catalog.otherHardware.isEmpty
                 ? "No models available."
                 : "No models match \u{201C}\(query)\u{201D}.")
                .font(.caption).foregroundStyle(.secondary)
                .padding(.vertical, 4)
        } else if let cap = maxListHeight {
            ScrollView {
                catalogListBody(runsHere: runsHere, experimental: experimental, otherHardware: otherHardware)
            }
            .frame(maxHeight: cap)
        } else {
            catalogListBody(runsHere: runsHere, experimental: experimental, otherHardware: otherHardware)
        }

        catalogFooterNote(catalog)
    }

    /// Same `LazyVStack` + pinned family headers idiom the fallback list
    /// uses for its compatible tier — see that view's doc comment for why
    /// tier labels are plain rows rather than nested `Section` headers.
    /// Experimental/other-hardware are flat (no family grouping): they're
    /// informational asides, not the primary browsing list, so the extra
    /// structure isn't worth it there.
    private func catalogListBody(
        runsHere: [CatalogEntry], experimental: [CatalogEntry], otherHardware: [CatalogEntry]
    ) -> some View {
        LazyVStack(alignment: .leading, spacing: 2, pinnedViews: [.sectionHeaders]) {
            if !runsHere.isEmpty {
                primaryTierHeader
                ForEach(groupEntriesByFamily(runsHere), id: \.family) { group in
                    Section {
                        ForEach(group.entries, id: \.id) { runsHereRow($0) }
                    } header: {
                        familyHeader(group.family)
                    }
                }
            }

            if !experimental.isEmpty {
                experimentalHeader(count: experimental.count)
                    .padding(.top, runsHere.isEmpty ? 0 : 6)
                if experimentalExpanded {
                    ForEach(experimental, id: \.id) { entry in
                        goBeyondRow(
                            entry, dimmed: false,
                            detail: entry.software.isEmpty ? nil : entry.software.joined(separator: ", "))
                    }
                }
            }

            if !otherHardware.isEmpty {
                otherHardwareHeader(count: otherHardware.count)
                    .padding(.top, (runsHere.isEmpty && experimental.isEmpty) ? 0 : 6)
                if otherHardwareExpanded {
                    ForEach(otherHardware, id: \.id) { entry in
                        goBeyondRow(
                            entry, dimmed: true,
                            detail: "Needs \(entry.neededHardware.joined(separator: ", "))")
                    }
                }
            }
        }
    }

    /// Catalog-mode header for the primary (runsHere) tier. Labeled "Models"
    /// rather than "Runs on this box" — Task 1 narrowed `runsHere` to
    /// tt-inference-server-servable entries only (tt-forge/tt-metal
    /// "supported on this mesh" models now live in Experimental), so this is
    /// the box's actual tt-inference-server model list, not a general
    /// "compatible with this hardware" claim. The caption underneath names
    /// that engine explicitly so the demo reads "these are the models this
    /// box serves [via tt-inference-server]" at a glance. The fallback
    /// (no-catalog) tier keeps the old "Runs on this box" wording — see
    /// `fallbackListBody` — because without a catalog there's no TIS/mesh
    /// split to claim.
    private var primaryTierHeader: some View {
        VStack(alignment: .leading, spacing: 0) {
            tierHeader("Models")
            Text("tt-inference-server")
                .font(.caption2).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func experimentalHeader(count: Int) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 4) {
                Button {
                    experimentalExpanded.toggle()
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: experimentalExpanded ? "chevron.down" : "chevron.right")
                            .font(.caption2).foregroundStyle(.secondary)
                        tierHeader("Experimental (\(count))")
                    }
                }
                .buttonStyle(.plain)
                .help(experimentalExpanded ? "Collapse" : "Expand — models that might run here but aren't fully verified.")

                Spacer(minLength: 0)

                Button(action: onOpenWorkbench) {
                    Text("Set up in Workbench →").font(.caption2.weight(.semibold))
                }
                .buttonStyle(.borderless)
                .foregroundStyle(TTTheme.teal)
                .help("Open the Workbench (VS Code + tt-vscode-toolkit) for this box.")
            }
            Text("Bring these up with the tools — the Workbench (VS Code + tt-vscode-toolkit, Terminal, "
                + "tt-inference-server) runs models beyond the paved path.")
                .font(.caption2).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func otherHardwareHeader(count: Int) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Button {
                otherHardwareExpanded.toggle()
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: otherHardwareExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption2).foregroundStyle(.secondary)
                    tierHeader("Needs other hardware (\(count))")
                }
            }
            .buttonStyle(.plain)
            .help(otherHardwareExpanded ? "Collapse" : "Expand — models that need hardware this box doesn't have.")

            // No "Set up in Workbench" button here, deliberately: these
            // entries need hardware this box doesn't have, so the Workbench
            // — VS Code/Terminal/tt-inference-server on THIS box — can't
            // make them runnable. The framing says so instead of offering a
            // CTA that wouldn't actually help.
            Text("These need hardware this box doesn't have — the Workbench's tools are how you'd run "
                + "beyond the paved path on hardware you do have, not on hardware you don't.")
                .font(.caption2).foregroundStyle(.secondary)
        }
    }

    /// A "Runs on this box" row. Selectable/runnable — the only tappable
    /// row in catalog mode.
    ///
    /// `runnableModelId(for:)` resolves the string to actually hand
    /// `box.selectedModel` (and, later, `box.run()`): a `CatalogEntry.id` is
    /// a compatibility-catalog slug (e.g. `"qwen3-8b"`), not necessarily the
    /// exact string the box's live `/models` list (and therefore `POST
    /// /run`) expects (e.g. `"Qwen/Qwen3-8B"`) — see that function's doc
    /// comment.
    private func runsHereRow(_ entry: CatalogEntry) -> some View {
        let runnableId = runnableModelId(for: entry)
        let isSelected = box.selectedModel == runnableId
        return Button {
            box.selectedModel = runnableId
        } label: {
            HStack(alignment: .center, spacing: 6) {
                Image(systemName: isSelected ? "checkmark.circle.fill" : "circle")
                    .font(.caption)
                    .foregroundStyle(isSelected ? Color.accentColor : Color.secondary)
                HStack(spacing: 4) {
                    Text(entry.displayName).font(.callout.weight(.medium))
                    downloadIndicator(entry)
                }
                if let size = entry.size {
                    Spacer(minLength: 8)
                    // Compact size chip (e.g. "8B") — monospaced for the
                    // number/unit shape, a subtle capsule so it reads as
                    // metadata rather than another label competing with the
                    // model name. `TTTheme.mono` itself is pinned to
                    // `.caption` size (see its doc comment), so a caption2
                    // chip needs its own `.system(.caption2, design:
                    // .monospaced)` rather than layering a second `.font()`
                    // on top of `TTTheme.mono` — the second call would just
                    // overwrite the first and silently drop the monospaced
                    // design.
                    Text(size)
                        .font(.system(.caption2, design: .monospaced))
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(Color.secondary.opacity(0.15))
                        .clipShape(Capsule())
                } else {
                    Spacer(minLength: 0)
                }
            }
            .padding(.vertical, 4).padding(.horizontal, 6)
            .background(
                RoundedRectangle(cornerRadius: 6)
                    .fill(isSelected ? Color.accentColor.opacity(0.12) : Color.clear)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Select \(entry.displayName)")
    }

    /// Download-state indicator for a runs-here row. Distinguishes:
    /// - **downloaded** → a small solid dot in the app's "serving" green, i.e.
    ///   "weights are on the box, this starts fast."
    /// - **available but not downloaded** → a hollow cloud+download glyph, i.e.
    ///   "the box can serve this, but it downloads on first run" (which for a
    ///   70B is a large, slow pull — worth flagging before you hit Run).
    /// A non-`availableNow` entry (catalog-only, box can't serve it now) shows
    /// nothing here — it's not a "run it" candidate in the first place.
    @ViewBuilder
    private func downloadIndicator(_ entry: CatalogEntry) -> some View {
        if entry.downloaded {
            Circle()
                .fill(TTTheme.statusServing)
                .frame(width: 5, height: 5)
                .help("Downloaded on this box — starts fast")
        } else if entry.availableNow {
            Image(systemName: "arrow.down.circle")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .help("Not downloaded yet — downloads on first run (can be a large pull)")
        }
    }

    /// An Experimental/Needs-other-hardware row: informational only, not a
    /// button — these tiers are never selectable/runnable (see the type doc
    /// comment's "Don't let a user Run a model the box can't serve" rule).
    private func goBeyondRow(_ entry: CatalogEntry, dimmed: Bool, detail: String?) -> some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: "wrench.and.screwdriver")
                .font(.caption2).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(entry.displayName).font(.caption)
                if let detail, !detail.isEmpty {
                    Text(detail).font(.caption2).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 2).padding(.horizontal, 4)
        .opacity(dimmed ? 0.55 : 0.85)
    }

    @ViewBuilder
    private func catalogFooterNote(_ catalog: BoxCatalog) -> some View {
        // Mutually exclusive in practice (a stale-but-parsed cached catalog
        // implies `catalogAvailable`), so `if`/`else if` rather than two
        // independent `if`s is enough to never miss either signal.
        if !catalog.catalogAvailable {
            Text("Model catalog offline — showing this box's models.")
                .font(.caption2).foregroundStyle(.secondary)
                .padding(.top, 4)
        } else if catalog.catalogStale {
            Text("Catalog cached.")
                .font(.caption2).foregroundStyle(.secondary)
                .padding(.top, 4)
        }
    }

    // MARK: - Catalog helpers

    private func filteredEntries(_ entries: [CatalogEntry]) -> [CatalogEntry] {
        guard !query.isEmpty else { return entries }
        return entries.filter {
            $0.displayName.localizedCaseInsensitiveContains(query)
                || $0.family.localizedCaseInsensitiveContains(query)
                || $0.id.localizedCaseInsensitiveContains(query)
        }
    }

    /// Groups catalog entries by `family`, sorted the same way
    /// `ModelDefaults.groupModelsByFamily` sorts `ModelInfo` families
    /// (case-insensitive, exact-string tie-break) so the two family-grouped
    /// lists in this view (fallback-compatible / catalog-runs-here) read
    /// consistently.
    private func groupEntriesByFamily(_ entries: [CatalogEntry]) -> [(family: String, entries: [CatalogEntry])] {
        var groups: [String: [CatalogEntry]] = [:]
        for e in entries { groups[e.family, default: []].append(e) }
        return groups
            .map { (family: $0.key, entries: $0.value.sorted { $0.displayName < $1.displayName }) }
            .sorted { a, b in
                let la = a.family.lowercased(), lb = b.family.lowercased()
                return la != lb ? la < lb : a.family < b.family
            }
    }

    /// The string to hand `box.selectedModel` (and, on Run, `POST /run`) for
    /// a "runs here" catalog entry.
    ///
    /// A `CatalogEntry.id` is the compatibility catalog's own slug (e.g.
    /// `"qwen3-8b"`) — the Rust merge (`libttstation::catalog::classify`)
    /// deliberately keeps a matched entry's catalog `id` rather than
    /// substituting the live model's name, because the catalog id is the
    /// stable cross-tier identity. But `box.run()` ultimately calls the
    /// live `/run` endpoint, which needs the exact string the agent's own
    /// `/models` list uses (e.g. `"Qwen/Qwen3-8B"`) — those two strings are
    /// not always the same.
    ///
    /// So: if this entry has a live match in `box.models` — same
    /// normalized-name comparison the Rust merge uses to decide
    /// `available_now` in the first place — prefer that live model's actual
    /// `name`, which is guaranteed to be something `/run` already
    /// understands. Only fall back to the catalog `id` itself when there is
    /// no live match (a catalog-says-"supported-here" entry the box hasn't
    /// pulled yet): it's the best identifier available, and `run.py` may
    /// still resolve it, but this is a known soft spot — see the Task 7
    /// report's Concerns section.
    private func runnableModelId(for entry: CatalogEntry) -> String {
        let idKey = Self.normalizeKey(entry.id)
        let nameKey = Self.normalizeKey(entry.displayName)
        if let match = box.models.first(where: {
            let liveKey = Self.normalizeKey($0.name)
            return liveKey == idKey || liveKey == nameKey
        }) {
            return match.name
        }
        return entry.id
    }

    /// Swift-side mirror of `libttstation::catalog::normalize_key` (Rust):
    /// lowercase, drop any `org/` prefix, fold `.`/`_`/` ` to `-`, collapse
    /// runs of `-`. Kept in lockstep with that function so "the same model"
    /// resolves to the same key on both sides of the CLI/app boundary —
    /// see `runnableModelId(for:)`.
    private static func normalizeKey(_ s: String) -> String {
        let lower = s.lowercased()
        let afterSlash = lower.split(separator: "/", omittingEmptySubsequences: false).last.map(String.init) ?? lower
        var folded = ""
        for c in afterSlash {
            folded.append(c == "." || c == "_" || c == " " ? "-" : c)
        }
        var collapsed = ""
        var prevDash = false
        for c in folded {
            if c == "-" {
                if !prevDash { collapsed.append(c) }
                prevDash = true
            } else {
                collapsed.append(c)
                prevDash = false
            }
        }
        return collapsed
    }

    // MARK: - Fallback (no catalog) rendering — unchanged from before Task 7

    @ViewBuilder
    private var fallbackBody: some View {
        if compatibleGroups.isEmpty && incompatibleModels.isEmpty {
            Text(box.models.isEmpty
                 ? "No models available."
                 : "No models match \u{201C}\(query)\u{201D}.")
                .font(.caption).foregroundStyle(.secondary)
                .padding(.vertical, 4)
        } else if let cap = maxListHeight {
            // Capped presentation (popover-style reuse): bounded,
            // scrollable list.
            ScrollView { fallbackListBody }
                .frame(maxHeight: cap)
        } else {
            // Uncapped (window): no inner ScrollView — see
            // ModelPickerView's identical note about nesting ScrollViews
            // defeating LazyVStack laziness.
            fallbackListBody
        }
    }

    private func tierHeader(_ text: String) -> some View {
        Text(text)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
    }

    private func familyHeader(_ text: String) -> some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, 2)
            .background(.background)
    }

    /// Both tiers in one `LazyVStack` so `pinnedViews: [.sectionHeaders]`
    /// pins each family's header as it scrolls — the "Runs on this box" /
    /// "Needs other hardware" tier labels are plain rows (not `Section`
    /// headers) alongside them, deliberately: nesting a `Section` inside
    /// another `Section` here would make the outer tier header ambiguous
    /// under SwiftUI's pinning (which only recognizes *directly*-nested
    /// `Section`s), so family-level pinning — the more useful pin target
    /// when scrolling a long catalog — is kept unambiguous by only ever
    /// nesting one level deep.
    private var fallbackListBody: some View {
        LazyVStack(alignment: .leading, spacing: 2, pinnedViews: [.sectionHeaders]) {
            if !compatibleGroups.isEmpty {
                tierHeader("Runs on this box")
                ForEach(compatibleGroups, id: \.family) { group in
                    Section {
                        ForEach(group.models, id: \.name) { fallbackModelRow($0, dimmed: false) }
                    } header: {
                        familyHeader(group.family)
                    }
                }
            }
            if !incompatibleModels.isEmpty {
                Button {
                    incompatibleExpanded.toggle()
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: incompatibleExpanded ? "chevron.down" : "chevron.right")
                            .font(.caption2).foregroundStyle(.secondary)
                        tierHeader("Needs other hardware (\(incompatibleModels.count))")
                    }
                }
                .buttonStyle(.plain)
                .padding(.top, compatibleGroups.isEmpty ? 0 : 6)
                .help(incompatibleExpanded ? "Collapse" : "Expand — models that need hardware this box doesn't have.")

                if incompatibleExpanded {
                    ForEach(incompatibleModels, id: \.name) { fallbackModelRow($0, dimmed: true) }
                }
            }
        }
    }

    /// Same checkmark + name + secondary-detail row style as
    /// `ModelPickerView.modelRow`, but the secondary line is the hardware
    /// compatibility label rather than a raw device list, and incompatible
    /// rows render at reduced opacity (`dimmed`) so the two tiers read as
    /// distinct at a glance even before considering the section they're in.
    private func fallbackModelRow(_ model: ModelInfo, dimmed: Bool) -> some View {
        let isSelected = box.selectedModel == model.name
        let label = ModelRanking.compatibilityLabel(for: model, boxMesh: box.record.deviceMesh)
        return Button {
            box.selectedModel = model.name
        } label: {
            HStack(alignment: .top, spacing: 6) {
                Image(systemName: isSelected ? "checkmark.circle.fill" : "circle")
                    .font(.caption)
                    .foregroundStyle(isSelected ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(model.name).font(.caption)
                    if !label.isEmpty {
                        Text(label).font(.caption2).foregroundStyle(.secondary)
                    } else if !model.devices.isEmpty {
                        Text(model.devices.joined(separator: ", "))
                            .font(.caption2).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
            .padding(.vertical, 2).padding(.horizontal, 4)
            .opacity(dimmed ? 0.55 : 1.0)
        }
        .buttonStyle(.plain)
        .help("Select \(model.name)")
    }
}

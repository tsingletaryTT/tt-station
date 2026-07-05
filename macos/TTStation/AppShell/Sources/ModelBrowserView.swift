import SwiftUI
import TTStationKit

/// Hardware-aware model browser for the window's detail pane.
///
/// Replaces `ModelPickerView`'s single flat family list with
/// `ModelRanking.rankForHardware`'s two tiers: a prominent, family-grouped
/// **"Runs on this box"** section (pinned family headers, same row style as
/// `ModelPickerView`) and a dimmed, collapsible **"Needs other hardware"**
/// section where each row's secondary line is `ModelRanking.compatibilityLabel`
/// instead of a raw device list. Selection still funnels into
/// `box.selectedModel`; the search field filters both tiers independently.
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
    @State private var query = ""
    @State private var incompatibleExpanded = false

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

            if compatibleGroups.isEmpty && incompatibleModels.isEmpty {
                Text(box.models.isEmpty
                     ? "No models available."
                     : "No models match \u{201C}\(query)\u{201D}.")
                    .font(.caption).foregroundStyle(.secondary)
                    .padding(.vertical, 4)
            } else if let cap = maxListHeight {
                // Capped presentation (popover-style reuse): bounded,
                // scrollable list.
                ScrollView { listBody }
                    .frame(maxHeight: cap)
            } else {
                // Uncapped (window): no inner ScrollView — see
                // ModelPickerView's identical note about nesting ScrollViews
                // defeating LazyVStack laziness.
                listBody
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

    /// Both tiers in one `LazyVStack` so `pinnedViews: [.sectionHeaders]`
    /// pins each family's header as it scrolls — the "Runs on this box" /
    /// "Needs other hardware" tier labels are plain rows (not `Section`
    /// headers) alongside them, deliberately: nesting a `Section` inside
    /// another `Section` here would make the outer tier header ambiguous
    /// under SwiftUI's pinning (which only recognizes *directly*-nested
    /// `Section`s), so family-level pinning — the more useful pin target
    /// when scrolling a long catalog — is kept unambiguous by only ever
    /// nesting one level deep.
    private var listBody: some View {
        LazyVStack(alignment: .leading, spacing: 2, pinnedViews: [.sectionHeaders]) {
            if !compatibleGroups.isEmpty {
                tierHeader("Runs on this box")
                ForEach(compatibleGroups, id: \.family) { group in
                    Section {
                        ForEach(group.models, id: \.name) { modelRow($0, dimmed: false) }
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
                    ForEach(incompatibleModels, id: \.name) { modelRow($0, dimmed: true) }
                }
            }
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

    /// Same checkmark + name + secondary-detail row style as
    /// `ModelPickerView.modelRow`, but the secondary line is the hardware
    /// compatibility label rather than a raw device list, and incompatible
    /// rows render at reduced opacity (`dimmed`) so the two tiers read as
    /// distinct at a glance even before considering the section they're in.
    private func modelRow(_ model: ModelInfo, dimmed: Bool) -> some View {
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

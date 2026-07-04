import SwiftUI
import TTStationKit

/// Searchable, family-grouped model browser for the menu-bar popover.
///
/// Replaces the old flat `Picker("Model")` while preserving its contract: a
/// model is always selectable and the choice flows straight into
/// `box.selectedModel`. Grouping/filtering is pure (`ModelDefaults`); this
/// view only renders and wires the tap.
///
/// A plain `TextField` (not `.searchable`) is used deliberately: `.searchable`
/// is unreliable inside a `MenuBarExtra` popover across macOS versions, so a
/// self-contained search field is the broadly-compatible choice. Verify the
/// look on a Mac.
struct ModelPickerView: View {
    @Bindable var box: BoxViewModel
    /// Max height of the scrollable model list. `nil` = uncapped (used in the
    /// resizable window); the default keeps the compact popover bounded.
    var maxListHeight: CGFloat? = 260
    @State private var query = ""

    /// Case-insensitive name filter, then family grouping — both pure.
    private var groups: [(family: String, models: [ModelInfo])] {
        let filtered = query.isEmpty
            ? box.models
            : box.models.filter { $0.name.localizedCaseInsensitiveContains(query) }
        return ModelDefaults.groupModelsByFamily(filtered)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
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

            if groups.isEmpty {
                Text(box.models.isEmpty
                     ? "No models available."
                     : "No models match \u{201C}\(query)\u{201D}.")
                    .font(.caption).foregroundStyle(.secondary)
                    .padding(.vertical, 4)
            } else if let cap = maxListHeight {
                // Popover: bounded, scrollable list keeps it compact even
                // with a long catalog.
                ScrollView { modelList }
                    .frame(maxHeight: cap)
            } else {
                // Window: no cap, so no inner ScrollView here — nesting a
                // second vertical ScrollView inside WindowRootView's outer
                // one collapses this list's height and defeats LazyVStack's
                // laziness. The outer ScrollView provides the scrolling;
                // this LazyVStack still renders lazily within it.
                modelList
            }
        }
    }

    /// Family-grouped, pinned-header model list. Shared by both the capped
    /// (popover, wrapped in its own `ScrollView`) and uncapped (window,
    /// scrolled by the caller's outer `ScrollView`) presentations so the
    /// group/section rendering isn't duplicated.
    private var modelList: some View {
        LazyVStack(alignment: .leading, spacing: 2, pinnedViews: [.sectionHeaders]) {
            ForEach(groups, id: \.family) { group in
                Section {
                    ForEach(group.models, id: \.name) { model in
                        modelRow(model)
                    }
                } header: {
                    Text(group.family)
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.vertical, 2)
                        .background(.background)
                }
            }
        }
    }

    private func modelRow(_ model: ModelInfo) -> some View {
        let isSelected = box.selectedModel == model.name
        return Button {
            box.selectedModel = model.name
        } label: {
            HStack(alignment: .top, spacing: 6) {
                Image(systemName: isSelected ? "checkmark.circle.fill" : "circle")
                    .font(.caption)
                    .foregroundStyle(isSelected ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(model.name).font(.caption)
                    if !model.devices.isEmpty {
                        // Device meshes this model can run on (e.g. "P300X2, T3K").
                        Text(model.devices.joined(separator: ", "))
                            .font(.caption2).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
            .padding(.vertical, 2).padding(.horizontal, 4)
        }
        .buttonStyle(.plain)
        .help("Select \(model.name)")
    }
}

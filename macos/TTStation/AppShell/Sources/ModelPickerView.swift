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
            } else {
                // Bounded, scrollable list keeps the popover compact even with
                // a long catalog. Pinned section headers keep the family label
                // visible while scrolling within a family.
                ScrollView {
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
                .frame(maxHeight: maxListHeight)
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

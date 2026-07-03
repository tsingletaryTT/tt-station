import SwiftUI
import TTStationKit

struct BoxRowView: View {
    let box: BoxViewModel
    let isSelected: Bool

    private var isServing: Bool { box.status?.isServing ?? false }

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(isServing ? .green : .gray).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(box.record.name).fontWeight(isSelected ? .semibold : .regular)
                Text(box.record.chips).font(.caption2).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
    }
}

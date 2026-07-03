import SwiftUI

struct ManualHostSheet: View {
    var onAdd: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var host = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add a box by address").font(.headline)
            TextField("host:port  (e.g. 192.168.5.119:8080)", text: $host)
                .textFieldStyle(.roundedBorder).frame(width: 280)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Add") { onAdd(host.trimmingCharacters(in: .whitespaces)); dismiss() }
                    .disabled(!host.contains(":"))
            }
        }
        .padding(16)
    }
}

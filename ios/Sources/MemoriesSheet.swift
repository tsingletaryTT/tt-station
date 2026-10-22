// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// What the assistant remembers about you: facts it saved when you told it
/// something worth keeping. Kept on your box; delete any of them here.
struct MemoriesSheet: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var memories: [MemoryItem] = []
    @State private var loading = true
    @State private var error: String?
    @State private var confirmClear = false

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Tell the assistant things like “remember I prefer metric units” and it will keep them in mind in every chat. It all stays on your box.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                }
                Section("Remembered") {
                    if loading && memories.isEmpty {
                        ProgressView().frame(maxWidth: .infinity)
                    } else if memories.isEmpty {
                        Text("Nothing yet.").foregroundStyle(.secondary)
                    }
                    ForEach(memories) { memory in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(memory.text)
                            Text(Date(timeIntervalSince1970: memory.added), format: .relative(presentation: .named))
                                .font(.tt(.caption))
                                .foregroundStyle(.secondary)
                        }
                    }
                    .onDelete { offsets in
                        let doomed = offsets.map { memories[$0] }
                        memories.remove(atOffsets: offsets)
                        Task {
                            for memory in doomed {
                                do { try await session.deleteMemory(id: memory.id) } catch { self.error = error.localizedDescription }
                            }
                        }
                    }
                }
                if let error {
                    Section { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange) }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            .navigationTitle("What I remember")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .destructiveAction) {
                    Button("Forget all", role: .destructive) { confirmClear = true }
                        .disabled(memories.isEmpty)
                }
            }
            .confirmationDialog("Forget everything the assistant remembers?", isPresented: $confirmClear,
                                titleVisibility: .visible) {
                Button("Forget all", role: .destructive) {
                    Task {
                        do {
                            try await session.clearMemories()
                            memories = []
                        } catch {
                            self.error = error.localizedDescription
                        }
                    }
                }
            }
            .task { await reload() }
            .refreshable { await reload() }
        }
        .presentationDetents([.medium, .large])
    }

    private func reload() async {
        loading = true
        defer { loading = false }
        do {
            memories = try await session.memories()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// Code → New: start a session in the box's sandbox, in a folder Code used
/// lately or one picked from the box's home folder. The sandbox keeps the agent
/// to that folder; it asks before commands unless told otherwise.
struct NewCodeSessionSheet: View {
    let session: BoxSession
    let recent: [String]
    let onStarted: (CodeSession) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var overview: CodeOverview?
    @State private var folder: String?
    @State private var policy = "ask"
    @State private var think = false
    @State private var starting = false
    @State private var problem: String?

    var body: some View {
        NavigationStack {
            List {
                if let sandbox = overview?.sandbox, !sandbox.ready {
                    Section {
                        Label(sandbox.error ?? sandbox.step.map { "Setting up the sandbox: \($0)" }
                              ?? "The sandbox isn't set up on the box yet. Set it up once from TT Station on your computer (Code).",
                              systemImage: "shippingbox")
                            .font(.tt(.footnote))
                    }
                }
                if !recent.isEmpty {
                    Section("Recent folders") {
                        ForEach(recent, id: \.self) { path in
                            folderRow(path: path, title: RecentFolders.short(path))
                        }
                    }
                }
                Section("On the box") {
                    NavigationLink {
                        FolderBrowser(session: session, path: nil) { picked in folder = picked }
                    } label: {
                        Label(folder.map(RecentFolders.short) ?? "Choose a project folder…", systemImage: "folder")
                    }
                }
                Section {
                    Picker("Without asking, it may", selection: $policy) {
                        Text("Nothing (ask first)").tag("ask")
                        Text("Edit files").tag("edits")
                        Text("Anything in the sandbox").tag("auto")
                    }
                    Toggle("Think before answering", isOn: $think)
                } footer: {
                    Text("It works in the box's sandbox: only that folder, no access to the rest of the box. You answer its questions from here or from the notification.")
                }
                if let problem {
                    Section { Text(problem).foregroundStyle(.orange) }
                }
            }
            .navigationTitle("New Code session")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        Task { await start() }
                    } label: {
                        if starting { ProgressView() } else { Text("Start") }
                    }
                    .disabled(folder == nil || starting || overview?.sandbox?.ready == false)
                }
            }
            .task {
                overview = try? await session.codeOverview()
                if folder == nil { folder = recent.first }
            }
        }
    }

    private func folderRow(path: String, title: String) -> some View {
        Button {
            folder = path
        } label: {
            HStack {
                Text(title).font(.ttMono(13)).foregroundStyle(.primary).lineLimit(1)
                Spacer(minLength: 0)
                if folder == path { Image(systemName: "checkmark").foregroundStyle(Brand.accent) }
            }
        }
    }

    private func start() async {
        guard let folder else { return }
        starting = true
        defer { starting = false }
        do {
            let started = try await session.startCode(NewCodeSession(folder: folder, sandbox: true, policy: policy, think: think))
            dismiss()
            onStarted(started)
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// The box's folders, one level at a time, from its account's home.
private struct FolderBrowser: View {
    let session: BoxSession
    let path: String?
    let onPick: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var listing: CodingFolders?
    @State private var problem: String?

    var body: some View {
        List {
            if let listing {
                Section {
                    Button {
                        onPick(listing.path)
                        dismiss()
                    } label: {
                        Label("Use \(RecentFolders.short(listing.path))", systemImage: listing.git ? "checkmark.circle" : "folder.badge.plus")
                    }
                } footer: {
                    if !listing.git { Text("Not a git project. Code works best in one project folder.") }
                }
                Section {
                    ForEach(listing.folders) { f in
                        NavigationLink {
                            FolderBrowser(session: session, path: f.path, onPick: onPick)
                        } label: {
                            Label(f.name, systemImage: f.git ? "chevron.left.forwardslash.chevron.right" : "folder")
                        }
                    }
                }
            } else if let problem {
                Text(problem).foregroundStyle(.orange)
            } else {
                ProgressView()
            }
        }
        .navigationTitle(listing.map { ($0.path as NSString).lastPathComponent } ?? "Folders")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            do { listing = try await session.codingFolders(path: path) } catch { problem = error.localizedDescription }
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import PDFKit
import SwiftUI
import UniformTypeIdentifiers

/// Your documents on the box: upload PDFs and text files, then ask the assistant
/// about them. The phone extracts the text (PDFKit); the box stores and searches
/// it -- nothing leaves your network.
struct DocumentsSheet: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var documents: [DocSummary] = []
    @State private var loading = true
    @State private var importing = false
    @State private var uploading: String?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Label {
                        Text("Upload PDFs or text, then ask things like “what does my lease say about pets?” The assistant searches them on the box and cites the document.")
                            .font(.tt(.footnote))
                            .foregroundStyle(.secondary)
                    } icon: {
                        Image(systemName: "lock.shield").foregroundStyle(Brand.accent)
                    }
                }
                if let uploading {
                    Section {
                        HStack(spacing: 12) {
                            ProgressView()
                            Text("Reading \(uploading)…").foregroundStyle(.secondary)
                        }
                    }
                }
                if let error {
                    Section {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                            .font(.tt(.footnote))
                    }
                }
                Section("On \(session.box.name)") {
                    if loading && documents.isEmpty {
                        ProgressView().frame(maxWidth: .infinity)
                    } else if documents.isEmpty {
                        Text("No documents yet.").foregroundStyle(.secondary)
                    }
                    ForEach(documents) { doc in
                        DocumentRow(doc: doc)
                    }
                    .onDelete { offsets in
                        let doomed = offsets.map { documents[$0] }
                        documents.remove(atOffsets: offsets)
                        Task {
                            for doc in doomed {
                                do { try await session.deleteDocument(id: doc.id) } catch { self.error = error.localizedDescription }
                            }
                        }
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            .navigationTitle("Documents")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    Button { importing = true } label: { Label("Add", systemImage: "plus") }
                        .disabled(uploading != nil)
                }
            }
            .fileImporter(isPresented: $importing, allowedContentTypes: DocumentText.supportedTypes,
                          allowsMultipleSelection: true) { result in
                switch result {
                case .success(let urls): Task { await upload(urls) }
                case .failure(let failure): error = failure.localizedDescription
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
            documents = try await session.documents()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func upload(_ urls: [URL]) async {
        error = nil
        for url in urls {
            let name = url.lastPathComponent
            uploading = name
            do {
                let text = try await Task.detached(priority: .userInitiated) { try DocumentText.extract(from: url) }.value
                let summary = try await session.addDocument(name: name, text: text)
                documents.removeAll { $0.name == summary.name }
                documents.insert(summary, at: 0)
            } catch {
                self.error = "\(name): \(error.localizedDescription)"
            }
        }
        uploading = nil
    }
}

private struct DocumentRow: View {
    let doc: DocSummary

    private var icon: String {
        doc.name.lowercased().hasSuffix(".pdf") ? "doc.richtext" : "doc.plaintext"
    }

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: icon)
                .font(.tt(.title3))
                .foregroundStyle(Brand.accent)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(doc.name).lineLimit(1)
                Text("\(Self.size(doc.chars)) · added \(Date(timeIntervalSince1970: doc.added), format: .relative(presentation: .named))")
                    .font(.tt(.caption))
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// Characters as an approximate page count, which means more than a number.
    static func size(_ chars: Int) -> String {
        let pages = max(1, Int((Double(chars) / 3000).rounded()))
        return pages == 1 ? "~1 page" : "~\(pages) pages"
    }
}

/// Plain text out of the files people actually have.
enum DocumentText {
    static let supportedTypes: [UTType] = [.pdf, .plainText, .utf8PlainText, .rtf, .html, .commaSeparatedText, .json]
        + [UTType(filenameExtension: "md"), UTType(filenameExtension: "markdown")].compactMap { $0 }

    enum ExtractError: LocalizedError {
        case unreadable, empty

        var errorDescription: String? {
            switch self {
            case .unreadable: "Couldn't open that file."
            case .empty: "No text found (a scanned PDF is just pictures of pages)."
            }
        }
    }

    static func extract(from url: URL) throws -> String {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let type = UTType(filenameExtension: url.pathExtension) ?? .plainText
        let text: String
        if type.conforms(to: .pdf) {
            guard let pdf = PDFDocument(url: url) else { throw ExtractError.unreadable }
            // Page by page, with breaks between pages so passages split sensibly.
            text = (0..<pdf.pageCount).compactMap { pdf.page(at: $0)?.string }.joined(separator: "\n\n")
        } else if type.conforms(to: .rtf) || type.conforms(to: .html) {
            let kind: NSAttributedString.DocumentType = type.conforms(to: .rtf) ? .rtf : .html
            guard let attributed = try? NSAttributedString(url: url, options: [.documentType: kind],
                                                           documentAttributes: nil)
            else { throw ExtractError.unreadable }
            text = attributed.string
        } else {
            guard let data = try? Data(contentsOf: url) else { throw ExtractError.unreadable }
            text = String(decoding: data, as: UTF8.self)
        }
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw ExtractError.empty }
        return text
    }
}

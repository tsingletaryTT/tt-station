import CoreImage.CIFilterBuiltins
import SwiftUI
import UIKit

/// Use the box from other apps: its OpenAI-compatible endpoint, per-app keys
/// (minted here, shown once, revocable), and copy-ready setup for popular tools.
struct ConnectAppsView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var info: ConnectInfo?
    @State private var error: String?
    @State private var newKeyName = ""
    @State private var minted: (name: String, key: String)?
    @State private var working = false

    private var baseURL: String {
        info?.urls.first(where: { $0.label == "Home network" })?.url ?? info?.urls.first?.url ?? "http://<box>:8765/v1"
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Anything that speaks the OpenAI API can use your box: it answers with whatever model is serving, and images go to the image model. Nothing leaves your network.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                }

                if let minted {
                    Section("Your new key for \(minted.name)") {
                        CopyRow(label: "API key", value: minted.key, monospaced: true)
                        Text("Copy it now: it isn't shown again. Revoke it here any time.")
                            .font(.tt(.caption))
                            .foregroundStyle(.orange)
                        RecipeList(baseURL: baseURL, key: minted.key, chatModel: chatModel)
                    }
                }

                HouseholdSection(session: session)

                Section("Endpoint") {
                    if let info {
                        ForEach(info.urls, id: \.self) { address in
                            CopyRow(label: address.label, value: address.url, monospaced: true)
                        }
                    } else if error == nil {
                        ProgressView()
                    }
                }

                if let models = info?.models {
                    Section("Serving now") {
                        if models.isEmpty {
                            Text("Nothing is serving. Start a model and apps will use it.")
                                .foregroundStyle(.secondary)
                        }
                        ForEach(models, id: \.self) { model in
                            Label(model.id, systemImage: model.kind == "image" ? "photo" : "text.bubble")
                                .font(.ttMono(15, relativeTo: .subheadline))
                        }
                    }
                }

                Section {
                    HStack {
                        TextField("App name (e.g. Open WebUI)", text: $newKeyName)
                            .textInputAutocapitalization(.words)
                            .submitLabel(.done)
                            .onSubmit { Task { await mint() } }
                        Button("Create") { Task { await mint() } }
                            .disabled(newKeyName.trimmingCharacters(in: .whitespaces).isEmpty || working)
                    }
                } header: {
                    Text("New app key")
                } footer: {
                    Text("One key per app, so you can cut off one without touching the others.")
                }

                if let keys = info?.keys, !keys.isEmpty {
                    Section("App keys") {
                        ForEach(keys) { key in
                            VStack(alignment: .leading, spacing: 2) {
                                Text(key.name).font(.tt(.headline))
                                Text("\(key.hint) · \(key.requests) requests · \(lastUsed(key))")
                                    .font(.tt(.caption))
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .onDelete { offsets in
                            let doomed = offsets.map { keys[$0] }
                            Task {
                                for key in doomed {
                                    do { try await session.revokeAppKey(id: key.id) } catch { self.error = error.localizedDescription }
                                }
                                await reload()
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
            .navigationTitle("Connect apps")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task { await reload() }
            .refreshable { await reload() }
        }
    }

    private var chatModel: String {
        info?.models.first(where: { $0.kind == "chat" })?.id ?? "any"
    }

    private func lastUsed(_ key: AppKeyInfo) -> String {
        guard let used = key.lastUsed else { return "never used" }
        return "used " + Date(timeIntervalSince1970: used).formatted(.relative(presentation: .named))
    }

    private func reload() async {
        do {
            info = try await session.connectInfo()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func mint() async {
        let name = newKeyName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        working = true
        defer { working = false }
        do {
            let (_, key) = try await session.createAppKey(name: name)
            minted = (name, key)
            newKeyName = ""
            await reload()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct CopyRow: View {
    let label: String
    let value: String
    var monospaced = false
    @State private var copied = false

    var body: some View {
        Button {
            UIPasteboard.general.string = value
            copied = true
            Task {
                try? await Task.sleep(for: .seconds(1.5))
                copied = false
            }
        } label: {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(label).font(.tt(.caption)).foregroundStyle(.secondary)
                    Text(value)
                        .font(monospaced ? .ttMono(13, relativeTo: .footnote) : .tt(.footnote))
                        .lineLimit(2)
                        .truncationMode(.middle)
                        .foregroundStyle(.primary)
                }
                Spacer()
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .foregroundStyle(copied ? .green : Brand.accent)
            }
        }
    }
}

/// Copy-ready setup for the tools people actually use.
private struct RecipeList: View {
    let baseURL: String
    let key: String
    let chatModel: String

    private var recipes: [(String, String)] {
        [
            ("Open WebUI", "Settings → Connections → OpenAI API\nURL: \(baseURL)\nKey: \(key)"),
            ("Environment (openai SDKs, LangChain, LlamaIndex)", "export OPENAI_BASE_URL=\(baseURL)\nexport OPENAI_API_KEY=\(key)"),
            ("Python", """
            from openai import OpenAI
            client = OpenAI(base_url="\(baseURL)", api_key="\(key)")
            reply = client.chat.completions.create(
                model="\(chatModel)",
                messages=[{"role": "user", "content": "Hello from my QuietBox"}])
            print(reply.choices[0].message.content)
            """),
            ("curl", """
            curl \(baseURL)/chat/completions \\
              -H "Authorization: Bearer \(key)" -H "Content-Type: application/json" \\
              -d '{"model":"\(chatModel)","messages":[{"role":"user","content":"Hi"}]}'
            """),
            ("Continue (VS Code)", """
            models:
              - name: QuietBox
                provider: openai
                model: \(chatModel)
                apiBase: \(baseURL)
                apiKey: \(key)
            """),
        ]
    }

    var body: some View {
        ForEach(Array(recipes.enumerated()), id: \.offset) { _, recipe in
            DisclosureGroup(recipe.0) {
                CopyRow(label: "Copy", value: recipe.1, monospaced: true)
            }
        }
    }
}

/// Browser chat for the whole household: `tt launch` runs Open WebUI or
/// AnythingLLM on the box, wired to the chat model. Anyone on the network opens
/// the address (or scans the QR) -- no app, no pairing.
private struct HouseholdSection: View {
    let session: BoxSession
    @State private var clients: [WebClient] = []
    @State private var working: String?
    @State private var error: String?

    var body: some View {
        Section {
            ForEach(clients) { client in
                VStack(alignment: .leading, spacing: 10) {
                    HStack {
                        Text(client.name).font(.tt(.headline))
                        Spacer()
                        if working == client.tool {
                            ProgressView()
                        } else if client.running {
                            Button("Stop") { Task { await stop(client) } }
                        } else {
                            Button("Start") { Task { await start(client) } }
                                .disabled(!client.available || !session.canChat)
                        }
                    }
                    if let url = client.url {
                        HStack(alignment: .center, spacing: 14) {
                            if let qr = QRCode.image(url) {
                                Image(uiImage: qr).interpolation(.none).resizable().frame(width: 96, height: 96)
                            }
                            VStack(alignment: .leading, spacing: 6) {
                                Text(url).font(.ttMono(13, relativeTo: .footnote)).textSelection(.enabled)
                                Button {
                                    UIPasteboard.general.string = url
                                } label: { Label("Copy address", systemImage: "doc.on.doc").font(.tt(.footnote)) }
                                if let link = URL(string: url) {
                                    Link(destination: link) { Label("Open", systemImage: "safari").font(.tt(.footnote)) }
                                }
                            }
                        }
                    }
                }
            }
            if let error { Text(error).font(.tt(.footnote)).foregroundStyle(.orange) }
        } header: {
            Text("For everyone at home")
        } footer: {
            Text(session.canChat
                 ? "Runs a chat website on your box. Anyone on your network can open it in a browser; the first person to sign up becomes its admin."
                 : "Start a chat model first; these websites chat with it.")
        }
        .task { await load() }
    }

    private func load() async {
        clients = (try? await session.webClients()) ?? clients
    }

    private func start(_ client: WebClient) async {
        working = client.tool
        defer { working = nil }
        do { _ = try await session.startWebClient(client.tool); error = nil } catch { self.error = error.localizedDescription }
        await load()
    }

    private func stop(_ client: WebClient) async {
        working = client.tool
        defer { working = nil }
        do { try await session.stopWebClient(client.tool); error = nil } catch { self.error = error.localizedDescription }
        await load()
    }
}

/// A QR code image for a string.
enum QRCode {
    static func image(_ text: String) -> UIImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage?.transformed(by: CGAffineTransform(scaleX: 8, y: 8)),
              let cg = CIContext().createCGImage(output, from: output.extent) else { return nil }
        return UIImage(cgImage: cg)
    }
}

import SwiftUI
import UIKit

// MARK: - Modes

/// "What do you want to do?" -- Chat, Images, Chat + images, Video, Transcribe.
/// Each card names what it runs, how long it usually takes to load here, and
/// what to download first; tap to switch.
struct ModesStrip: View {
    let session: BoxSession
    @State private var modes: [BoxMode] = []
    @State private var confirming: BoxMode?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if !modes.isEmpty {
                SectionTitle(text: "What do you want to do?")
                ScrollView(.horizontal) {
                    HStack(spacing: 10) {
                        ForEach(modes) { mode in
                            Button { confirming = mode } label: { card(mode) }
                                .buttonStyle(.plain)
                                .disabled(mode.active || session.startingModel != nil)
                        }
                    }
                }
                .scrollIndicators(.hidden)
            }
            if let error {
                Text(error).font(.tt(.footnote)).foregroundStyle(.orange)
            }
        }
        .task(id: session.serve?.phase) { modes = (try? await session.modes()) ?? modes }
        .confirmationDialog(confirming.map { "Switch to \($0.title)?" } ?? "", isPresented: Binding(
            get: { confirming != nil }, set: { if !$0 { confirming = nil } }), titleVisibility: .visible,
            presenting: confirming) { mode in
            if mode.ready {
                Button("Switch to \(mode.title)") {
                    Task {
                        do { try await session.startMode(mode); error = nil } catch { self.error = error.localizedDescription }
                    }
                }
            }
        } message: { mode in
            Text(message(mode))
        }
    }

    private func card(_ mode: BoxMode) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Image(systemName: mode.icon).font(.tt(.title3)).foregroundStyle(Brand.accent)
                Spacer()
                if mode.active {
                    Text("Now").font(.tt(.caption2).weight(.bold)).padding(.horizontal, 6).padding(.vertical, 2)
                        .background(Brand.accent, in: Capsule())
                } else if !mode.ready {
                    Image(systemName: "arrow.down.circle").foregroundStyle(.secondary)
                }
            }
            Text(mode.title).font(.tt(.headline))
            Text(mode.models.map(Brand.shortModel).joined(separator: " + "))
                .font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
            if let eta = mode.etaS, !mode.active {
                Text("Loads in ~\(max(1, eta / 60)) min").font(.tt(.caption2)).foregroundStyle(.tertiary)
            }
        }
        .frame(width: 150, height: 120, alignment: .topLeading)
        .padding(14)
        .card(tint: mode.active ? Brand.accent.opacity(0.35) : nil, radius: 20)
    }

    private func message(_ mode: BoxMode) -> String {
        if !mode.ready {
            return "Download \(mode.needsDownload.map(Brand.shortModel).joined(separator: " and ")) first (below)."
        }
        var parts = [mode.blurb]
        if let note = mode.note { parts.append(note) }
        if let current = session.serve?.displayModel, session.isServing {
            parts.append("This replaces \(current).")
        }
        parts.append(mode.etaS.map { "It usually takes about \(max(1, $0 / 60)) min to load." } ?? "Loading takes a few minutes.")
        return parts.joined(separator: " ")
    }
}

// MARK: - Box Doctor

/// The Box tab's entry: the Doctor's verdict at a glance.
/// The Box tab's health line: Box Doctor's verdict at a glance; tap for the report.
struct DoctorCard: View {
    let session: BoxSession
    @State private var report: DoctorReport?
    @State private var showReport = false

    var body: some View {
        Button { showReport = true } label: {
            HStack(spacing: 12) {
                Image(systemName: icon).font(.tt(.title3)).foregroundStyle(tint)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Box Doctor").font(.tt(.headline))
                    Text(summary).font(.tt(.caption)).foregroundStyle(.secondary)
                        .multilineTextAlignment(.leading).lineLimit(2)
                }
                Spacer(minLength: 0)
                Image(systemName: "chevron.right").foregroundStyle(.tertiary)
            }
            .padding(16)
            .card()
        }
        .buttonStyle(.plain)
        .task(id: session.box.id) { report = try? await session.doctor() }
        .sheet(isPresented: $showReport) { DoctorView(session: session, report: $report) }
    }

    private var icon: String {
        switch report?.status {
        case "fail": return "cross.case.fill"
        case "warn": return "exclamationmark.triangle.fill"
        case nil: return "stethoscope"
        default: return "checkmark.seal.fill"
        }
    }

    private var tint: Color {
        switch report?.status {
        case "fail": return .red
        case "warn": return .orange
        case nil: return Brand.accent
        default: return .green
        }
    }

    private var summary: String {
        guard let report else { return "Checking chips, cooling, disk, memory and the model…" }
        let problems = report.checks.filter { $0.status == "warn" || $0.status == "fail" }
        return problems.isEmpty ? "Everything looks healthy." : problems.map(\.title).joined(separator: " · ")
    }
}

struct DoctorView: View {
    let session: BoxSession
    @Binding var report: DoctorReport?
    @Environment(\.dismiss) private var dismiss
    @State private var working: String?
    @State private var error: String?
    @State private var bundle: URL?

    var body: some View {
        NavigationStack {
            List {
                if let report {
                    ForEach(report.checks, id: \.self) { check in
                        Section {
                            HStack(alignment: .top, spacing: 12) {
                                Image(systemName: symbol(check.status)).foregroundStyle(color(check.status))
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(check.title).font(.tt(.headline))
                                    Text(check.detail).font(.tt(.subheadline)).foregroundStyle(.secondary)
                                }
                            }
                            if let fix = check.fix {
                                Button {
                                    Task { await apply(fix) }
                                } label: {
                                    HStack {
                                        if working == fix { ProgressView() }
                                        Text(fixTitle(fix))
                                    }
                                }
                                .disabled(working != nil || !mayApply(fix))
                                if !mayApply(fix) {
                                    Text(Self.ownersOnly.contains(fix) ? "Only an owner can do this."
                                         : "Only owners can start or stop models on this box.")
                                        .font(.tt(.caption)).foregroundStyle(.secondary)
                                }
                            }
                            if let command = check.command {
                                Button {
                                    UIPasteboard.general.string = command
                                } label: {
                                    Label(command, systemImage: "doc.on.doc").font(.ttMono(13, relativeTo: .footnote))
                                }
                            }
                        }
                    }
                } else {
                    ProgressView("Examining the box…")
                }
                Section {
                    Button {
                        Task { await makeBundle() }
                    } label: {
                        HStack {
                            if working == "bundle" { ProgressView() }
                            Label("Share diagnostics with support", systemImage: "square.and.arrow.up")
                        }
                    }
                    .disabled(working != nil || report?.owner == false)
                } footer: {
                    Text(report?.owner == false
                         ? "An owner can make a bundle of the box's logs for support."
                         : "A bundle of the box's logs, versions and this report (passwords and tokens removed) that you can send to Tenstorrent.")
                }
                if let error {
                    Section { Text(error).foregroundStyle(.orange) }
                }
            }
            .navigationTitle("Box Doctor")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    Button { Task { await refresh() } } label: { Image(systemName: "arrow.clockwise") }
                }
            }
            .task { await refresh() }
            .sheet(item: Binding(get: { bundle.map(SharedFile.init) }, set: { if $0 == nil { bundle = nil } })) { file in
                ActivitySheet(items: [file.url])
            }
        }
    }

    private func refresh() async {
        do { report = try await session.doctor(); error = nil } catch { self.error = error.localizedDescription }
    }

    private func apply(_ fix: String) async {
        working = fix
        defer { working = nil }
        do { report = try await session.applyFix(fix); error = nil } catch { self.error = error.localizedDescription }
    }

    private func makeBundle() async {
        working = "bundle"
        defer { working = nil }
        do {
            let (name, data) = try await session.diagnostics()
            let url = FileManager.default.temporaryDirectory.appendingPathComponent(name)
            try data.write(to: url)
            bundle = url
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// The fixes that are an owner's (the box refuses members too).
    private static let ownersOnly: Set<String> = ["reset_chips", "stop_stray_containers"]

    private func mayApply(_ fix: String) -> Bool {
        if Self.ownersOnly.contains(fix) { return report?.owner ?? true }
        if fix == "restart_model" { return session.access?.mayControlModels ?? true }
        return true
    }

    private func fixTitle(_ fix: String) -> String {
        ["restart_model": "Restart the model", "stop_stray_containers": "Stop the leftover servers",
         "reset_chips": "Reset the chips"][fix] ?? fix
    }

    private func symbol(_ status: String) -> String {
        ["ok": "checkmark.circle.fill", "info": "info.circle.fill", "warn": "exclamationmark.triangle.fill",
         "fail": "xmark.octagon.fill"][status] ?? "circle"
    }

    private func color(_ status: String) -> Color {
        ["ok": .green, "info": .blue, "warn": .orange, "fail": .red][status] ?? .secondary
    }
}

private struct SharedFile: Identifiable {
    let url: URL
    var id: String { url.path }
}

struct ActivitySheet: UIViewControllerRepresentable {
    let items: [Any]
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}

// MARK: - Software

struct SoftwareView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var report: SoftwareReport?
    @State private var error: String?
    @State private var updating = false

    var body: some View {
        NavigationStack {
            List {
                if let report {
                    Section("tt (Tenstorrent CLI)") {
                        LabeledContent("Installed", value: report.tt.current ?? "?")
                        if report.tt.newer, let latest = report.tt.latest {
                            Button {
                                Task { await updateTt() }
                            } label: {
                                HStack {
                                    if updating { ProgressView() }
                                    Text("Update to \(latest)")
                                }
                            }
                            .disabled(updating)
                        } else {
                            Label("Up to date", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                        }
                    }
                    Section("Tools tt manages") {
                        ForEach(report.tools, id: \.self) { tool in
                            LabeledContent(tool.name) {
                                HStack(spacing: 6) {
                                    Text(tool.installed).monospacedDigit()
                                    Image(systemName: tool.upToDate ? "checkmark.circle.fill" : "arrow.up.circle.fill")
                                        .foregroundStyle(tool.upToDate ? .green : .orange)
                                }
                            }
                        }
                    }
                    if let system = report.system {
                        Section {
                            if let kmd = system.installedKmd { LabeledContent("Driver (KMD)", value: kmd) }
                            Text(system.advice).font(.tt(.footnote))
                                .foregroundStyle(system.downgrade ? .orange : .secondary)
                        } header: {
                            Text("Driver, firmware and system")
                        } footer: {
                            Text("Tenstorrent's tested set: \(system.target)")
                        }
                    }
                    if !report.leftovers.isEmpty {
                        Section {
                            ForEach(report.leftovers, id: \.self) { item in
                                LabeledContent(item.path, value: ByteFormat.string(item.bytes))
                                    .font(.tt(.footnote))
                            }
                            Button {
                                UIPasteboard.general.string = "rm -rf " + report.leftovers.map(\.path).joined(separator: " ")
                            } label: {
                                Label("Copy the command to delete them", systemImage: "doc.on.doc")
                            }
                        } header: {
                            Text("Old installs tt no longer uses")
                        }
                    }
                } else if error == nil {
                    ProgressView("Checking versions…")
                }
                if let error {
                    Section { Text(error).foregroundStyle(.orange) }
                }
            }
            .navigationTitle("Software")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task { await load() }
        }
    }

    private func load() async {
        do { report = try await session.software(); error = nil } catch { self.error = error.localizedDescription }
    }

    private func updateTt() async {
        updating = true
        defer { updating = false }
        do { try await session.updateTt(); await load() } catch { self.error = error.localizedDescription }
    }
}

/// Box Doctor as a standalone sheet (from Settings): loads its own report.
struct DoctorSheet: View {
    let session: BoxSession
    @State private var report: DoctorReport?

    var body: some View {
        DoctorView(session: session, report: $report)
            .task { report = try? await session.doctor() }
    }
}

import SwiftUI

// MARK: - Onboarding (no paired box yet)

/// First run: find a box on the network and pair with it by scanning its QR.
struct WelcomeView: View {
    @Environment(Discovery.self) private var discovery
    @State private var pairing: PairTarget?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 28) {
                    VStack(spacing: 16) {
                        TTMark()
                            .fill(Brand.accent.gradient)
                            .frame(width: 72, height: 72)
                            .shadow(color: Brand.accent.opacity(0.6), radius: 30)
                        TTLogo(height: 20)
                        Text("Talk to your Tenstorrent box from your phone.")
                            .font(.tt(.title3).weight(.semibold))
                            .multilineTextAlignment(.center)
                    }
                    .padding(.top, 40)

                    VStack(spacing: 12) {
                        SectionTitle(text: discovery.boxes.isEmpty ? "Looking nearby" : "Found nearby")
                        if discovery.boxes.isEmpty {
                            HStack(spacing: 12) {
                                ProgressView()
                                Text("Searching this network for boxes running tt-station-agentd…")
                                    .font(.tt(.subheadline))
                                    .foregroundStyle(.secondary)
                                Spacer()
                            }
                            .padding(18)
                            .card()
                        }
                        ForEach(discovery.boxes) { found in
                            NearbyRow(box: found) {
                                pairing = PairTarget(name: found.name, host: found.host, port: found.port)
                            }
                        }
                    }

                    HStack(alignment: .top, spacing: 14) {
                        Image(systemName: "qrcode.viewfinder")
                            .font(.tt(.title))
                            .foregroundStyle(Brand.accent)
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Scan the pairing code").font(.tt(.headline))
                            Text("Open TT Station on the box (**Settings → Devices and pairing**), then point this iPhone's Camera at the code.")
                                .font(.tt(.subheadline))
                                .foregroundStyle(.secondary)
                        }
                    }
                    .padding(18)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .card(tint: Brand.accent.opacity(0.2))

                    VStack(alignment: .leading, spacing: 12) {
                        Step(number: 1, text: "Run tt-station-agentd on your Tenstorrent box (a QuietBox, a Galaxy server, a workstation with Tenstorrent cards).")
                        Step(number: 2, text: "Keep this phone on the same Wi-Fi as the box.")
                        Step(number: 3, text: "Scan the pairing code TT Station shows on the box.")
                    }
                    .padding(18)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .card()
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 40)
            }
            .scrollIndicators(.hidden)
            .background(Brand.backdrop)
            .sheet(item: $pairing) { PairSheet(target: $0) }
            .onAppear { discovery.start() }
            .onDisappear { discovery.stop() }
        }
    }

    private struct Step: View {
        let number: Int
        let text: String
        var body: some View {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text("\(number)")
                    .font(.tt(.caption).weight(.bold))
                    .frame(width: 22, height: 22)
                    .glassEffect(.regular.tint(Brand.accent.opacity(0.5)), in: .circle)
                Text(text).font(.tt(.subheadline))
            }
        }
    }
}

// MARK: - Boxes tab

/// Paired boxes (tap to make one active), boxes nearby, and how to add one
/// (scan its pairing QR). Lives in Settings → Boxes.
struct BoxesList: View {
    @Environment(AppModel.self) private var app
    @Environment(Discovery.self) private var discovery
    @State private var pairing: PairTarget?
    @State private var showAdd = false

    private var nearby: [DiscoveredBox] {
        discovery.boxes.filter { found in
            !app.store.boxes.contains { $0.id == found.id || $0.name == found.name }
        }
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 14) {
                SectionTitle(text: "Your boxes")
                ForEach(app.store.boxes) { box in
                    BoxCard(box: box, isActive: app.activeBox?.id == box.id) {
                        app.activate(box)
                        app.selectedTab = .box
                    }
                    .contextMenu {
                        Button("Forget this box", systemImage: "trash", role: .destructive) {
                            app.forget(box)
                        }
                    }
                }

                if !nearby.isEmpty {
                    SectionTitle(text: "Nearby").padding(.top, 14)
                    ForEach(nearby) { found in
                        NearbyRow(box: found) {
                            pairing = PairTarget(name: found.name, host: found.host, port: found.port)
                        }
                    }
                }

                Text("Long-press a box to forget it.")
                    .font(.tt(.footnote))
                    .foregroundStyle(.tertiary)
                    .padding(.top, 8)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
        }
        .scrollIndicators(.hidden)
        .background(Brand.backdrop)
        .navigationTitle("Boxes")
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button { showAdd = true } label: { Image(systemName: "plus") }
                    .accessibilityLabel("Add a box")
            }
        }
        .refreshable { discovery.start() }
        .sheet(item: $pairing) { PairSheet(target: $0) }
        .sheet(isPresented: $showAdd) { PairSheet(target: nil) }
        .onAppear { discovery.start() }
        .onDisappear { discovery.stop() }
    }
}

/// A paired box with its live serving state, fetched when the card appears.
private struct BoxCard: View {
    let box: SavedBox
    let isActive: Bool
    let onSelect: () -> Void
    @State private var status: BoxStatus?
    @State private var unreachable = false

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 16) {
                TTMark()
                    .fill(Brand.accent)
                    .padding(11)
                    .frame(width: 52, height: 52)
                    .glassEffect(.regular.tint(Brand.accent.opacity(0.3)), in: .rect(cornerRadius: 14))

                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 8) {
                        Text(box.name).font(.tt(.headline))
                        if let mesh = status?.deviceMesh {
                            Text(mesh.uppercased())
                                .font(.ttMono(11, .semibold, relativeTo: .caption2))
                                .foregroundStyle(.secondary)
                        }
                    }
                    pill
                }
                Spacer()
                if isActive {
                    Image(systemName: "checkmark.circle.fill")
                        .font(.tt(.title3))
                        .foregroundStyle(Brand.accent)
                        .accessibilityLabel("Active")
                }
            }
            .padding(16)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .card(tint: isActive ? Brand.accent.opacity(0.12) : nil, interactive: true)
        .task { await refresh() }
    }

    @ViewBuilder private var pill: some View {
        if box.pin == nil {
            StatusPill(state: .offline, text: "Pair again to connect securely")
        } else if unreachable {
            StatusPill(state: .offline, text: "Can't reach \(box.host)")
        } else if let status {
            if let model = status.servingModel {
                StatusPill(state: .serving, text: Brand.shortModel(model))
            } else {
                StatusPill(state: .idle, text: "Nothing serving")
            }
        } else {
            StatusPill(state: .checking, text: "Checking…")
        }
    }

    private func refresh() async {
        // No pinned certificate: never probe over plain HTTP.
        guard box.pin != nil else { return }
        do {
            status = try await AgentClient(box: box, token: nil).status()
            unreachable = false
        } catch {
            unreachable = true
        }
    }
}

struct NearbyRow: View {
    let box: DiscoveredBox
    let onPair: () -> Void

    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: "dot.radiowaves.left.and.right")
                .font(.tt(.title3))
                .foregroundStyle(Brand.accent)
                .frame(width: 36)
            VStack(alignment: .leading, spacing: 3) {
                Text(box.name).font(.tt(.headline))
                Text(box.id).font(.ttMono(12, relativeTo: .caption)).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Pair", action: onPair)
                .buttonStyle(.glassProminent)
        }
        .padding(14)
        .card()
    }
}

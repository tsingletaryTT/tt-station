// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI
import UIKit

@main
struct TTStationMobileApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var app = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    init() {
        // Navigation titles in Vesper's Inter Tight.
        // Only the fonts: the bars keep iOS's own Liquid Glass.
        if let large = UIFont(name: "InterTight-Medium", size: 34),
           let inline = UIFont(name: "InterTight-SemiBold", size: 17) {
            UINavigationBar.appearance().largeTitleTextAttributes =
                [.font: UIFontMetrics(forTextStyle: .largeTitle).scaledFont(for: large)]
            UINavigationBar.appearance().titleTextAttributes =
                [.font: UIFontMetrics(forTextStyle: .headline).scaledFont(for: inline)]
        }
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(app)
                .environment(app.store)
                .environment(app.discovery)
                .tint(Brand.accentFill)
                .font(.tt(.body))
                .preferredColorScheme(.dark)
                // QR pairing: the Camera app opens `ttstation://pair?...`.
                .onOpenURL { url in Task { await app.handle(url: url) } }
                .overlay(alignment: .top) {
                    if let event = app.alerts.banner {
                        AlertBanner(event: event) { app.alerts.dismissBanner() }
                            .transition(.move(edge: .top).combined(with: .opacity))
                    }
                }
                .animation(.snappy, value: app.alerts.banner)
                .onChange(of: scenePhase, initial: true) { _, phase in
                    switch phase {
                    case .active: app.alerts.start(app)
                    case .background:
                        app.alerts.stop()
                        AlertCenter.scheduleRefresh()
                    default: break
                    }
                }
                .alert("Couldn't pair", isPresented: Binding(
                    get: { app.linkError != nil },
                    set: { if !$0 { app.linkError = nil } })
                ) {
                    Button("OK", role: .cancel) {}
                } message: {
                    Text(app.linkError ?? "")
                }
        }
        .backgroundTask(.appRefresh(AlertCenter.refreshTaskID)) {
            await AlertCenter.backgroundCheck()
        }
    }
}

/// Onboarding until a box is paired; then the four Liquid Glass tabs.
struct RootView: View {
    @Environment(AppModel.self) private var app

    var body: some View {
        if app.store.boxes.isEmpty {
            WelcomeView()
        } else {
            MainTabs()
        }
    }
}

private struct MainTabs: View {
    @Environment(AppModel.self) private var app

    var body: some View {
        @Bindable var app = app
        TabView(selection: $app.selectedTab) {
            Tab("Chat", systemImage: "sparkles", value: AppTab.assistant) {
                AssistantView()
            }
            Tab("Create", systemImage: "wand.and.stars", value: AppTab.create) {
                CreateView()
            }
            Tab("Box", systemImage: "cpu", value: AppTab.box) {
                DashboardView()
            }
            Tab("Models", systemImage: "square.stack.3d.up", value: AppTab.models) {
                ModelsView()
            }
            Tab("Settings", systemImage: "gearshape", value: AppTab.settings) {
                SettingsView()
            }
        }
        // A sidebar on iPad, tabs on iPhone.
        .tabViewStyle(.sidebarAdaptable)
        .tabBarMinimizeBehavior(.onScrollDown)
        .modifier(LiveActivityDriver())
        .tabViewBottomAccessory {
            NowServingAccessory()
        }
        // An approval notification tapped: what's waiting is on the Box tab.
        .onReceive(NotificationCenter.default.publisher(for: ApprovalActions.openWaiting)) { _ in
            app.selectedTab = .box
        }
        .overlay(alignment: .top) {
            if let box = app.insecureBox {
                RepairBanner(box: box, message: "Pair again to connect securely to \(box.name).")
            } else if let session = app.session, session.needsRepair {
                RepairBanner(box: session.box, message: "\(session.box.name) doesn't recognize this phone anymore.")
            } else if let session = app.session, session.statusError != nil {
                OfflineBanner(session: session)
            } else if let message = app.session?.compat.message {
                UpdateNeededBanner(message: message)
            }
        }
    }
}

/// The box stopped accepting this phone's token (it was reset, or its token store
/// cleared), or it was paired before TT Station used HTTPS: say so plainly and
/// offer to pair again, instead of failing every call.
private struct RepairBanner: View {
    let box: SavedBox
    let message: String
    @State private var pairing: PairTarget?

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "lock.trianglebadge.exclamationmark").foregroundStyle(.orange)
            Text(message)
                .font(.tt(.footnote))
            Spacer(minLength: 4)
            Button("Pair again") { pairing = PairTarget(name: box.name, host: box.host, port: box.port) }
                .buttonStyle(.glassProminent)
                .controlSize(.small)
        }
        .padding(12)
        .card(tint: .orange.opacity(0.2), radius: 18)
        .padding(.horizontal, 12)
        .sheet(item: $pairing) { PairSheet(target: $0) }
    }
}

/// The glass bar above the tab bar: what the active box is serving, at a glance, from
/// any tab. Tap to jump to the Box tab.
private struct NowServingAccessory: View {
    @Environment(AppModel.self) private var app
    @Environment(\.tabViewBottomAccessoryPlacement) private var placement

    var body: some View {
        Button {
            app.selectedTab = app.session?.servedKind.map(AppModel.tab(for:)) ?? .box
        } label: {
            HStack(spacing: 10) {
                TTMark().fill(Brand.accent).frame(width: 18, height: 18)
                VStack(alignment: .leading, spacing: 0) {
                    Text(title).font(.tt(.subheadline).weight(.semibold)).lineLimit(1)
                    if placement != .inline {
                        Text(app.activeBox?.name ?? "No box").font(.tt(.caption2)).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                if let hottest = app.telemetry.hottest, app.telemetry.isLive() {
                    Label(Temp.string(hottest), systemImage: "thermometer.medium")
                        .font(.tt(.caption).weight(.semibold).monospacedDigit())
                        .foregroundStyle(Brand.tempColor(hottest))
                }
                Circle().fill(dotColor).frame(width: 8, height: 8)
            }
            .padding(.horizontal, 14)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var title: String {
        guard let session = app.session else {
            return app.insecureBox != nil ? "Pair again to connect securely" : "No box selected"
        }
        if session.statusError != nil { return "Can't reach \(session.box.name)" }
        if let starting = session.startingModel {
            let elapsed = session.serve.map { " · " + Elapsed.string(since: $0.since) } ?? ""
            return "Starting \(Brand.shortModel(starting))\(elapsed)"
        }
        if session.startFailure != nil { return "\(session.serve?.displayModel ?? "Model") failed to start" }
        if session.isSplit, let chat = session.serve?.displayModel, let image = session.companion?.displayModel {
            return "\(chat) + \(image)"
        }
        if let model = session.serve?.displayModel ?? session.status?.servingModel.map(Brand.shortModel) {
            return session.isServing ? "\(model)\(kindSuffix(session.servedKind))" : "Nothing serving"
        }
        return session.status == nil ? "Connecting…" : "Nothing serving"
    }

    private func kindSuffix(_ kind: String?) -> String {
        switch kind {
        case "image": return " · images"
        case "video": return " · video"
        case "speech": return " · speech"
        case "tts": return " · voice"
        default: return ""
        }
    }

    private var dotColor: Color {
        guard let session = app.session else { return .gray }
        if session.statusError != nil || session.startFailure != nil { return .red }
        if session.startingModel != nil { return .orange }
        return session.isServing ? .green : .gray
    }
}

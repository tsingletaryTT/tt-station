// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import BackgroundTasks
import SwiftUI
import UIKit
import UserNotifications

/// Gets the APNs device token from iOS (the standard app-delegate callbacks).
final class AppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    static let tokenKey = "tt.push.deviceToken"
    static let tokenArrived = Notification.Name("tt.push.tokenArrived")

    func application(_ application: UIApplication,
                     didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        UNUserNotificationCenter.current().setNotificationCategories(ApprovalActions.categories)
        return true
    }

    /// Allow / Don't allow, tapped on a notification: answer the box right away.
    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        guard let approval = info[ApprovalActions.approvalKey] as? String else { return }
        let boxID = info[ApprovalActions.boxKey] as? String
        switch response.actionIdentifier {
        case ApprovalActions.allow, ApprovalActions.always, ApprovalActions.deny:
            await ApprovalActions.answer(approval, boxID: boxID,
                                         allow: response.actionIdentifier != ApprovalActions.deny,
                                         always: response.actionIdentifier == ApprovalActions.always)
        default:
            // The notification itself: open the list of what's waiting.
            await MainActor.run { NotificationCenter.default.post(name: ApprovalActions.openWaiting, object: boxID) }
        }
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        let hex = deviceToken.map { String(format: "%02x", $0) }.joined()
        UserDefaults.standard.set(hex, forKey: Self.tokenKey)
        NotificationCenter.default.post(name: Self.tokenArrived, object: hex)
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        NotificationCenter.default.post(name: Self.tokenArrived, object: error)
    }

    /// Show box notifications even while the app is open.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async
        -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }
}

/// Notifications from the box -- "FLUX.1-schnell downloaded", "Qwen3-32B is
/// ready", "tt-quietbox is online" -- the way any app does it: the phone gives
/// the box its push token, and the box sends each alert through Apple's push
/// service. Nothing to sign up for.
///
/// Until the box can push (it needs the app's APNs key -- see
/// docs/reference/alerts-and-power.md), the app falls back to checking the
/// box's event feed itself: instantly while open, now and then in the background.
@MainActor
@Observable
final class AlertCenter {
    static let refreshTaskID = "com.tenstorrent.ttstation.mobile.refresh"

    /// The newest event while the app is open and push isn't doing the job.
    private(set) var banner: BoxEvent?
    private(set) var permission: UNAuthorizationStatus = .notDetermined
    /// The box will push to this phone.
    private(set) var pushActive = false
    /// Why the box can't push yet, if it can't.
    private(set) var pushProblem: String?
    private(set) var registrationError: String?

    private var pollTask: Task<Void, Never>?

    private static func seqKey(_ boxID: String) -> String { "tt.alerts.lastSeq.\(boxID)" }
    private static func pushKey(_ boxID: String) -> String { "tt.push.active.\(boxID)" }

    #if DEBUG
    private static let sandbox = true
    #else
    private static let sandbox = false
    #endif

    func refreshPermission() async {
        permission = await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    /// The moment something slow starts: ask once, the way apps do.
    func offer(for session: BoxSession) async {
        await refreshPermission()
        if permission == .notDetermined {
            await enable(for: session)
        }
    }

    /// Ask for permission, then register this phone with the box.
    func enable(for session: BoxSession) async {
        _ = try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge])
        await refreshPermission()
        await registerIfAllowed(with: session)
    }

    /// Remote push (APNs) needs the `aps-environment` entitlement, which only a
    /// paid Apple team can sign. Off for now: alerts arrive while the app is
    /// open and from background refresh.
    static let remotePush = false

    /// If notifications are allowed, get the device token and hand it to the box.
    func registerIfAllowed(with session: BoxSession) async {
        await refreshPermission()
        guard Self.remotePush else { return }
        guard [.authorized, .provisional, .ephemeral].contains(permission) else { return }
        let token = await deviceToken()
        guard let token else { return }
        do {
            let result = try await session.registerPush(token: token, sandbox: Self.sandbox, name: UIDevice.current.name)
            pushActive = result.pushAvailable
            pushProblem = result.problem
            registrationError = nil
            UserDefaults.standard.set(result.pushAvailable, forKey: Self.pushKey(session.box.id))
        } catch {
            registrationError = error.localizedDescription
        }
    }

    /// The APNs token: iOS hands it to the app delegate after this call.
    private func deviceToken() async -> String? {
        let waiter = Task { () -> String? in
            for await note in NotificationCenter.default.notifications(named: AppDelegate.tokenArrived) {
                if let hex = note.object as? String { return hex }
                if let error = note.object as? Error {
                    await MainActor.run { self.registrationError = error.localizedDescription }
                    return nil
                }
            }
            return nil
        }
        UIApplication.shared.registerForRemoteNotifications()
        let timeout = Task {
            try? await Task.sleep(for: .seconds(10))
            waiter.cancel()
        }
        let fresh = await waiter.value
        timeout.cancel()
        return fresh ?? UserDefaults.standard.string(forKey: AppDelegate.tokenKey)
    }

    /// Check the active box every 10 s while the app is in front (the fallback
    /// path; with push active it only keeps its place in the feed).
    func start(_ app: AppModel) {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                if let session = app.session {
                    await self?.check(session, foreground: true)
                }
                try? await Task.sleep(for: .seconds(10))
            }
        }
    }

    func stop() {
        pollTask?.cancel()
        pollTask = nil
    }

    func dismissBanner() { banner = nil }

    func check(_ session: BoxSession, foreground: Bool) async {
        let key = Self.seqKey(session.box.id)
        let last = UserDefaults.standard.integer(forKey: key)
        guard let (events, latest) = try? await session.events(since: last) else { return }
        UserDefaults.standard.set(latest, forKey: key)
        // First look at a box: just learn where its feed is.
        guard last != 0 else { return }
        // With push on, the box already told the phone -- except a pairing
        // code, which a push never carries (its text passes through Apple):
        // that's shown here, from the box's own feed, while it's still good.
        let pushOn = UserDefaults.standard.bool(forKey: Self.pushKey(session.box.id))
        for event in events where event.kind != "test" {
            if pushOn && !(event.kind == "pairing" && Date().timeIntervalSince1970 - event.at < 120) { continue }
            if foreground && UIApplication.shared.applicationState == .active {
                banner = event
            } else {
                await Self.post(event, boxName: session.box.name, boxID: session.box.id)
            }
        }
    }

    static func post(_ event: BoxEvent, boxName: String, boxID: String) async {
        let content = UNMutableNotificationContent()
        content.title = event.title
        content.subtitle = boxName
        content.body = event.body
        content.sound = .default
        if let approval = event.data?.approval {
            // Answerable from the notification (see ApprovalActions).
            content.categoryIdentifier = approval.canAlways ? ApprovalActions.categoryWithAlways : ApprovalActions.category
            content.userInfo = [ApprovalActions.approvalKey: approval.id, ApprovalActions.boxKey: boxID]
            content.interruptionLevel = .timeSensitive
        }
        let request = UNNotificationRequest(identifier: "tt-event-\(event.seq)", content: content, trigger: nil)
        try? await UNUserNotificationCenter.current().add(request)
    }

    static func scheduleRefresh() {
        let request = BGAppRefreshTaskRequest(identifier: refreshTaskID)
        request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(request)
    }

    /// Background refresh (fallback path only).
    nonisolated static func backgroundCheck() async {
        await MainActor.run { scheduleRefresh() }
        let store = BoxStore()
        let activeID = UserDefaults.standard.string(forKey: "tt.mobile.activeBox")
        guard let box = store.boxes.first(where: { $0.id == activeID }) ?? store.boxes.first,
              !UserDefaults.standard.bool(forKey: "tt.push.active.\(box.id)"),
              let token = store.token(for: box),
              let client = try? AgentClient(box: box, token: token) else { return }
        let key = "tt.alerts.lastSeq.\(box.id)"
        let last = UserDefaults.standard.integer(forKey: key)
        guard let (events, latest) = try? await client.events(since: last) else { return }
        UserDefaults.standard.set(latest, forKey: key)
        guard last != 0 else { return }
        for event in events where event.kind != "test" {
            await post(event, boxName: box.name, boxID: box.id)
        }
    }
}

/// Approvals answered from a notification: a Code session wants to run a
/// command, a routine wants to start a model. The box's events name the
/// approval (`data.approval`); the actions answer `POST /approvals/answer`.
enum ApprovalActions {
    static let category = "TT_APPROVAL"
    static let categoryWithAlways = "TT_APPROVAL_ALWAYS"
    static let allow = "TT_ALLOW"
    static let always = "TT_ALWAYS"
    static let deny = "TT_DENY"
    static let approvalKey = "tt.approval"
    static let boxKey = "tt.box"
    /// Posted when the person taps an approval notification itself.
    static let openWaiting = Notification.Name("tt.approvals.open")

    static var categories: Set<UNNotificationCategory> {
        // Allowing needs the phone unlocked; saying no doesn't.
        let allowAction = UNNotificationAction(identifier: allow, title: "Allow", options: [.authenticationRequired])
        let alwaysAction = UNNotificationAction(identifier: always, title: "Always allow this", options: [.authenticationRequired])
        let denyAction = UNNotificationAction(identifier: deny, title: "Don't allow", options: [.destructive])
        return [
            UNNotificationCategory(identifier: category, actions: [allowAction, denyAction], intentIdentifiers: []),
            UNNotificationCategory(identifier: categoryWithAlways, actions: [allowAction, alwaysAction, denyAction], intentIdentifiers: []),
        ]
    }

    /// Answer on the box the approval came from; tell the person if it didn't work.
    static func answer(_ id: String, boxID: String?, allow: Bool, always: Bool) async {
        let store = BoxStore()
        guard let box = store.boxes.first(where: { $0.id == boxID }) ?? store.boxes.first,
              let client = try? AgentClient(box: box, token: store.token(for: box)) else { return }
        do {
            try await client.answer(approval: id, allow: allow, always: always)
        } catch {
            let content = UNMutableNotificationContent()
            content.title = "Couldn't answer"
            content.subtitle = box.name
            content.body = "\(error.localizedDescription) Open TT Station to see what's waiting."
            try? await UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "tt-answer-\(id)", content: content, trigger: nil))
        }
    }
}

/// The in-app banner (fallback path, while the app is open).
struct AlertBanner: View {
    let event: BoxEvent
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: icon)
                .font(.tt(.title3))
                .foregroundStyle(event.kind.contains("failed") || event.kind == "chips_hot" ? .orange : Brand.accent)
            VStack(alignment: .leading, spacing: 2) {
                Text(event.title).font(.tt(.subheadline).weight(.semibold))
                Text(event.body).font(.tt(.caption)).foregroundStyle(.secondary).lineLimit(2)
            }
            Spacer(minLength: 0)
            Button(action: onDismiss) { Image(systemName: "xmark").font(.tt(.caption).weight(.bold)) }
                .foregroundStyle(.secondary)
        }
        .padding(14)
        .glassEffect(.regular, in: .rect(cornerRadius: 20))
        .padding(.horizontal, 12)
        .task {
            // A pairing code needs time to read and type.
            try? await Task.sleep(for: .seconds(event.kind == "pairing" ? 60 : 6))
            onDismiss()
        }
    }

    private var icon: String {
        switch event.kind {
        case "download_done": return "arrow.down.circle.fill"
        case "model_ready": return "checkmark.circle.fill"
        case "box_online": return "power.circle.fill"
        case "chips_hot": return "thermometer.high"
        case "power": return "moon.zzz.fill"
        default: return event.kind.contains("failed") ? "exclamationmark.triangle.fill" : "bell.fill"
        }
    }
}

/// Box tab → Notifications.
struct AlertsView: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var settings: NotifySettings?
    @State private var working = false
    @State private var message: String?

    private var alerts: AlertCenter { app.alerts }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("Get a notification when a download finishes, a model is loaded and ready, a start fails, the box comes online, or its chips run hot.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                }

                Section {
                    switch alerts.permission {
                    case .authorized, .provisional, .ephemeral:
                        if alerts.pushActive || settings?.push.available == true {
                            Label("On for this iPhone", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                        } else {
                            Label("Allowed on this iPhone", systemImage: "checkmark.circle").foregroundStyle(.green)
                        }
                    case .denied:
                        Button("Turn on notifications in Settings") {
                            if let url = URL(string: UIApplication.openSettingsURLString) { openURL(url) }
                        }
                    default:
                        Button("Turn on notifications") {
                            Task { await alerts.enable(for: session); await reload() }
                        }
                    }
                    if let error = alerts.registrationError {
                        Text(error).font(.tt(.footnote)).foregroundStyle(.orange)
                    }
                }

                if let push = settings?.push, !push.available {
                    Section("Your box can't send them yet") {
                        Text("Until it can, TT Station shows alerts while it's open and checks now and then in the background, so they can arrive late.")
                            .font(.tt(.footnote))
                        if let problem = push.problem {
                            Text(problem).font(.tt(.footnote)).foregroundStyle(.secondary)
                        }
                    }
                }

                if let devices = settings?.push.devices, !devices.isEmpty {
                    Section("Phones \(session.box.name) notifies") {
                        ForEach(devices, id: \.self) { device in
                            Text(device.name.isEmpty ? "iPhone" : device.name)
                        }
                    }
                }

                Section {
                    Button {
                        Task { await sendTest() }
                    } label: {
                        Label("Send a test notification", systemImage: "bell.badge")
                    }
                    .disabled(working || ![.authorized, .provisional, .ephemeral].contains(alerts.permission))
                    if let message {
                        Text(message).font(.tt(.footnote)).foregroundStyle(.secondary)
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.backdrop)
            .navigationTitle("Notifications")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task {
                await alerts.registerIfAllowed(with: session)
                await reload()
            }
        }
    }

    private func reload() async {
        settings = try? await session.notifySettings()
    }

    private func sendTest() async {
        working = true
        defer { working = false }
        do {
            let sent = try await session.testNotification()
            message = sent == 1 ? "Sent. It should appear in a moment." : "Sent to \(sent) phones."
        } catch {
            message = error.localizedDescription
        }
    }
}

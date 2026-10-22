// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation

// The agent routes behind the phone's box controls (wire types in
// `BoxControlModels.swift`), and the active box's session's way to them.
// Every call goes through `AgentClient.send`/`raw`, so every one carries the
// token and turns a `401` into the "Pair again" flow.

extension AgentClient {
    // MARK: power and sleep

    /// Turn Wake-on-LAN on for good (owners). `applied: false` carries the
    /// command for the owner to run on the box when it can't do it itself.
    func enableWakeOnLAN() async throws -> WakeOnLanEnable {
        try await send("POST", "/power/wake-on-lan", body: [String: String](), authed: true, timeout: 40)
    }

    func idle() async throws -> IdleInfo {
        try await send("GET", "/idle", authed: true, timeout: 15)
    }

    func setIdle(_ settings: IdleSettings) async throws -> IdleSettings {
        struct Response: Decodable { let settings: IdleSettings }
        let r: Response = try await send("PUT", "/idle", body: settings, authed: true, timeout: 15)
        return r.settings
    }

    /// Put the served model to sleep now; returns what's asleep.
    func sleepModel() async throws -> String {
        struct Response: Decodable { let asleep: String }
        let r: Response = try await send("POST", "/idle/sleep", body: [String: String](), authed: true, timeout: 180)
        return r.asleep
    }

    /// Start the model that was put to sleep; returns what's waking.
    func wakeModel() async throws -> String {
        struct Response: Decodable { let waking: String }
        let r: Response = try await send("POST", "/idle/wake", body: [String: String](), authed: true, timeout: 60)
        return r.waking
    }

    // MARK: devices, activity, settings

    func devices() async throws -> DeviceList {
        try await send("GET", "/devices", authed: true, timeout: 15)
    }

    /// Make a device an owner, or a member again (owners).
    func setOwner(device id: String, owner: Bool) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/devices/\(id)/owner", body: ["owner": owner], authed: true, timeout: 15)
    }

    /// Sign a device out (any device itself; another one, owners).
    func removeDevice(_ id: String) async throws {
        _ = try await raw("DELETE", "/devices/\(id)", body: Optional<[String: String]>.none, timeout: 15)
    }

    func audit(offset: Int = 0, limit: Int = 50) async throws -> AuditPage {
        try await send("GET", "/audit", query: [URLQueryItem(name: "offset", value: String(offset)),
                                               URLQueryItem(name: "limit", value: String(limit))],
                       authed: true, timeout: 15)
    }

    func dataSettings() async throws -> DataSettingsInfo {
        try await send("GET", "/data/settings", authed: true, timeout: 15)
    }

    /// Who may start and stop models: `anyone` or `owners` (owners).
    func setModelControl(_ rule: String) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("PUT", "/data/settings", body: ["model_control": rule], authed: true, timeout: 15)
    }

    // MARK: everything the box can serve

    func serveOptions() async throws -> ServeOptions {
        try await send("GET", "/code/models", authed: true, timeout: 60)
    }

    /// Start `option` the way the box said to (its route and body).
    func start(_ option: ServeOption) async throws {
        guard let call = option.serve, call.route.hasPrefix("/") else {
            throw AgentError.http(409, option.blocked ?? "That can't be started from here.")
        }
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", call.route, body: call.body, authed: true, timeout: 60)
    }

    // MARK: Code

    func codeOverview() async throws -> CodeOverview {
        try await send("GET", "/code", authed: true, timeout: 20)
    }

    func startCode(_ new: NewCodeSession) async throws -> CodeSession {
        struct Response: Decodable { let session: CodeSession }
        let r: Response = try await send("POST", "/code/sessions", body: new, authed: true, timeout: 30)
        return r.session
    }

    func codingFolders(path: String? = nil) async throws -> CodingFolders {
        try await send("GET", "/coding/folders", query: path.map { [URLQueryItem(name: "path", value: $0)] } ?? [],
                       authed: true, timeout: 15)
    }

    /// Stop a session's agent (the session and its files stay).
    func stopCode(_ id: String) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/code/sessions/\(id)/stop", body: [String: String](), authed: true, timeout: 20)
    }

    // MARK: research

    func research() async throws -> ResearchList {
        try await send("GET", "/research", authed: true, timeout: 20)
    }

    func startResearch(_ request: ResearchRequest) async throws -> ResearchJob {
        struct Response: Decodable { let job: ResearchJob }
        let r: Response = try await send("POST", "/research", body: request, authed: true, timeout: 30)
        return r.job
    }

    func researchReport(_ id: String) async throws -> ResearchReport {
        try await send("GET", "/research/\(id)", authed: true, timeout: 20)
    }

    func shareResearch(_ id: String, shared: Bool) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/research/\(id)/share", body: ["shared": shared], authed: true, timeout: 15)
    }

    func stopResearch(_ id: String) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/research/\(id)/stop", body: [String: String](), authed: true, timeout: 15)
    }

    // MARK: recording on the box

    func recordStatus() async throws -> RecordStatus {
        try await send("GET", "/record", authed: true, timeout: 15)
    }

    func startRecording(_ start: RecordStart) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/record/start", body: start, authed: true, timeout: 30)
    }

    /// Stop; the recording is queued for transcription unless `discard`.
    func stopRecording(discard: Bool) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/record/stop", body: ["discard": discard], authed: true, timeout: 30)
    }

    // MARK: routines, updates

    /// Pause or resume a routine (the rest of it sent back as it was).
    func updateRoutine(_ id: String, _ update: RoutineUpdate) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("PUT", "/routines/\(id)", body: update, authed: true, timeout: 15)
    }

    func updates() async throws -> UpdatesInfo {
        try await send("GET", "/updates", authed: true, timeout: 60)
    }

    /// Install the newest signed release on the box (owners).
    func installUpdate(version: String) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/updates/install", body: ["version": version], authed: true, timeout: 120)
    }
}

extension BoxSession {
    /// Ask the box who this phone is there and what's asleep, and (with
    /// `learnWake`, or while the phone doesn't know yet) remember how to wake it.
    /// Quiet: an older agent without a route just leaves it unknown.
    func refreshAccess(learnWake: Bool = false) async {
        if let devices = try? await client.devices() {
            let settings = try? await client.dataSettings()
            access = BoxAccess(owner: devices.owner,
                               mayControlModels: settings?.mayControlModels ?? true,
                               modelControl: settings?.settings.modelControl ?? "anyone")
        }
        idle = try? await client.idle()
        if learnWake || WakeOnLAN.target(for: box) == nil {
            _ = try? await powerInfo()
        }
    }

    var isOwner: Bool { access?.owner ?? false }

    // Sleep and wake.
    func enableWakeOnLAN() async throws -> WakeOnLanEnable {
        let result = try await client.enableWakeOnLAN()
        _ = try? await powerInfo()
        return result
    }
    func setIdle(_ settings: IdleSettings) async throws {
        _ = try await client.setIdle(settings)
        idle = try? await client.idle()
    }
    func sleepModel() async throws -> String {
        let label = try await client.sleepModel()
        idle = try? await client.idle()
        await refreshStatus()
        return label
    }
    func wakeModel() async throws -> String {
        onLongOperation?()
        let label = try await client.wakeModel()
        idle = try? await client.idle()
        await refreshStatus()
        return label
    }

    // Devices and security.
    func devices() async throws -> DeviceList { try await client.devices() }
    func setOwner(device id: String, owner: Bool) async throws { try await client.setOwner(device: id, owner: owner) }
    func removeDevice(_ id: String) async throws { try await client.removeDevice(id) }
    func audit(offset: Int = 0) async throws -> AuditPage { try await client.audit(offset: offset) }
    func setModelControl(_ rule: String) async throws {
        try await client.setModelControl(rule)
        await refreshAccess()
    }

    // Everything the box can serve.
    func serveOptions() async throws -> ServeOptions { try await client.serveOptions() }
    func start(_ option: ServeOption) async throws {
        onLongOperation?()
        try await client.start(option)
        await refreshStatus()
    }

    // Code.
    func codeOverview() async throws -> CodeOverview { try await client.codeOverview() }
    func startCode(_ new: NewCodeSession) async throws -> CodeSession { try await client.startCode(new) }
    func stopCode(_ id: String) async throws { try await client.stopCode(id) }
    func codingFolders(path: String? = nil) async throws -> CodingFolders { try await client.codingFolders(path: path) }

    // Research.
    func research() async throws -> ResearchList { try await client.research() }
    func startResearch(_ request: ResearchRequest) async throws -> ResearchJob {
        onLongOperation?()
        return try await client.startResearch(request)
    }
    func researchReport(_ id: String) async throws -> ResearchReport { try await client.researchReport(id) }
    func shareResearch(_ id: String, shared: Bool) async throws { try await client.shareResearch(id, shared: shared) }
    func stopResearch(_ id: String) async throws { try await client.stopResearch(id) }

    // Recording on the box.
    func recordStatus() async throws -> RecordStatus { try await client.recordStatus() }
    func startBoxRecording(_ start: RecordStart) async throws { try await client.startRecording(start) }
    func stopBoxRecording(discard: Bool) async throws { try await client.stopRecording(discard: discard) }

    // Routines, updates.
    func setRoutine(_ routine: Routine, enabled: Bool) async throws {
        guard let update = RoutineUpdate(routine, enabled: enabled) else {
            throw AgentError.http(409, "Update TT Station on the box to pause routines from your phone.")
        }
        try await client.updateRoutine(routine.id, update)
    }
    func updates() async throws -> UpdatesInfo { try await client.updates() }
    func installUpdate(version: String) async throws { try await client.installUpdate(version: version) }
}

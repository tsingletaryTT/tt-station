// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation

// Wire types for controlling the box from the phone: status and versions,
// power and wake, idle sleep, devices and the activity log, the model-control
// setting, everything the box can serve, Code, research, recording, routines
// and updates. Shapes mirror `crates/tt-station-agentd` (route by route in the
// doc comments). Foundation only, so they build and test anywhere.

struct BoxStatus: Decodable, Equatable {
    let name: String
    let chips: String
    /// `idle` or `serving:<model>` — the same string form the mDNS TXT record uses.
    let status: String
    let deviceMesh: String?
    /// The MAC of the port with the default route (often Wi-Fi). For waking, use
    /// `PowerInfo.wakeOnLan.mac` instead: only a wired port can wake a computer.
    var mac: String? = nil
    /// Whether the served model is answering (`ok`, `degraded`, `unknown`).
    var modelHealth: ModelHealth? = nil
    /// This agent's build, the API generation it speaks, and the oldest client
    /// generation it still serves; `nil` from agents that predate them.
    var agentVersion: String? = nil
    var apiVersion: Int? = nil
    var minClient: Int? = nil

    enum CodingKeys: String, CodingKey {
        case name, chips, status, mac
        case deviceMesh = "device_mesh"
        case modelHealth = "model_health"
        case agentVersion = "agent_version"
        case apiVersion = "api_version"
        case minClient = "min_client"
    }

    var servingModel: String? {
        status.hasPrefix("serving:") ? String(status.dropFirst("serving:".count)) : nil
    }
}

/// `/status` → `model_health`: is the served model answering?
struct ModelHealth: Decodable, Equatable {
    let state: String // unknown | ok | degraded
    var reason: String? = nil
    var isDegraded: Bool { state == "degraded" }
}

enum AgentError: LocalizedError {
    case http(Int, String)
    case notPaired
    case badHost(String)
    case unreachable(address: String, reason: String)
    /// A box saved before TT Station used HTTPS: it has no pinned certificate,
    /// and the app never falls back to plain HTTP.
    case insecure

    var errorDescription: String? {
        switch self {
        case .http(401, _): return "This phone isn't paired with the box any more. Pair again."
        case .http(403, let message):
            // The box says who may (owners, or owners only for models) in plain words.
            return message.isEmpty || message == "Forbidden" ? "Only an owner of this box can do that." : message
        case .insecure:
            return "Pair again to connect securely: scan the pairing code shown in TT Station on the box (Settings → Devices and pairing)."
        case .http(_, let message): return message
        case .notPaired: return "Pair with the box first."
        case .badHost(let host): return "Can't make an address from \"\(host)\"."
        case let .unreachable(address, reason):
            return "Couldn't reach the box at \(address). \(reason) Make sure this phone is on the same network as the box, and that Local Network access is on in Settings → TT Station."
        }
    }

    /// The box refused because only an owner (or, for models, only owners) may.
    var isOwnerOnly: Bool {
        if case .http(403, _) = self { return true }
        return false
    }

    /// The box couldn't be reached at all (asleep, off, or another network).
    var isUnreachable: Bool {
        if case .unreachable = self { return true }
        return false
    }
}

// MARK: - Power and wake

/// `GET /power/info`: what the phone can do with the box's power.
struct PowerInfo: Decodable, Equatable {
    struct Nic: Decodable, Equatable {
        let name: String
        let mac: String
        let connected: Bool
        let wakeEnabled: Bool
        enum CodingKeys: String, CodingKey {
            case name, mac, connected
            case wakeEnabled = "wake_enabled"
        }
    }
    struct Step: Decodable, Equatable, Hashable {
        let why: String
        let command: String
    }
    let canPowerOff: String
    let canReboot: String
    let canSuspend: String
    let startsAtBoot: Bool
    let wired: [Nic]
    /// `nil` from agents that predate it: fall back to `wired`.
    var wakeOnLan: WakeOnLanInfo? = nil
    let setup: [Step]
    enum CodingKeys: String, CodingKey {
        case wired, setup
        case canPowerOff = "can_power_off", canReboot = "can_reboot", canSuspend = "can_suspend"
        case startsAtBoot = "starts_at_boot"
        case wakeOnLan = "wake_on_lan"
    }
    /// Wake-on-LAN can work: a cabled NIC allowed to wake the machine.
    var canWake: Bool { wakeOnLan?.possible ?? wired.contains { $0.connected && $0.wakeEnabled } }
}

/// `GET /power/info` → `wake_on_lan`: can a phone wake this box, and with what.
struct WakeOnLanInfo: Decodable, Equatable {
    let possible: Bool
    let nic: String?
    /// The wired port's MAC: what the magic packet carries.
    let mac: String?
    let supported: Bool
    let enabled: Bool
    let persistent: Bool
    let connected: Bool
    let connection: String?
    /// The box's LAN broadcast addresses.
    let broadcast: [String]
    let whyNot: String?
    let enableCommand: String?
    enum CodingKeys: String, CodingKey {
        case possible, nic, mac, supported, enabled, persistent, connected, connection, broadcast
        case whyNot = "why_not"
        case enableCommand = "enable_command"
    }
}

/// `POST /power/wake-on-lan`: what turning it on did.
struct WakeOnLanEnable: Decodable, Equatable {
    let applied: Bool
    let message: String
    /// The command for an owner to run once on the box, when the box couldn't.
    let command: String?
}

// MARK: - Idle sleep

/// `GET /idle`: the model's sleep settings, and what's asleep.
struct IdleInfo: Decodable, Equatable {
    struct Asleep: Decodable, Equatable {
        let target: String
        let community: Bool
        /// As a person would say it.
        let label: String
        let at: Double
    }
    let settings: IdleSettings
    let asleep: Asleep?
    /// How long the served model has had nothing to do (while serving).
    let idleForS: Int?
    let waking: Bool
    enum CodingKeys: String, CodingKey {
        case settings, asleep, waking
        case idleForS = "idle_for_s"
    }
}

/// `PUT /idle`: put the model to sleep after a quiet spell, and wake it on use.
struct IdleSettings: Codable, Equatable {
    var enabled: Bool
    var afterMinutes: Int
    var wakeOnRequest: Bool
    enum CodingKeys: String, CodingKey {
        case enabled
        case afterMinutes = "after_minutes"
        case wakeOnRequest = "wake_on_request"
    }
}

// MARK: - Devices, activity, the model-control setting

/// What this phone may do on the box.
struct BoxAccess: Equatable {
    /// An owner: manages devices, keys, power and updates.
    var owner: Bool
    /// May start, stop and switch the served model (the owners' model-control rule).
    var mayControlModels: Bool
    /// Who may start and stop models: `anyone` or `owners`.
    var modelControl: String

    /// The rule as a member hears it, when it keeps them from models.
    var modelsNote: String? {
        mayControlModels ? nil : "Only owners can start or stop models on this box."
    }
}

/// `GET /devices`: an owner sees every paired device; a member sees itself.
struct DeviceList: Decodable, Equatable {
    let devices: [PairedDevice]
    /// This device is an owner.
    let owner: Bool
    let idleExpiryDays: Int?
    enum CodingKeys: String, CodingKey {
        case devices, owner
        case idleExpiryDays = "idle_expiry_days"
    }
}

struct PairedDevice: Decodable, Equatable, Identifiable {
    let id: String
    let name: String
    let platform: String?
    let paired: Double?
    let lastUsed: Double
    let thisDevice: Bool
    let owner: Bool
    let person: String?
    enum CodingKeys: String, CodingKey {
        case id, name, platform, paired, owner, person
        case lastUsed = "last_used"
        case thisDevice = "this_device"
    }
}

/// `GET /audit` (owners): the activity log, newest first.
struct AuditPage: Decodable, Equatable {
    let entries: [AuditEntry]
    let more: Bool
    let offset: Int
}

struct AuditEntry: Decodable, Equatable, Hashable {
    let ts: Double
    let deviceId: String?
    let deviceName: String
    let action: String
    let target: String
    let result: String
    enum CodingKeys: String, CodingKey {
        case ts, action, target, result
        case deviceId = "device_id"
        case deviceName = "device_name"
    }
}

/// `GET /data/settings`: the household settings, whether this device is an
/// owner, and whether it may start and stop models.
struct DataSettingsInfo: Decodable, Equatable {
    let settings: DataSettings
    let owner: Bool
    let mayControlModels: Bool
    enum CodingKeys: String, CodingKey {
        case settings, owner
        case mayControlModels = "may_control_models"
    }
}

struct DataSettings: Decodable, Equatable {
    let keepAudio: Bool
    let transcriptsToLibrary: Bool
    let retentionDays: Int?
    let restartLastModel: Bool
    /// `anyone` or `owners`: who may start, stop and switch the served model.
    let modelControl: String
    enum CodingKeys: String, CodingKey {
        case keepAudio = "keep_audio"
        case transcriptsToLibrary = "transcripts_to_library"
        case retentionDays = "retention_days"
        case restartLastModel = "restart_last_model"
        case modelControl = "model_control"
    }
}

// MARK: - Everything the box can serve

/// Any JSON value, kept as it came (a serve call's body is the agent's to define).
enum JSONValue: Codable, Equatable, Hashable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }
}

/// `GET /code/models`: every model the box can serve -- its catalog, community
/// packages (one entry per profile) and native vLLM bundles -- with what
/// choosing each does.
struct ServeOptions: Decodable, Equatable {
    struct Serve: Decodable, Equatable {
        let phase: String
        let model: String?
        let since: Double?
        let error: String?
        let etaS: Int?
        enum CodingKeys: String, CodingKey {
            case phase, model, since, error
            case etaS = "eta_s"
        }
    }
    let codeModel: String?
    let serve: Serve?
    let catalogError: String?
    let models: [ServeOption]
    enum CodingKeys: String, CodingKey {
        case serve, models
        case codeModel = "code_model"
        case catalogError = "catalog_error"
    }
}

struct ServeOption: Decodable, Equatable, Identifiable {
    struct Tools: Decodable, Equatable { let verified: Bool; let how: String }
    /// `in_use` (serving now), `switch` (start it), `busy` or `unavailable` (with `why`).
    struct Choice: Decodable, Equatable { let action: String; let why: String? }
    /// What starting it posts: a route and its body, exactly as the agent gave them.
    struct Call: Decodable, Equatable { let route: String; let body: JSONValue }

    /// `catalog:<name>`, `community:<id>#<profile>`, `native:<id>`.
    let key: String
    /// The model, the same for all its setups.
    let group: String
    let name: String
    let model: String
    /// `catalog`, `community`, `native` or `running`.
    let source: String
    let origin: String?
    let profile: String?
    let profileLabel: String?
    let profileNote: String?
    let chips: Int?
    let context: Int?
    let tools: Tools?
    let choice: Choice
    let serve: Call?
    let etaS: Int?
    var id: String { key }

    enum CodingKeys: String, CodingKey {
        case key, group, name, model, source, origin, profile, chips, context, tools, choice, serve
        case profileLabel = "profile_label"
        case profileNote = "profile_note"
        case etaS = "eta_s"
    }

    var isInUse: Bool { choice.action == "in_use" }
    var canStart: Bool { choice.action == "switch" && serve != nil }
    /// Why it can't be started now, when it can't.
    var blocked: String? { canStart || isInUse ? nil : choice.why ?? "Not available on this box." }
}

// MARK: - Waiting for the person, Code, Routines

/// Something waiting for the person's OK (`/approvals`): a Code session's
/// command or edit, or a routine's action.
struct Approval: Decodable, Identifiable, Equatable {
    let id: String
    let kind: String // code | routine
    let place: String
    let summary: String
    let canAlways: Bool
    let session: String?
    let run: String?
    enum CodingKeys: String, CodingKey {
        case id, kind, summary, session, run
        case place = "where"
        case canAlways = "can_always"
    }
}

/// A Code session (`/code`).
struct CodeSession: Decodable, Identifiable, Hashable {
    let id: String
    let folder: String
    let title: String
    let last: Double
    let sandbox: Bool
    let running: Bool
    let busy: Bool
    let waiting: Int
    let changes: Int
}

/// `GET /code`: the sessions, and whether new ones may run outside the sandbox.
struct CodeOverview: Decodable {
    struct Sandbox: Decodable { let ready: Bool; let step: String?; let error: String? }
    let sessions: [CodeSession]
    /// Owners only: members get the sandbox.
    var outsideSandbox: Bool? = nil
    var sandbox: Sandbox? = nil
    var home: String? = nil
    enum CodingKeys: String, CodingKey {
        case sessions, sandbox, home
        case outsideSandbox = "outside_sandbox"
    }
}

/// `GET /coding/folders?path=`: the folders inside one (the box account's home by default).
struct CodingFolders: Decodable, Equatable {
    struct Folder: Decodable, Equatable, Hashable, Identifiable {
        let name: String
        let path: String
        let git: Bool
        var id: String { path }
    }
    let path: String
    let parent: String?
    let git: Bool
    let folders: [Folder]
}

/// `POST /code/sessions`.
struct NewCodeSession: Encodable, Equatable {
    let folder: String
    var sandbox = true
    /// `ask`, `edits` or `auto` (`auto` is for the sandbox only).
    var policy = "ask"
    var think = false
}

/// One thing that happened in a Code session (`/code/sessions/{id}`).
struct CodeEvent: Decodable, Identifiable, Equatable {
    let seq: Int
    let kind: String // user | text | tool | tool_result | approval | decided | done | error | notice | ...
    let text: String?
    let name: String?
    let summary: String?
    let output: String?
    let isError: Bool?
    let ok: Bool?
    let input: ToolInput?
    var id: Int { seq }
    enum CodingKeys: String, CodingKey {
        case seq, kind, text, name, summary, output, ok, input
        case isError = "is_error"
    }

    /// The parts of a tool's input worth showing.
    struct ToolInput: Decodable, Equatable {
        let command: String?
        let filePath: String?
        let path: String?
        let pattern: String?
        let description: String?
        enum CodingKeys: String, CodingKey {
            case command, path, pattern, description
            case filePath = "file_path"
        }
    }
}

struct CodeSessionDetail: Decodable {
    let session: CodeSession
    let events: [CodeEvent]
}

/// When a routine runs (`/routines`).
struct RoutineSchedule: Codable, Equatable {
    /// `manual`, `daily`, `weekdays`, `days` or `hourly`.
    var kind: String
    var time: String? = nil
    var everyHours: Int? = nil
    var days: [Int]? = nil
    enum CodingKeys: String, CodingKey {
        case kind, time, days
        case everyHours = "every_hours"
    }
}

/// A routine (`/routines`).
struct Routine: Decodable, Identifiable, Equatable {
    struct LastRun: Decodable, Equatable {
        let id: String
        let status: String
        let started: Double
        let finished: Double?
        let headline: String?
    }
    let id: String
    let name: String
    let goal: String
    let when: String
    let enabled: Bool
    let nextRun: Double?
    let last: LastRun?
    /// What a `PUT` sends back; `nil` from agents that predate them.
    var schedule: RoutineSchedule? = nil
    var allowed: [String]? = nil
    var mode: String? = nil
    enum CodingKeys: String, CodingKey {
        case id, name, goal, when, enabled, last, schedule, allowed, mode
        case nextRun = "next_run"
    }
}

/// `PUT /routines/{id}`: the whole routine, as it is, with `enabled` changed.
struct RoutineUpdate: Encodable, Equatable {
    let name: String
    let goal: String
    let schedule: RoutineSchedule
    let allowed: [String]
    let enabled: Bool
    let mode: String?

    /// Pausing or resuming `routine` without changing anything else; `nil`
    /// when the box didn't say enough about it to send it back whole.
    init?(_ routine: Routine, enabled: Bool) {
        guard let schedule = routine.schedule else { return nil }
        name = routine.name
        goal = routine.goal
        self.schedule = schedule
        allowed = routine.allowed ?? []
        self.enabled = enabled
        mode = routine.mode
    }
}

/// One run of a routine (`/routines/runs/{id}`).
struct RoutineRun: Decodable, Identifiable, Hashable {
    struct Pending: Decodable, Hashable { let summary: String }
    let id: String
    let routine: String
    let name: String
    let started: Double
    let finished: Double?
    let status: String // running | waiting | done | failed | stopped
    let report: String?
    let pending: Pending?
    let activity: String?
}

// MARK: - Research

/// `GET /research`: the reports this device may see.
struct ResearchList: Decodable {
    struct Check: Decodable, Equatable {
        let ok: Bool
        let why: String?
        let model: String?
        let search: String?
    }
    let jobs: [ResearchJob]
    let check: Check?
    let owner: Bool?
}

struct ResearchJob: Decodable, Equatable, Identifiable, Hashable {
    let id: String
    let question: String
    /// `quick`, `standard` or `deep`.
    let depth: String
    let created: Double
    /// `queued`, `running`, `done`, `failed` or `stopped`.
    let state: String
    let step: String?
    let finished: Double?
    let error: String?
    let model: String?
    let summary: String?
    let title: String?
    let startedByName: String?
    let shared: Bool?
    enum CodingKeys: String, CodingKey {
        case id, question, depth, created, state, step, finished, error, model, summary, title, shared
        case startedByName = "started_by_name"
    }

    var displayTitle: String { title ?? question }
    var isActive: Bool { state == "queued" || state == "running" }
}

/// `GET /research/{id}`: the report and its sources.
struct ResearchReport: Decodable {
    struct Source: Decodable, Equatable, Identifiable, Hashable {
        let n: Int
        let url: String
        let title: String?
        let status: String
        let relevant: Bool?
        var id: Int { n }
    }
    let job: ResearchJob
    let report: String?
    let sources: [Source]
    /// This device may stop, rename, delete and share it.
    let canChange: Bool?
    enum CodingKeys: String, CodingKey {
        case job, report, sources
        case canChange = "can_change"
    }
}

/// `POST /research`.
struct ResearchRequest: Encodable, Equatable {
    let question: String
    var depth = "standard"
}

// MARK: - Recording on the box

/// `GET /record`: the box's microphone and speakers, and the recording in
/// progress (with who started it: anyone paired can, so the room is told).
struct RecordStatus: Decodable, Equatable {
    struct Recording: Decodable, Equatable {
        let id: String
        let title: String
        let started: Double
        let seconds: Int
        let ended: Bool?
        let maxSeconds: Int?
        let startedBy: String?
        enum CodingKeys: String, CodingKey {
            case id, title, started, seconds, ended
            case maxSeconds = "max_seconds"
            case startedBy = "started_by"
        }
    }
    let microphone: String?
    let speakers: String?
    let recording: Recording?
}

/// `POST /record/start`.
struct RecordStart: Encodable, Equatable {
    var title: String?
    var mic = true
    var system = true
}

// MARK: - Updates

/// `GET /updates`: this box's TT Station version and a newer signed release.
struct UpdatesInfo: Decodable, Equatable {
    struct Release: Decodable, Equatable {
        let version: String
        let name: String
        let notes: String
        let published: String
    }
    struct Undo: Decodable, Equatable { let to: String; let label: String? }
    let version: String
    let update: Release?
    let latest: Release?
    let working: String?
    let problem: String?
    let undo: Undo?
}

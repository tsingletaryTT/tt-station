import CryptoKit
import Foundation

// Wire types for `tt-station-agentd`'s control API. Shapes mirror
// `crates/libttstation/src/model.rs` and `crates/tt-station-agentd/src/routes.rs`;
// the assistant routes are documented in `docs/reference/assistant.md`.

struct BoxStatus: Decodable, Equatable {
    let name: String
    let chips: String
    /// `idle` or `serving:<model>` — the same string form the mDNS TXT record uses.
    let status: String
    let deviceMesh: String?

    enum CodingKeys: String, CodingKey {
        case name, chips, status
        case deviceMesh = "device_mesh"
    }

    var servingModel: String? {
        status.hasPrefix("serving:") ? String(status.dropFirst("serving:".count)) : nil
    }
}

struct ModelInfo: Decodable, Hashable {
    let name: String
    let devices: [String]
    let downloaded: Bool?
}

struct ModelsResponse: Decodable {
    let models: [ModelInfo]
}

/// One model in the box's catalog, from the official tt CLI (`GET /tt/models`).
struct TtModel: Decodable, Hashable, Identifiable {
    let name: String
    let hfRepo: String?
    let modelType: String
    let status: String?
    let supported: Bool
    let engines: [String]
    let toolCallParser: String?
    let cached: Bool
    let cacheSizeBytes: Int64
    let minDiskGB: Double?
    let paramCount: Double?
    /// The agent's own measurement: `complete`, `partial`, `missing`, or `container`
    /// (the server fetches its own weights). `nil` from agents that predate it.
    let weights: String?
    let downloadedBytes: Int64?

    var id: String { name }
    /// Where this model's weights stand, falling back to tt's `cached` flag.
    var weightsState: String { weights ?? (cached ? "complete" : "missing") }
    /// Can start without downloading first.
    var isReady: Bool { weightsState == "complete" || weightsState == "container" }
    /// What the app does with it once it's serving.
    var kind: String {
        switch modelType {
        case "llm", "vlm": return "chat"
        case "image": return "image"
        case "video": return "video"
        case "audio": return "speech"
        case "text_to_speech": return "tts"
        default: return "other"
        }
    }
    var isChatModel: Bool { modelType == "llm" || engines.contains("vLLM") }
    /// Can drive the box assistant: a chat model served with a tool-call parser.
    var worksWithAssistant: Bool { isChatModel && toolCallParser != nil }

    enum CodingKeys: String, CodingKey {
        case name, status, supported, engines, cached
        case hfRepo = "hf_repo"
        case modelType = "model_type"
        case toolCallParser = "tool_call_parser"
        case cacheSizeBytes = "cache_size_bytes"
        case minDiskGB = "min_disk_gb"
        case paramCount = "param_count"
        case weights
        case downloadedBytes = "downloaded_bytes"
    }

    /// A model from the legacy `GET /models` list, for agents without the tt CLI.
    init(legacy info: ModelInfo) {
        name = info.name
        hfRepo = info.name
        modelType = "llm"
        status = nil
        supported = true
        engines = ["vLLM"]
        toolCallParser = nil
        cached = info.downloaded ?? false
        cacheSizeBytes = 0
        minDiskGB = nil
        paramCount = nil
        weights = nil
        downloadedBytes = nil
    }
}

/// A `tt model pull` the box is running or ran.
struct PullJob: Decodable, Hashable {
    let model: String
    let state: String // "running" | "done" | "failed" | "cancelled"
    let error: String?
    /// A page that fixes the failure, e.g. a gated repo's license on huggingface.co.
    let actionURL: String?
    let startedAt: Int
    let finishedAt: Int?
    let bytesOnDisk: Int64

    var isRunning: Bool { state == "running" }

    enum CodingKeys: String, CodingKey {
        case model, state, error
        case actionURL = "action_url"
        case startedAt = "started_at"
        case finishedAt = "finished_at"
        case bytesOnDisk = "bytes_on_disk"
    }
}

struct TtCatalog: Decodable {
    let device: String?
    let diskFreeBytes: Int64?
    let models: [TtModel]
    let pulls: [PullJob]

    enum CodingKeys: String, CodingKey {
        case device, models, pulls
        case diskFreeBytes = "disk_free_bytes"
    }
}

struct PullsInfo: Decodable {
    let pulls: [PullJob]
    let diskFreeBytes: Int64?

    enum CodingKeys: String, CodingKey {
        case pulls
        case diskFreeBytes = "disk_free_bytes"
    }
}

/// One event of a streamed answer (`POST /assistant/stream`).
enum ChatEvent {
    case delta(String)
    case reasoning(String)
    case action(AssistantAction)
    /// Throw away the streamed text: it was garbled and a retry follows.
    case reset
    /// The authoritative final reply.
    case done(reply: String, model: String?)
    case failed(String)
}

/// The agent's serving lifecycle (`GET /serve`): what it's doing with the chips.
struct ServeState: Decodable, Equatable {
    let phase: String // idle | starting | serving | stopping | failed
    let model: String?
    let servedId: String?
    let since: Int
    let error: String?
    let kind: String // chat | image | video | speech | tts | other
    /// The box is split: chat on one card, an image model on the other.
    var split: Bool? = nil
    /// How long this model usually takes to load here (while starting).
    var etaS: Int? = nil
    /// The image half of a split box.
    var companion: CompanionState? = nil

    var isStarting: Bool { phase == "starting" }
    var isServing: Bool { phase == "serving" }
    var isFailed: Bool { phase == "failed" }
    /// The best name to show: the catalog name it was started as, else what the server reports.
    var displayModel: String? { (model ?? servedId).map(Brand.shortModel) }

    enum CodingKeys: String, CodingKey {
        case phase, model, since, error, kind, split, companion
        case servedId = "served_id"
        case etaS = "eta_s"
    }
}

/// The companion (second) serving slot of a split box.
struct CompanionState: Decodable, Equatable {
    let phase: String
    let model: String?
    let servedId: String?
    let since: Int
    let error: String?
    var etaS: Int? = nil

    var isStarting: Bool { phase == "starting" }
    var isServing: Bool { phase == "serving" }
    var isFailed: Bool { phase == "failed" }
    var displayModel: String? { (model ?? servedId).map(Brand.shortModel) }

    enum CodingKeys: String, CodingKey {
        case phase, model, since, error
        case servedId = "served_id"
        case etaS = "eta_s"
    }
}

/// `GET /modes`: what the box can be set up to do.
struct BoxMode: Decodable, Equatable, Identifiable {
    let id: String
    let title: String
    let blurb: String
    let icon: String
    let models: [String]
    let ready: Bool
    let needsDownload: [String]
    let active: Bool
    let etaS: Int?
    let note: String?
    enum CodingKeys: String, CodingKey {
        case id, title, blurb, icon, models, ready, active, note
        case needsDownload = "needs_download"
        case etaS = "eta_s"
    }
}

/// `GET /doctor`: Box Doctor's checks.
struct DoctorReport: Decodable, Equatable {
    struct Check: Decodable, Equatable, Hashable {
        let id: String
        let title: String
        let status: String // ok | info | warn | fail
        let detail: String
        let fix: String?
        let command: String?
    }
    let status: String
    let checks: [Check]
}

/// `GET /software`: tt and its tools.
struct SoftwareReport: Decodable, Equatable {
    struct Tt: Decodable, Equatable { let current: String?; let latest: String?; let newer: Bool }
    struct Tool: Decodable, Equatable, Hashable {
        let name: String
        let installed: String
        let golden: String
        let upToDate: Bool
        enum CodingKeys: String, CodingKey {
            case name, installed, golden
            case upToDate = "up_to_date"
        }
    }
    struct System: Decodable, Equatable {
        let target: String
        let installedKmd: String?
        let targetKmd: String?
        let downgrade: Bool
        let advice: String
        enum CodingKeys: String, CodingKey {
            case target, downgrade, advice
            case installedKmd = "installed_kmd", targetKmd = "target_kmd"
        }
    }
    struct Leftover: Decodable, Equatable, Hashable { let path: String; let bytes: Int64 }
    let tt: Tt
    let tools: [Tool]
    let system: System?
    let leftovers: [Leftover]
}

/// `GET /clients`: browser apps the box can run for the household.
struct WebClient: Decodable, Equatable, Identifiable {
    let tool: String
    let name: String
    let available: Bool
    let running: Bool
    let url: String?
    var id: String { tool }
}

/// `GET /coserve`: whether the box can run chat and images side by side.
struct CoserveInfo: Decodable, Equatable {
    struct Choice: Decodable, Equatable, Hashable {
        let name: String
        let ready: Bool
        /// A known problem running it on half the box.
        var note: String? = nil
    }
    struct Choices: Decodable, Equatable {
        let chat: [Choice]
        let image: [Choice]
    }
    struct Layout: Decodable, Equatable {
        let chat: String
        let image: String
    }
    let available: Bool
    let reason: String?
    let split: Bool
    let layout: Layout
    let choices: Choices
}

struct ImageRequest: Encodable {
    let prompt: String
    var negativePrompt: String?
    var numInferenceSteps: Int
    var seed: Int?
    var numberOfImages: Int
    var imageReturnFormat = "JPEG"

    enum CodingKeys: String, CodingKey {
        case prompt, seed
        case negativePrompt = "negative_prompt"
        case numInferenceSteps = "num_inference_steps"
        case numberOfImages = "number_of_images"
        case imageReturnFormat = "image_return_format"
    }
}

struct ImageResult: Decodable {
    let images: [String] // base64
    let generationTime: Double?

    enum CodingKeys: String, CodingKey {
        case images
        case generationTime = "generation_time"
    }
}

struct VideoRequest: Encodable {
    let prompt: String
    var negativePrompt: String?
    var numInferenceSteps: Int
    var seed: Int?

    enum CodingKeys: String, CodingKey {
        case prompt, seed
        case negativePrompt = "negative_prompt"
        case numInferenceSteps = "num_inference_steps"
    }
}

/// A tt-media-server video job.
struct VideoJob: Decodable, Equatable {
    struct JobError: Decodable, Equatable {
        let message: String?
        let detail: String?
        var text: String { message ?? detail ?? "The video failed." }
    }

    let id: String
    let status: String // queued | in_progress | completed | failed | cancelled | cancelling
    let createdAt: Int?
    let completedAt: Int?
    let error: JobError?

    var isActive: Bool { ["queued", "in_progress", "cancelling"].contains(status) }

    enum CodingKeys: String, CodingKey {
        case id, status, error
        case createdAt = "created_at"
        case completedAt = "completed_at"
    }
}

/// What submitting a video returns: a job to poll, or (if the server runs in
/// synchronous mode) the finished MP4 itself.
enum VideoSubmission {
    case job(VideoJob)
    case video(Data)
}

/// One moment of the box, from the agent's recorded history (`/metrics/*`).
struct MetricPoint: Decodable, Identifiable {
    struct Chip: Decodable {
        let tempC: Double?
        let powerW: Double?
        let aiclkMHz: Double?
        enum CodingKeys: String, CodingKey {
            case tempC = "temp_c", powerW = "power_w", aiclkMHz = "aiclk_mhz"
        }
    }

    let t: Int
    let chips: [Chip]
    let genTps: Double?
    let promptTps: Double?
    let running: Int?
    let waiting: Int?
    let kvCache: Double?
    let ttftS: Double?
    let tpotS: Double?

    var id: Int { t }
    var date: Date { Date(timeIntervalSince1970: TimeInterval(t)) }
    var totalPower: Double? {
        let w = chips.compactMap(\.powerW)
        return w.isEmpty ? nil : w.reduce(0, +)
    }

    enum CodingKeys: String, CodingKey {
        case t, chips, running, waiting
        case genTps = "gen_tps", promptTps = "prompt_tps", kvCache = "kv_cache"
        case ttftS = "ttft_s", tpotS = "tpot_s"
    }
}

/// Energy and usage the agent has integrated.
struct MetricTotals: Decodable {
    let day: String
    let energyWhToday: Double
    let tokensToday: Double
    let requestsToday: Int
    let energyWhAll: Double
    let tokensAll: Double
    let requestsAll: Int
    let since: Int

    enum CodingKeys: String, CodingKey {
        case day, since
        case energyWhToday = "energy_wh_today", tokensToday = "tokens_today", requestsToday = "requests_today"
        case energyWhAll = "energy_wh_all", tokensAll = "tokens_all", requestsAll = "requests_all"
    }
}

struct MetricsHistory: Decodable {
    let range: String
    let stepS: Int
    let points: [MetricPoint]
    let totals: MetricTotals
    enum CodingKeys: String, CodingKey { case range, points, totals; case stepS = "step_s" }
}

/// The live inference view the agent scrapes from the model server.
struct InferenceNow: Decodable {
    struct Serving: Decodable {
        let generationTps: Double
        let promptTps: Double
        let requestsRunning: Int
        let requestsWaiting: Int
        let kvCacheUsage: Double
        let ttftAvgS: Double
        let tpotAvgS: Double
        enum CodingKeys: String, CodingKey {
            case generationTps = "generation_tps", promptTps = "prompt_tps"
            case requestsRunning = "requests_running", requestsWaiting = "requests_waiting"
            case kvCacheUsage = "kv_cache_usage", ttftAvgS = "ttft_avg_s", tpotAvgS = "tpot_avg_s"
        }
    }
    struct Media: Decodable {
        let generationsPerMin: Double
        let jobsInProgress: Int
        let completedTotal: Int
        let erroredTotal: Int
        let durationAvgS: Double
        enum CodingKeys: String, CodingKey {
            case generationsPerMin = "generations_per_min", jobsInProgress = "jobs_in_progress"
            case completedTotal = "completed_total", erroredTotal = "errored_total", durationAvgS = "duration_avg_s"
        }
    }
    let label: String
    let phase: String
    let serving: Serving?
    let media: Media?
}

struct MetricsNow: Decodable {
    let point: MetricPoint?
    let inference: InferenceNow?
    let totals: MetricTotals
}

/// Something that happened on the box (`/events`): a download finished, a model
/// is ready, the box came online...
struct BoxEvent: Decodable, Identifiable, Equatable {
    let seq: Int
    let at: Double
    let kind: String
    let title: String
    let body: String
    /// When the event asks for the person's OK: what to answer.
    let data: EventData?
    var id: Int { seq }

    struct EventData: Decodable, Equatable {
        let approval: ApprovalRef?
    }
}

/// An approval named on an event (`data.approval`): enough to answer it.
struct ApprovalRef: Decodable, Equatable {
    let id: String
    let kind: String
    let summary: String
    let canAlways: Bool
    enum CodingKeys: String, CodingKey {
        case id, kind, summary
        case canAlways = "can_always"
    }
}

/// `/notify`: whether the box can send push notifications, and to which phones.
struct NotifySettings: Decodable, Equatable {
    struct Push: Decodable, Equatable {
        struct Device: Decodable, Equatable, Hashable {
            let name: String
            let added: Double
            let tokenHint: String
            enum CodingKeys: String, CodingKey {
                case name, added
                case tokenHint = "token_hint"
            }
        }
        let available: Bool
        let problem: String?
        let devices: [Device]
    }
    let push: Push
    let latestEvent: Int
    enum CodingKeys: String, CodingKey {
        case push
        case latestEvent = "latest_event"
    }
}

/// `POST /notify/devices`'s answer.
struct PushRegistration: Decodable, Equatable {
    let registered: Bool
    let pushAvailable: Bool
    let problem: String?
    enum CodingKeys: String, CodingKey {
        case registered, problem
        case pushAvailable = "push_available"
    }
}

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
    let setup: [Step]
    enum CodingKeys: String, CodingKey {
        case wired, setup
        case canPowerOff = "can_power_off", canReboot = "can_reboot", canSuspend = "can_suspend"
        case startsAtBoot = "starts_at_boot"
    }
    /// Wake-on-LAN can work: a cabled NIC allowed to wake the machine.
    var canWake: Bool { wired.contains { $0.connected && $0.wakeEnabled } }
}

/// A fact the assistant remembers about the user (`/memories`).
struct MemoryItem: Decodable, Identifiable, Equatable {
    let id: String
    let text: String
    let added: Double
}

/// `POST /bench`: how fast the box is with what it's serving.
struct BenchResult: Decodable, Equatable {
    let model: String?
    let imageModel: String?
    let at: Double
    let ttftS: Double?
    let singleTps: Double?
    let singleTokens: Int?
    let batchTps: Double?
    let batchSize: Int
    let imageS: Double?
    let watts: Double?
    let whPerMillionTokens: Double?
    let notes: [String]

    enum CodingKeys: String, CodingKey {
        case model, at, watts, notes
        case imageModel = "image_model", ttftS = "ttft_s", singleTps = "single_tps"
        case singleTokens = "single_tokens", batchTps = "batch_tps", batchSize = "batch_size"
        case imageS = "image_s", whPerMillionTokens = "wh_per_million_tokens"
    }
}

/// `GET /connect`: how other apps reach the box's OpenAI-compatible API.
struct ConnectInfo: Decodable, Equatable {
    struct Address: Decodable, Equatable, Hashable {
        let label: String
        let url: String
    }
    struct Model: Decodable, Equatable, Hashable {
        let id: String
        let kind: String
    }
    let urls: [Address]
    let models: [Model]
    let keys: [AppKeyInfo]
}

/// An app key for the `/v1` API (never the secret, which is shown once).
struct AppKeyInfo: Decodable, Equatable, Identifiable, Hashable {
    let id: String
    let name: String
    let hint: String
    let created: Double
    let lastUsed: Double?
    let requests: Int

    enum CodingKeys: String, CodingKey {
        case id, name, hint, created, requests
        case lastUsed = "last_used"
    }
}

/// A document uploaded for the assistant to search (`/docs`).
struct DocSummary: Decodable, Identifiable, Equatable {
    let id: String
    let name: String
    let added: Double
    let chars: Int
    let passages: Int
}

struct LogsInfo: Decodable {
    let source: String
    let origin: String?
    let lines: [String]
}

struct ChatTurn: Codable, Equatable {
    let role: String // "user" | "assistant"
    let content: String
}

struct AssistantAction: Codable, Identifiable, Equatable {
    struct Args: Codable, Equatable {
        let model: String?
        var prompt: String? = nil
    }

    let id: String
    let tool: String
    let args: Args?
    let status: String // "done" | "needs_confirmation" | "error"
    let summary: String?
    /// Pictures this action drew (`create_image`), fetched from `/creations/{id}`.
    var images: [String]? = nil

    var needsConfirmation: Bool { status == "needs_confirmation" }
}

struct AssistantResponse: Decodable {
    let reply: String
    let actions: [AssistantAction]
    let model: String?
}

struct ConfirmResponse: Decodable {
    let status: String // "started" | "done" | "cancelled"
    let summary: String?
}

private struct ErrorBody: Decodable { let error: String }
private struct Ignored: Decodable {}

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
        case .insecure:
            return "Pair again to connect securely: scan the pairing code shown in TT Station on the box (Settings → Pairing and security)."
        case .http(_, let message): return message
        case .notPaired: return "Pair with the box first."
        case .badHost(let host): return "Can't make an address from \"\(host)\"."
        case let .unreachable(address, reason):
            return "Couldn't reach the box at \(address). \(reason) Make sure this phone is on the same network as the box, and that Local Network access is on in Settings → TT Station."
        }
    }
}

/// Async client for one box's control plane: always HTTPS on the box's TLS port
/// (`https://host:tlsPort`), pinned to the certificate learned from the pairing QR.
/// There is no plain-HTTP path.
struct AgentClient {
    let baseURL: URL
    var token: String?

    /// The URL session, pinned to the box's certificate.
    let session: URLSession

    init(host: String, pin: TLSPin, token: String? = nil) throws {
        guard let url = URL(string: "https://\(Self.authority(host)):\(pin.port)") else {
            throw AgentError.badHost(host)
        }
        self.baseURL = url
        self.token = token
        self.session = PinnedSession.session(sha256: pin.sha256)
    }

    /// A client for a saved box. Throws `AgentError.insecure` for a box paired
    /// before HTTPS (no pin): it has to pair again by QR.
    init(box: SavedBox, token: String?) throws {
        guard let pin = box.pin else { throw AgentError.insecure }
        try self.init(host: box.host, pin: pin, token: token)
    }

    /// A host as it must appear in a URL: IPv6 literals in brackets, with a link-local
    /// zone ("fe80::1%en0") percent-encoded as "%25" so the interface survives.
    static func authority(_ host: String) -> String {
        guard host.contains(":"), !host.hasPrefix("[") else { return host }
        return "[\(host.replacingOccurrences(of: "%", with: "%25"))]"
    }

    /// "10.0.0.122:8765" — what error messages and the UI show for this box.
    var address: String {
        "\(baseURL.host ?? "?"):\(baseURL.port.map(String.init) ?? "?")"
    }

    // MARK: unauthed

    func status() async throws -> BoxStatus {
        try await send("GET", "/status", timeout: 5)
    }

    func models() async throws -> [ModelInfo] {
        let response: ModelsResponse = try await send("GET", "/models", timeout: 10)
        return response.models
    }

    /// Tail of the serving container's log (`GET /logs?source=container`).
    func logs(tail: Int) async throws -> LogsInfo {
        try await send("GET", "/logs", query: [URLQueryItem(name: "source", value: "container"),
                                               URLQueryItem(name: "tail", value: String(tail))],
                       timeout: 10)
    }

    /// The box's model catalog from the official tt CLI. `409` when the box has no tt CLI.
    func ttModels() async throws -> TtCatalog {
        try await send("GET", "/tt/models", timeout: 20)
    }

    func ttPulls() async throws -> PullsInfo {
        try await send("GET", "/tt/pulls", timeout: 10)
    }

    /// Start downloading a model on the box (`tt model pull`, in the background).
    /// Returns `started` or `already_downloaded`.
    func pull(model: String) async throws -> String {
        struct Response: Decodable { let status: String }
        let response: Response = try await send("POST", "/tt/pull", body: ["model": model], authed: true, timeout: 30)
        return response.status
    }

    func cancelPull(model: String) async throws {
        let _: Ignored = try await send("POST", "/tt/pull/cancel", body: ["model": model], authed: true, timeout: 15)
    }

    /// The recorded history for `range` (`5m`, `1h`, `24h`).
    func metricsHistory(range: String) async throws -> MetricsHistory {
        try await send("GET", "/metrics/history", query: [URLQueryItem(name: "range", value: range)], timeout: 15)
    }

    func metricsNow() async throws -> MetricsNow {
        try await send("GET", "/metrics/now", timeout: 10)
    }

    /// Finish pairing (over this already-pinned HTTPS client): the bearer token,
    /// and the certificate the box reports (when it does). `name` and `platform`
    /// are how this phone appears in the box's device list (Settings → Devices);
    /// a box from before device tracking ignores them.
    func pairComplete(pairID: String, code: String, name: String? = nil,
                      platform: String? = nil) async throws -> (token: String, tls: TLSPin?) {
        struct Response: Decodable { let token: String; let tls: TLSPin? }
        var body = ["pair_id": pairID, "code": code]
        if let name, !name.isEmpty { body["name"] = name }
        if let platform, !platform.isEmpty { body["platform"] = platform }
        let response: Response = try await send("POST", "/pair/complete", body: body, timeout: 10)
        return (response.token, response.tls)
    }

    // MARK: authed

    func ask(_ history: [ChatTurn]) async throws -> AssistantResponse {
        struct Body: Encodable { let messages: [ChatTurn] }
        return try await send("POST", "/assistant", body: Body(messages: history), authed: true, timeout: 180)
    }

    func confirm(actionID: String, approve: Bool) async throws -> ConfirmResponse {
        struct Body: Encodable { let action_id: String; let approve: Bool }
        return try await send("POST", "/assistant/confirm",
                              body: Body(action_id: actionID, approve: approve), authed: true, timeout: 120)
    }

    /// `POST /run` blocks until the model is healthy, which can take many minutes;
    /// callers fire this and poll `status()` rather than awaiting it in the UI.
    func run(model: String) async throws {
        let _: Ignored = try await send("POST", "/run", body: ["model": model], authed: true, timeout: 3600)
    }

    func stop() async throws {
        let _: Ignored = try await send("POST", "/stop", body: [String: String](), authed: true, timeout: 120)
    }

    /// `POST /power` with `reset-chips` (keeps pairing; unlike `/reset`, which unpairs everyone).
    func resetChips() async throws {
        let _: Ignored = try await send("POST", "/power", body: ["action": "reset-chips"], authed: true, timeout: 180)
    }

    /// Ask the box's assistant and stream the answer as it's written. `detailed`
    /// asks for thorough Markdown (vs. a short spoken-style reply); `think` lets a
    /// reasoning model think first. Cancel the consuming task to stop generating.
    func streamChat(_ history: [ChatTurn], detailed: Bool, think: Bool, tori: Bool = false) -> AsyncThrowingStream<ChatEvent, Error> {
        struct Body: Encodable { let messages: [ChatTurn]; let style: String; let think: Bool; let persona: String? }
        struct Wire: Decodable {
            let type: String
            let text: String?
            let action: AssistantAction?
            let reply: String?
            let model: String?
            let error: String?
        }
        let request: URLRequest? = {
            guard let token else { return nil }
            var request = URLRequest(url: baseURL.appendingPathComponent("/assistant/stream"), timeoutInterval: 600)
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
            request.httpBody = try? JSONEncoder().encode(
                Body(messages: history, style: detailed ? "detailed" : "concise", think: think,
                     persona: tori ? "tori" : nil))
            return request
        }()
        let address = self.address
        return AsyncThrowingStream { continuation in
            let task = Task {
                guard let request else {
                    continuation.finish(throwing: AgentError.notPaired)
                    return
                }
                do {
                    let (bytes, response) = try await session.bytes(for: request)
                    let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                    guard (200..<300).contains(code) else {
                        var body = ""
                        for try await line in bytes.lines { body += line }
                        let message = (try? JSONDecoder().decode(ErrorBody.self, from: Data(body.utf8)))?.error
                            ?? HTTPURLResponse.localizedString(forStatusCode: code)
                        throw AgentError.http(code, message)
                    }
                    for try await line in bytes.lines {
                        guard line.hasPrefix("data:") else { continue }
                        let payload = Data(line.dropFirst(5).trimmingCharacters(in: .whitespaces).utf8)
                        guard let event = try? JSONDecoder().decode(Wire.self, from: payload) else { continue }
                        switch event.type {
                        case "delta": continuation.yield(.delta(event.text ?? ""))
                        case "reasoning": continuation.yield(.reasoning(event.text ?? ""))
                        case "action": if let a = event.action { continuation.yield(.action(a)) }
                        case "reset": continuation.yield(.reset)
                        case "done": continuation.yield(.done(reply: event.reply ?? "", model: event.model))
                        case "error": continuation.yield(.failed(event.error ?? "Something went wrong."))
                        default: break
                        }
                    }
                    continuation.finish()
                } catch let error as URLError where error.code == .cancelled {
                    continuation.finish(throwing: CancellationError())
                } catch let error as URLError {
                    continuation.finish(throwing: AgentError.unreachable(address: address, reason: error.localizedDescription))
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    // MARK: lifecycle & generation

    func serveState() async throws -> ServeState {
        try await send("GET", "/serve", timeout: 10)
    }

    /// Start serving `model` in the background (`POST /serve`): returns at once;
    /// poll `serveState()` for the outcome. Survives the app going to the background.
    func serve(model: String) async throws {
        let _: Ignored = try await send("POST", "/serve", body: ["model": model], authed: true, timeout: 30)
    }

    func generateImage(_ request: ImageRequest) async throws -> ImageResult {
        try await send("POST", "/generate/image", body: request, authed: true, timeout: 600)
    }

    func submitVideo(_ request: VideoRequest) async throws -> VideoSubmission {
        let (data, type) = try await raw("POST", "/generate/video", body: request, timeout: 900)
        if type.hasPrefix("video/") { return .video(data) }
        return .job(try JSONDecoder().decode(VideoJob.self, from: data))
    }

    func videoJob(id: String) async throws -> VideoJob {
        try await send("GET", "/generate/video/\(id)", authed: true, timeout: 30)
    }

    func downloadVideo(id: String) async throws -> Data {
        try await raw("GET", "/generate/video/\(id)/download", body: Optional<[String: String]>.none, timeout: 600).0
    }

    func cancelVideo(id: String) async throws {
        _ = try await raw("POST", "/generate/video/\(id)/cancel", body: [String: String](), timeout: 30)
    }

    /// A JPEG the assistant drew in chat.
    func creation(id: String) async throws -> Data {
        try await raw("GET", "/creations/\(id)", body: Optional<[String: String]>.none, timeout: 60).0
    }

    /// Benchmark what's serving (30 s to a few minutes).
    func bench() async throws -> BenchResult {
        try await send("POST", "/bench", body: [String: String](), authed: true, timeout: 600)
    }

    func events(since: Int) async throws -> (events: [BoxEvent], latest: Int) {
        struct Response: Decodable { let events: [BoxEvent]; let latest: Int }
        let response: Response = try await send("GET", "/events", query: [URLQueryItem(name: "since", value: String(since))],
                                                authed: true, timeout: 15)
        return (response.events, response.latest)
    }

    func notifySettings() async throws -> NotifySettings {
        try await send("GET", "/notify", authed: true, timeout: 15)
    }

    /// Give the box this phone's APNs device token.
    func registerPush(token: String, sandbox: Bool, name: String) async throws -> PushRegistration {
        struct Body: Encodable { let token: String; let sandbox: Bool; let name: String }
        return try await send("POST", "/notify/devices", body: Body(token: token, sandbox: sandbox, name: name),
                              authed: true, timeout: 15)
    }

    /// Push a test notification; returns how many phones got it.
    func testNotification() async throws -> Int {
        struct Response: Decodable { let sent: Int }
        let response: Response = try await send("POST", "/notify/test", body: [String: String](), authed: true, timeout: 30)
        return response.sent
    }

    func powerInfo() async throws -> PowerInfo {
        try await send("GET", "/power/info", authed: true, timeout: 20)
    }

    /// `suspend`, `reboot` or `shutdown`: the machine goes down shortly after.
    func power(_ action: String) async throws {
        let _: Ignored = try await send("POST", "/power", body: ["action": action], authed: true, timeout: 60)
    }

    func memories() async throws -> [MemoryItem] {
        struct Response: Decodable { let memories: [MemoryItem] }
        let response: Response = try await send("GET", "/memories", authed: true, timeout: 15)
        return response.memories
    }

    func deleteMemory(id: String) async throws {
        _ = try await raw("DELETE", "/memories/\(id)", body: Optional<[String: String]>.none, timeout: 15)
    }

    func clearMemories() async throws {
        _ = try await raw("DELETE", "/memories", body: Optional<[String: String]>.none, timeout: 15)
    }

    // MARK: modes, doctor, software

    func modes() async throws -> [BoxMode] {
        struct Response: Decodable { let modes: [BoxMode] }
        let response: Response = try await send("GET", "/modes", timeout: 30)
        return response.modes
    }

    func startMode(_ id: String) async throws {
        let _: Ignored = try await send("POST", "/modes/\(id)", body: [String: String](), authed: true, timeout: 30)
    }

    func doctor() async throws -> DoctorReport {
        try await send("GET", "/doctor", authed: true, timeout: 90)
    }

    func applyFix(_ fix: String) async throws -> DoctorReport {
        try await send("POST", "/doctor/fix/\(fix)", body: [String: String](), authed: true, timeout: 180)
    }

    /// Build a diagnostics bundle on the box and download it.
    func diagnostics() async throws -> (name: String, data: Data) {
        struct Response: Decodable { let id: String }
        let response: Response = try await send("POST", "/diagnostics", body: [String: String](), authed: true, timeout: 180)
        let data = try await raw("GET", "/diagnostics/\(response.id)", body: Optional<[String: String]>.none, timeout: 120).0
        return ("\(response.id).tar.gz", data)
    }

    func software() async throws -> SoftwareReport {
        try await send("GET", "/software", authed: true, timeout: 180)
    }

    func updateTt() async throws {
        let _: Ignored = try await send("POST", "/software/cli-update", body: [String: String](), authed: true, timeout: 600)
    }

    /// Delete a model from the box; returns bytes freed.
    func removeModel(_ name: String, weights: Bool) async throws -> Int64 {
        struct Response: Decodable { let freedBytes: Int64; enum CodingKeys: String, CodingKey { case freedBytes = "freed_bytes" } }
        let (data, _) = try await raw("DELETE", "/tt/models/\(name)",
                                      query: weights ? [URLQueryItem(name: "weights", value: "true")] : [],
                                      body: Optional<[String: String]>.none, timeout: 300)
        return (try? JSONDecoder().decode(Response.self, from: data))?.freedBytes ?? 0
    }

    func webClients() async throws -> [WebClient] {
        struct Response: Decodable { let clients: [WebClient] }
        let response: Response = try await send("GET", "/clients", authed: true, timeout: 60)
        return response.clients
    }

    /// Start a browser app on the box; returns its address.
    func startWebClient(_ tool: String) async throws -> String {
        struct Response: Decodable { let url: String }
        let response: Response = try await send("POST", "/clients/\(tool)/start", body: [String: String](), authed: true, timeout: 600)
        return response.url
    }

    func stopWebClient(_ tool: String) async throws {
        let _: Ignored = try await send("POST", "/clients/\(tool)/stop", body: [String: String](), authed: true, timeout: 120)
    }

    // MARK: jobs

    func jobs() async throws -> [BoxJob] {
        struct Response: Decodable { let jobs: [BoxJob] }
        let response: Response = try await send("GET", "/jobs", authed: true, timeout: 20)
        return response.jobs
    }

    /// Upload a recording to transcribe, streamed from disk.
    func uploadTranscription(file: URL, name: String, tonight: Bool) async throws -> BoxJob {
        guard let token else { throw AgentError.notPaired }
        var components = URLComponents(url: baseURL.appendingPathComponent("/jobs/transcribe"), resolvingAgainstBaseURL: false)
        components?.queryItems = [URLQueryItem(name: "name", value: name),
                                  URLQueryItem(name: "when", value: tonight ? "tonight" : "now")]
        guard let url = components?.url else { throw AgentError.badHost(address) }
        var request = URLRequest(url: url, timeoutInterval: 3600)
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.upload(for: request, fromFile: file)
        } catch let error as URLError {
            throw AgentError.unreachable(address: address, reason: error.localizedDescription)
        }
        let code = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(code) else {
            let message = (try? JSONDecoder().decode(ErrorBody.self, from: data))?.error
                ?? HTTPURLResponse.localizedString(forStatusCode: code)
            throw AgentError.http(code, message)
        }
        return try JSONDecoder().decode(BoxJob.self, from: data)
    }

    func runJobNow(_ id: String) async throws {
        let _: Ignored = try await send("POST", "/jobs/\(id)/run", body: [String: String](), authed: true, timeout: 15)
    }

    func deleteJob(_ id: String) async throws {
        _ = try await raw("DELETE", "/jobs/\(id)", body: Optional<[String: String]>.none, timeout: 15)
    }

    /// The transcript: `txt`, `srt` or `json`.
    func jobResult(_ id: String, format: String = "txt") async throws -> Data {
        try await raw("GET", "/jobs/\(id)/result", query: [URLQueryItem(name: "format", value: format)],
                      body: Optional<[String: String]>.none, timeout: 60).0
    }

    // MARK: orchard

    func orchard(fresh: Bool = false) async throws -> OrchardStatus {
        try await send("GET", "/orchard", query: fresh ? [URLQueryItem(name: "fresh", value: "1")] : [],
                       authed: true, timeout: 60)
    }

    func orchardSetup() async throws {
        let _: Ignored = try await send("POST", "/orchard/setup", body: [String: String](), authed: true, timeout: 30)
    }

    /// tt-orchard's own preflight. It can import torch to check the reference
    /// environment, so it gets minutes, not seconds.
    func orchardPreflight(model: String, acceptCredentials: Bool) async throws -> OrchardPreflight {
        try await send("POST", "/orchard/preflight",
                       body: OrchardRequest(model: model, stopServing: false, acceptCredentials: acceptCredentials),
                       authed: true, timeout: 300)
    }

    /// Start a bring-up. `stopServing` is the user's consent to stop the model
    /// that's serving (a bring-up needs every chip).
    func startOrchard(model: String, stopServing: Bool, acceptCredentials: Bool) async throws {
        let _: Ignored = try await send("POST", "/orchard/runs",
                                        body: OrchardRequest(model: model, stopServing: stopServing,
                                                             acceptCredentials: acceptCredentials),
                                        authed: true, timeout: 120)
    }

    func orchardRun(_ name: String) async throws -> OrchardRun {
        try await send("GET", "/orchard/runs/\(name)", authed: true, timeout: 30)
    }

    /// `pause`, `resume`, `abort` or `retry`.
    func orchardControl(_ name: String, action: String, stopServing: Bool = false) async throws {
        let _: Ignored = try await send("POST", "/orchard/runs/\(name)/\(action)",
                                        body: OrchardRequest(model: "", stopServing: stopServing, acceptCredentials: false),
                                        authed: true, timeout: 120)
    }

    /// `results`, `risks`, `card`, `publish`, `blocked` or `console`.
    func orchardReport(_ name: String, _ which: String) async throws -> String {
        let data = try await raw("GET", "/orchard/runs/\(name)/report/\(which)",
                                 body: Optional<[String: String]>.none, timeout: 30).0
        return String(decoding: data, as: UTF8.self)
    }

    // MARK: household, schedules, SSH, energy

    func people() async throws -> PeopleList {
        try await send("GET", "/people", authed: true, timeout: 15)
    }

    func addPerson(name: String, me: Bool) async throws -> PeopleList {
        struct Body: Encodable { let name: String; let me: Bool }
        return try await send("POST", "/people", body: Body(name: name, me: me), authed: true, timeout: 15)
    }

    func setMe(_ person: String?) async throws -> PeopleList {
        struct Body: Encodable { let person: String? }
        return try await send("PUT", "/people/me", body: Body(person: person), authed: true, timeout: 15)
    }

    func renamePerson(_ id: String, to name: String) async throws -> PeopleList {
        try await send("PATCH", "/people/\(id)", body: ["name": name], authed: true, timeout: 15)
    }

    func removePerson(_ id: String) async throws -> PeopleList {
        try await send("DELETE", "/people/\(id)", body: Optional<[String: String]>.none, authed: true, timeout: 15)
    }

    func schedules() async throws -> [ModeSchedule] {
        struct R: Decodable { let schedules: [ModeSchedule] }
        let r: R = try await send("GET", "/schedules", authed: true, timeout: 15)
        return r.schedules
    }

    func addSchedule(mode: String, time: String, days: [Int]) async throws -> [ModeSchedule] {
        struct Body: Encodable { let mode: String; let time: String; let days: [Int] }
        struct R: Decodable { let schedules: [ModeSchedule] }
        let r: R = try await send("POST", "/schedules", body: Body(mode: mode, time: time, days: days), authed: true, timeout: 15)
        return r.schedules
    }

    func setSchedule(_ id: String, enabled: Bool) async throws -> [ModeSchedule] {
        struct R: Decodable { let schedules: [ModeSchedule] }
        let r: R = try await send("PATCH", "/schedules/\(id)", body: ["enabled": enabled], authed: true, timeout: 15)
        return r.schedules
    }

    func deleteSchedule(_ id: String) async throws -> [ModeSchedule] {
        struct R: Decodable { let schedules: [ModeSchedule] }
        let r: R = try await send("DELETE", "/schedules/\(id)", body: Optional<[String: String]>.none, authed: true, timeout: 15)
        return r.schedules
    }

    /// `POST /ssh/authorize`: add this phone's SSH public key to the box's
    /// `authorized_keys`, tagged with `label` (no whitespace).
    func authorizeSSH(publicKey: String, label: String) async throws -> SSHAuthorizeResult {
        try await send("POST", "/ssh/authorize", body: ["public_key": publicKey, "label": label],
                       authed: true, timeout: 15)
    }

    /// `DELETE /ssh/authorize`: remove this phone's key from the box. Matched by
    /// the key itself, so it goes even if it was added under another label.
    func revokeSSH(publicKey: String) async throws {
        let _: Ignored = try await send("DELETE", "/ssh/authorize", body: ["public_key": publicKey],
                                        authed: true, timeout: 15)
    }

    /// `GET /ssh/info`: the account, addresses and keys for SSH into the box.
    func sshInfo() async throws -> SSHInfo {
        try await send("GET", "/ssh/info", authed: true, timeout: 15)
    }

    func energy() async throws -> EnergyReport {
        try await send("GET", "/energy", authed: true, timeout: 15)
    }

    /// Agent-driven bring-ups (Codex / Claude Code), newest first.
    func bringupRuns() async throws -> [BringupRun] {
        struct Runs: Decodable { let runs: [BringupRun] }
        let r: Runs = try await send("GET", "/bringup/codex/runs", authed: true, timeout: 20)
        return r.runs
    }

    /// Allow or deny what a bring-up is waiting on.
    func decideBringup(run: String, approval: String, decision: String) async throws {
        let _: Ignored = try await send("POST", "/bringup/codex/runs/\(run)/approvals/\(approval)",
                                        body: ["decision": decision], authed: true, timeout: 20)
    }

    /// A `wss://` WebSocket to `path` on the pinned session, with the bearer token
    /// in the `Authorization` header.
    func webSocket(_ path: String, query: [URLQueryItem] = []) -> URLSessionWebSocketTask? {
        guard var components = URLComponents(url: baseURL.appendingPathComponent(path), resolvingAgainstBaseURL: false)
        else { return nil }
        components.scheme = "wss"
        if !query.isEmpty { components.queryItems = query }
        guard let url = components.url else { return nil }
        var request = URLRequest(url: url)
        if let token { request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        return session.webSocketTask(with: request)
    }

    /// The real tt-toplike, streamed: a WebSocket to `/toplike/term` (see the agent's
    /// `toplike_term.rs`), over `wss://` on the pinned session. The token goes in
    /// the `Authorization` header (`webSocket`), never the URL.
    func toplikeSocket(mode: String, cols: Int, rows: Int) -> URLSessionWebSocketTask? {
        guard token != nil else { return nil }
        let task = webSocket("/toplike/term", query: [
            URLQueryItem(name: "mode", value: mode),
            URLQueryItem(name: "cols", value: String(cols)), URLQueryItem(name: "rows", value: String(rows)),
        ])
        task?.maximumMessageSize = 8 << 20
        return task
    }

    /// The box's own voice (Kokoro on its CPU).
    func voice() async throws -> BoxVoiceStatus {
        try await send("GET", "/voice", authed: true, timeout: 15)
    }

    func setVoice(_ id: String) async throws {
        let _: BoxVoiceSettings = try await send("PUT", "/voice", body: ["voice": id], authed: true, timeout: 15)
    }

    /// `text` spoken by the box: 24 kHz 16-bit mono PCM for `pcm`, else MP3.
    func speak(_ text: String, voice: String?, pcm: Bool = true) async throws -> Data {
        struct Body: Encodable { let text: String; let voice: String?; let format: String }
        return try await raw("POST", "/voice/speak", body: Body(text: text, voice: voice, format: pcm ? "pcm" : "mp3"),
                             timeout: 60).0
    }

    func serveOrchard(_ name: String, stopServing: Bool) async throws {
        let _: Ignored = try await send("POST", "/orchard/runs/\(name)/serve",
                                        body: OrchardRequest(model: "", stopServing: stopServing, acceptCredentials: false),
                                        authed: true, timeout: 120)
    }

    func setOrchardPackaging(_ enabled: Bool) async throws {
        struct Body: Encodable { let enabled: Bool }
        let _: Ignored = try await send("POST", "/orchard/packaging", body: Body(enabled: enabled), authed: true, timeout: 30)
    }

    // MARK: lab

    func lab() async throws -> LabStatus {
        try await send("GET", "/lab", authed: true, timeout: 20)
    }

    func prepareLab(model: String) async throws {
        let _: Ignored = try await send("POST", "/lab/prepare", body: ["model": model], authed: true, timeout: 30)
    }

    /// Run tt-lab, streaming its output. `stopServing` is the user's consent to
    /// stop the model that's serving (Lab needs every chip).
    func runLab(_ run: LabRunRequest) -> AsyncThrowingStream<LabEvent, Error> {
        let request: URLRequest? = {
            guard let token else { return nil }
            var request = URLRequest(url: baseURL.appendingPathComponent("/lab/run"), timeoutInterval: 3600)
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
            request.httpBody = try? JSONEncoder().encode(run)
            return request
        }()
        let address = self.address
        let session = self.session
        return AsyncThrowingStream { continuation in
            let task = Task {
                guard let request else {
                    continuation.finish(throwing: AgentError.notPaired)
                    return
                }
                do {
                    let (bytes, response) = try await session.bytes(for: request)
                    let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                    guard (200..<300).contains(code) else {
                        var body = ""
                        for try await line in bytes.lines { body += line }
                        let message = (try? JSONDecoder().decode(ErrorBody.self, from: Data(body.utf8)))?.error
                            ?? HTTPURLResponse.localizedString(forStatusCode: code)
                        throw AgentError.http(code, message)
                    }
                    for try await line in bytes.lines {
                        guard line.hasPrefix("data:") else { continue }
                        let payload = Data(line.dropFirst(5).trimmingCharacters(in: .whitespaces).utf8)
                        guard let event = try? JSONDecoder().decode(LabEvent.self, from: payload) else { continue }
                        continuation.yield(event)
                    }
                    continuation.finish()
                } catch let error as URLError where error.code == .cancelled {
                    continuation.finish(throwing: CancellationError())
                } catch let error as URLError {
                    continuation.finish(throwing: AgentError.unreachable(address: address, reason: error.localizedDescription))
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    // MARK: other apps

    func connectInfo() async throws -> ConnectInfo {
        try await send("GET", "/connect", authed: true, timeout: 15)
    }

    /// Mint an app key; the returned secret is shown once and never again.
    func createAppKey(name: String) async throws -> (AppKeyInfo, String) {
        struct Response: Decodable { let key: String; let info: AppKeyInfo }
        let response: Response = try await send("POST", "/keys", body: ["name": name], authed: true, timeout: 15)
        return (response.info, response.key)
    }

    func revokeAppKey(id: String) async throws {
        _ = try await raw("DELETE", "/keys/\(id)", body: Optional<[String: String]>.none, timeout: 15)
    }

    // MARK: co-serving

    func coserve() async throws -> CoserveInfo {
        try await send("GET", "/coserve", timeout: 20)
    }

    /// Chat on chips 0-1 and images on 2-3, side by side. Answers at once (`202`);
    /// both come up in the background.
    func startCoserve(chat: String, image: String) async throws {
        let _: Ignored = try await send("POST", "/coserve", body: ["chat": chat, "image": image],
                                        authed: true, timeout: 30)
    }

    // MARK: documents

    func documents() async throws -> [DocSummary] {
        struct Response: Decodable { let documents: [DocSummary] }
        let response: Response = try await send("GET", "/docs", authed: true, timeout: 15)
        return response.documents
    }

    /// Upload a document's text (extracted on the phone) for the assistant to search.
    func addDocument(name: String, text: String) async throws -> DocSummary {
        try await send("POST", "/docs", body: ["name": name, "text": text], authed: true, timeout: 120)
    }

    func deleteDocument(id: String) async throws {
        _ = try await raw("DELETE", "/docs/\(id)", body: Optional<[String: String]>.none, timeout: 15)
    }

    // MARK: plumbing

    /// An authed request whose body may not be JSON (video bytes): returns the body
    /// and its content type, mapping errors like `send` does.
    private func raw<B: Encodable>(_ method: String, _ path: String, query: [URLQueryItem] = [], body: B?,
                                   timeout: TimeInterval) async throws -> (Data, String) {
        guard let token else { throw AgentError.notPaired }
        var components = URLComponents(url: baseURL.appendingPathComponent(path), resolvingAgainstBaseURL: false)
        if !query.isEmpty { components?.queryItems = query }
        guard let url = components?.url else { throw AgentError.badHost(address) }
        var request = URLRequest(url: url, timeoutInterval: timeout)
        request.httpMethod = method
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch let error as URLError {
            throw AgentError.unreachable(address: address, reason: error.localizedDescription)
        }
        let http = response as? HTTPURLResponse
        let code = http?.statusCode ?? 0
        guard (200..<300).contains(code) else {
            let message = (try? JSONDecoder().decode(ErrorBody.self, from: data))?.error
                ?? HTTPURLResponse.localizedString(forStatusCode: code)
            throw AgentError.http(code, message)
        }
        return (data, http?.value(forHTTPHeaderField: "Content-Type") ?? "")
    }

    private func send<T: Decodable>(_ method: String, _ path: String, query: [URLQueryItem] = [],
                                    authed: Bool = false, timeout: TimeInterval) async throws -> T {
        try await send(method, path, query: query, body: Optional<[String: String]>.none,
                       authed: authed, timeout: timeout)
    }

    private func send<T: Decodable, B: Encodable>(_ method: String, _ path: String, query: [URLQueryItem] = [],
                                                  body: B?, authed: Bool = false,
                                                  timeout: TimeInterval) async throws -> T {
        var components = URLComponents(url: baseURL.appendingPathComponent(path), resolvingAgainstBaseURL: false)
        if !query.isEmpty { components?.queryItems = query }
        guard let url = components?.url else { throw AgentError.badHost(address) }
        var request = URLRequest(url: url, timeoutInterval: timeout)
        request.httpMethod = method
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        // The box answers only paired callers (all but `/status` and pairing),
        // so the token goes on every request once there is one.
        if authed, token == nil { throw AgentError.notPaired }
        if let token {
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch let error as URLError {
            throw AgentError.unreachable(address: address, reason: error.localizedDescription)
        }
        let code = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(code) else {
            let message = (try? JSONDecoder().decode(ErrorBody.self, from: data))?.error
                ?? String(data: data, encoding: .utf8).flatMap { $0.isEmpty ? nil : $0 }
                ?? HTTPURLResponse.localizedString(forStatusCode: code)
            throw AgentError.http(code, message)
        }
        return try JSONDecoder().decode(T.self, from: data.isEmpty ? Data("{}".utf8) : data)
    }
}


/// A box's HTTPS port and certificate fingerprint (SHA-256 of the DER), as
/// learned at pairing.
struct TLSPin: Codable, Equatable, Hashable {
    let port: Int
    let sha256: String
}

/// URL sessions that accept exactly one certificate -- the box's own, pinned at
/// pairing -- instead of trusting certificate authorities (a box has none).
enum PinnedSession {
    private static var sessions: [String: URLSession] = [:]
    private static let lock = NSLock()

    static func session(sha256: String) -> URLSession {
        lock.lock()
        defer { lock.unlock() }
        if let existing = sessions[sha256] { return existing }
        let session = URLSession(configuration: .default, delegate: PinningDelegate(sha256: sha256), delegateQueue: nil)
        sessions[sha256] = session
        return session
    }

    /// Whether a certificate (its DER bytes) is the pinned one: its SHA-256, in
    /// hex, equals `sha256` (either case).
    static func matches(certificateDER der: Data, sha256: String) -> Bool {
        SHA256.hash(data: der).map { String(format: "%02x", $0) }.joined() == sha256.lowercased()
    }
}

private final class PinningDelegate: NSObject, URLSessionDelegate {
    let sha256: String
    init(sha256: String) { self.sha256 = sha256.lowercased() }

    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge)
        async -> (URLSession.AuthChallengeDisposition, URLCredential?) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let trust = challenge.protectionSpace.serverTrust,
              let leaf = (SecTrustCopyCertificateChain(trust) as? [SecCertificate])?.first else {
            return (.performDefaultHandling, nil)
        }
        let der = SecCertificateCopyData(leaf) as Data
        return PinnedSession.matches(certificateDER: der, sha256: sha256)
            ? (.useCredential, URLCredential(trust: trust)) : (.cancelAuthenticationChallenge, nil)
    }
}


// MARK: - Lab wire types

struct LabStats: Codable, Equatable {
    struct Stage: Codable, Equatable, Hashable {
        let stage: String
        let cycles: Double?
        let percent: Double?
    }
    var decodeTps: Double?
    var prefillTps: Double?
    var msPerToken: Double?
    var preloadS: Double?
    var dramGib: Double?
    var tokens: Double?
    var check: String?
    var profile: [Stage]?
    enum CodingKeys: String, CodingKey {
        case tokens, check, profile
        case decodeTps = "decode_tps", prefillTps = "prefill_tps", msPerToken = "ms_per_token"
        case preloadS = "preload_s", dramGib = "dram_gib"
    }
}

// MARK: - Household, schedules, SSH, energy

struct Person: Decodable, Identifiable, Hashable {
    let id: String
    let name: String
}

struct PeopleList: Decodable {
    let people: [Person]
    let me: String?
}

struct ModeSchedule: Decodable, Identifiable, Hashable {
    let id: String
    let mode: String
    let time: String
    let days: [Int]
    let enabled: Bool
}

/// `POST /ssh/authorize`'s answer.
struct SSHAuthorizeResult: Decodable {
    let authorized: Bool
    let sshUser: String
    let alreadyPresent: Bool
    enum CodingKeys: String, CodingKey {
        case authorized
        case sshUser = "ssh_user"
        case alreadyPresent = "already_present"
    }
}

/// `GET /ssh/info`: how an SSH app reaches the box.
struct SSHInfo: Decodable {
    struct Host: Decodable, Hashable { let label: String; let host: String }
    struct Key: Decodable, Hashable {
        let label: String
        let type: String
        let added: String?
        /// The stored label, what `DELETE /ssh/authorize` takes.
        let raw: String
    }
    let listening: Bool
    let user: String
    let port: Int
    let hosts: [Host]
    let keys: [Key]
    let command: String?
    let link: String?
    let config: String?
}

struct BringupApproval: Decodable, Identifiable, Hashable {
    let id: String
    let kind: String
    let why: String
    let command: String?
}

struct BringupRun: Decodable, Identifiable {
    let id: String
    let engine: String
    let model: String
    let state: String
    let live: Bool
    let done: Int
    let total: Int
    let current: Int?
    let approvals: [BringupApproval]
    let chipsHeldBy: String?
    let exitMeaning: String?
    /// A stage the agent can't finish without a person's answer.
    let blocked: Blocked?

    struct Blocked: Decodable {
        let stage: Int
        let title: String
        let reason: String?
    }

    enum CodingKeys: String, CodingKey {
        case id, engine, model, state, live, done, total, current, approvals, blocked
        case chipsHeldBy = "chips_held_by"
        case exitMeaning = "exit_meaning"
    }

    var agent: String { engine == "claude" ? "Claude Code" : "Codex" }
    var title: String { model.split(separator: "/").last.map(String.init) ?? model }
    var detail: String {
        switch state {
        case "waiting_for_you": return "\(agent) needs you"
        case "waiting_for_chips": return "Waiting for the chips" + (chipsHeldBy.map { ": \($0) has them" } ?? "")
        case "running": return "\(agent) · stage \(current ?? done + 1) of \(total)"
        case "finished": return "Every stage passed"
        default:
            if let b = blocked { return "\(agent) needs an answer on stage \(b.stage): " + (b.reason ?? b.title) }
            return exitMeaning ?? state.capitalized
        }
    }
}

struct BoxVoice: Decodable, Identifiable, Hashable {
    let id: String
    let name: String
    let accent: String
    let gender: String
    let english: Bool
    let favourite: Bool
}

struct BoxVoiceStatus: Decodable {
    let state: String
    let voice: String
    let speed: Double
    let voices: [BoxVoice]
    var ready: Bool { state == "ready" }
}

struct BoxVoiceSettings: Decodable {
    let voice: String
    let speed: Double
}

struct EnergyReport: Decodable {
    struct Month: Decodable, Identifiable, Hashable {
        let month: String
        let energyKwh: Double
        let tokens: Double
        let requests: Int
        let days: Int
        var id: String { month }
        enum CodingKeys: String, CodingKey { case month, tokens, requests, days; case energyKwh = "energy_kwh" }
    }
    struct Day: Decodable, Identifiable, Hashable {
        let day: String
        let energyKwh: Double
        let tokens: Double
        let requests: Int
        var id: String { day }
        enum CodingKeys: String, CodingKey { case day, tokens, requests; case energyKwh = "energy_kwh" }
    }
    let months: [Month]
    let days: [Day]
}

// MARK: - Orchard

struct OrchardRequest: Encodable {
    let model: String
    let stopServing: Bool
    let acceptCredentials: Bool
    enum CodingKeys: String, CodingKey {
        case model
        case stopServing = "stop_serving"
        case acceptCredentials = "accept_credentials_visible"
    }
}

struct OrchardStep: Decodable, Hashable {
    let name: String
    let status: String // ok | warn | todo | fail
    let detail: String
}

struct OrchardNote: Decodable, Hashable {
    let text: String
    let command: String?
}

struct OrchardStatus: Decodable {
    struct Config: Decodable {
        struct Coder: Decodable { let target: String; let port: Int; let chips: Int }
        let path: String
        let runsRoot: String
        let coder: Coder
        enum CodingKeys: String, CodingKey { case path, coder; case runsRoot = "runs_root" }
    }
    let installed: Bool
    let version: String?
    let ready: Bool
    let setup: [OrchardStep]
    let setupRunning: Bool
    let setupLog: [String]
    let config: Config?
    let configError: String?
    let busy: Bool
    let runs: [OrchardRun]
    let notes: [OrchardNote]
    let packaging: Bool
    enum CodingKeys: String, CodingKey {
        case installed, version, ready, setup, config, busy, runs, notes, packaging
        case setupRunning = "setup_running"
        case setupLog = "setup_log"
        case configError = "config_error"
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        installed = try c.decode(Bool.self, forKey: .installed)
        version = try c.decodeIfPresent(String.self, forKey: .version)
        ready = (try? c.decode(Bool.self, forKey: .ready)) ?? false
        setup = (try? c.decode([OrchardStep].self, forKey: .setup)) ?? []
        setupRunning = (try? c.decode(Bool.self, forKey: .setupRunning)) ?? false
        setupLog = (try? c.decode([String].self, forKey: .setupLog)) ?? []
        config = try? c.decodeIfPresent(Config.self, forKey: .config)
        configError = try? c.decodeIfPresent(String.self, forKey: .configError)
        busy = (try? c.decode(Bool.self, forKey: .busy)) ?? false
        runs = (try? c.decode([OrchardRun].self, forKey: .runs)) ?? []
        notes = (try? c.decode([OrchardNote].self, forKey: .notes)) ?? []
        packaging = (try? c.decode(Bool.self, forKey: .packaging)) ?? false
    }
}

struct OrchardRun: Decodable, Identifiable, Equatable {
    struct Stage: Decodable, Equatable {
        let stage: Int
        let name: String
        let status: String // pass | fail | skipped | running | escalated
        let wallS: Double?
        let attempts: Int?
        enum CodingKeys: String, CodingKey { case stage, name, status, attempts; case wallS = "wall_s" }
    }
    struct Current: Decodable, Equatable { let current: Int?; let name: String? }
    struct Reason: Decodable, Equatable { let code: String?; let reason: String? }
    let runName: String
    let model: String?
    let state: String
    let live: Bool
    let stage: Current?
    let stages: [Stage]
    let blocked: Reason?
    let pause: Reason?
    let hint: String?
    let started: Double
    let restore: String?
    let console: [String]?
    let reports: [String]?
    let progress: OrchardProgress?
    let servable: Bool
    var id: String { runName }
    enum CodingKeys: String, CodingKey {
        case model, state, live, stage, stages, blocked, pause, hint, started, restore, console, reports, progress, servable
        case runName = "run_name"
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        runName = try c.decode(String.self, forKey: .runName)
        model = try? c.decodeIfPresent(String.self, forKey: .model)
        state = try c.decode(String.self, forKey: .state)
        live = (try? c.decode(Bool.self, forKey: .live)) ?? false
        stage = try? c.decodeIfPresent(Current.self, forKey: .stage)
        stages = (try? c.decode([Stage].self, forKey: .stages)) ?? []
        blocked = try? c.decodeIfPresent(Reason.self, forKey: .blocked)
        pause = try? c.decodeIfPresent(Reason.self, forKey: .pause)
        hint = try? c.decodeIfPresent(String.self, forKey: .hint)
        started = (try? c.decode(Double.self, forKey: .started)) ?? 0
        restore = try? c.decodeIfPresent(String.self, forKey: .restore)
        console = try? c.decodeIfPresent([String].self, forKey: .console)
        reports = try? c.decodeIfPresent([String].self, forKey: .reports)
        progress = try? c.decodeIfPresent(OrchardProgress.self, forKey: .progress)
        servable = (try? c.decode(Bool.self, forKey: .servable)) ?? false
    }
    var title: String { model.map(Brand.shortModel) ?? runName }
}

/// What a run is doing now and how far along it is (`orchard_progress.rs`).
struct OrchardProgress: Decodable, Equatable {
    let title: String
    let detail: String
    /// The current step's bar; `nil` when it can't be measured.
    let fraction: Double?
    let etaS: Double?
    /// The whole run, estimated from typical stage times.
    let overall: Double
    let overallEtaS: Double?
    /// Something the owner should do, when there is.
    let needsYou: String?
    /// "Nothing to do…" while it runs.
    let hint: String?
    let typicalS: [Double]
    let estimateNote: String?
    enum CodingKeys: String, CodingKey {
        case title, detail, fraction, overall, hint
        case etaS = "eta_s"
        case overallEtaS = "overall_eta_s"
        case needsYou = "needs_you"
        case typicalS = "typical_s"
        case estimateNote = "estimate_note"
    }

    /// "about 3 h 10 min", "about 12 min".
    static func approx(_ seconds: Double) -> String {
        let m = Int((seconds / 60).rounded())
        if m < 1 { return "under a minute" }
        if m < 60 { return "about \(m) min" }
        return m % 60 == 0 ? "about \(m / 60) h" : "about \(m / 60) h \(m % 60) min"
    }
}

struct OrchardPreflight: Decodable, Equatable {
    struct Check: Decodable, Equatable, Hashable {
        let name: String
        let status: String // ok | warn | block
        let detail: String
        let reason: String?
    }
    let model: String
    let runName: String
    let resuming: Bool
    let local: Bool
    let downloadGb: Double
    let license: String?
    let checks: [Check]
    let blocked: Bool
    enum CodingKeys: String, CodingKey {
        case model, resuming, local, license, checks, blocked
        case runName = "run_name"
        case downloadGb = "download_gb"
    }
}

struct LabStatus: Decodable, Equatable {
    struct Model: Decodable, Equatable, Identifiable {
        struct Preparing: Decodable, Equatable {
            let stage: String
            let bytes: Int64
            let total: Int64?
            let error: String?
        }
        let id: String
        let title: String
        let downloadGb: Double
        let tiles: [Int]
        let downloaded: Bool
        let ready: Bool
        let preparing: Preparing?
        enum CodingKeys: String, CodingKey {
            case id, title, tiles, downloaded, ready, preparing
            case downloadGb = "download_gb"
        }
    }
    struct Job: Decodable, Equatable {
        let model: String
        let stage: String
        let bytes: Int64
        let total: Int64?
        let error: String?
    }
    struct Run: Decodable, Equatable, Identifiable {
        let model: String
        let tiles: Int
        let prompt: String
        let check: Bool
        let ok: Bool
        let seconds: Double
        let at: Double
        let stats: LabStats
        var id: Double { at }
    }
    let installed: Bool
    let models: [Model]
    let job: Job?
    let history: [Run]
    let serving: String?
}

struct LabRunRequest: Encodable {
    let model: String
    let prompt: String
    let tokens: Int
    let tiles: Int
    let check: Bool
    let profile: Bool
    let stopServing: Bool
    enum CodingKeys: String, CodingKey {
        case model, prompt, tokens, tiles, check, profile
        case stopServing = "stop_serving"
    }
}

/// One server-sent event from a Lab run.
struct LabEvent: Decodable {
    let type: String // text | done
    let stream: String?
    let text: String?
    let ok: Bool?
    let seconds: Double?
    let stats: LabStats?
    let previous: String?
}


/// A queued job on the box (`/jobs`).
struct BoxJob: Decodable, Equatable, Identifiable {
    let id: String
    let kind: String
    let title: String
    let when: String // now | tonight
    let status: String // queued | waiting | switching | running | done | failed | cancelled
    let done: Int
    let total: Int
    let durationS: Double?
    let created: Double
    let finished: Double?
    let error: String?
    enum CodingKeys: String, CodingKey {
        case id, kind, title, when, status, done, total, created, finished, error
        case durationS = "duration_s"
    }
    var isActive: Bool { ["queued", "waiting", "switching", "running"].contains(status) }
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
struct CodeSession: Decodable, Identifiable, Equatable {
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
    enum CodingKeys: String, CodingKey {
        case id, name, goal, when, enabled, last
        case nextRun = "next_run"
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

extension AgentClient {
    func approvals() async throws -> [Approval] {
        struct Response: Decodable { let approvals: [Approval] }
        let r: Response = try await send("GET", "/approvals", authed: true, timeout: 15)
        return r.approvals
    }

    func answer(approval id: String, allow: Bool, always: Bool = false) async throws {
        struct Body: Encodable { let id: String; let allow: Bool; let always: Bool }
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/approvals/answer", body: Body(id: id, allow: allow, always: always), authed: true, timeout: 20)
    }

    func codeSessions() async throws -> [CodeSession] {
        struct Response: Decodable { let sessions: [CodeSession] }
        let r: Response = try await send("GET", "/code", authed: true, timeout: 20)
        return r.sessions
    }

    func codeSession(_ id: String, after: Int) async throws -> CodeSessionDetail {
        try await send("GET", "/code/sessions/\(id)", query: [URLQueryItem(name: "after", value: String(after))], authed: true, timeout: 20)
    }

    /// Tell a session something. `interrupt` stops the turn that's going first.
    func sendCode(_ id: String, text: String, interrupt: Bool = false) async throws {
        struct Body: Encodable { let text: String; let interrupt: Bool }
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/code/sessions/\(id)/message", body: Body(text: text, interrupt: interrupt), authed: true, timeout: 60)
    }

    func interruptCode(_ id: String) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/code/sessions/\(id)/interrupt", body: [String: String](), authed: true, timeout: 20)
    }

    func routines() async throws -> (routines: [Routine], running: RoutineRun?) {
        struct Response: Decodable { let routines: [Routine]; let running: RoutineRun? }
        let r: Response = try await send("GET", "/routines", authed: true, timeout: 20)
        return (r.routines, r.running)
    }

    func runRoutine(_ id: String) async throws -> RoutineRun {
        struct Response: Decodable { let run: RoutineRun }
        let r: Response = try await send("POST", "/routines/\(id)/run", body: [String: String](), authed: true, timeout: 20)
        return r.run
    }

    func routineRun(_ id: String) async throws -> RoutineRun {
        struct Response: Decodable { let run: RoutineRun }
        let r: Response = try await send("GET", "/routines/runs/\(id)", authed: true, timeout: 20)
        return r.run
    }

    func stopRoutineRun(_ id: String) async throws {
        struct Ok: Decodable {}
        let _: Ok = try await send("POST", "/routines/runs/\(id)/stop", body: [String: String](), authed: true, timeout: 20)
    }
}

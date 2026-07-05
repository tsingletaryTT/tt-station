import Foundation

public struct BoxRecord: Codable, Equatable {
    public let name: String
    public let host: String
    public let ctrlPort: Int
    public let chips: String
    public let statusRaw: String
    public let apiver: Int
    /// Device mesh topology reported by the box (e.g. `"p300x2"`), used to
    /// rank models by hardware fit. `nil` for older agents that predate this
    /// field or for boxes discovered without it (e.g. some mDNS TXT records).
    public let deviceMesh: String?

    enum CodingKeys: String, CodingKey {
        case name, host, chips, apiver
        case ctrlPort = "ctrl_port"
        case statusRaw = "status"
        case deviceMesh = "device_mesh"
    }

    public init(name: String, host: String, ctrlPort: Int, chips: String, statusRaw: String, apiver: Int, deviceMesh: String? = nil) {
        self.name = name; self.host = host; self.ctrlPort = ctrlPort
        self.chips = chips; self.statusRaw = statusRaw; self.apiver = apiver
        self.deviceMesh = deviceMesh
    }

    /// `host:port` — the identity string every `tt` command keys off of.
    ///
    /// mDNS resolvers hand back hostnames as FQDNs with a trailing `.`
    /// (e.g. `qb2-lab.local.`), but the `tt` CLI keys its stored bearer
    /// token by the exact `--host` string used at pair time, which never
    /// has the dot (`qb2-lab.local:8765`). Stripping a single trailing dot
    /// here keeps mDNS-discovered and manually-entered hosts canonical and
    /// identical, so the app's identity always matches what the CLI stored.
    public var hostPort: String {
        let canonicalHost = host.hasSuffix(".") ? String(host.dropLast()) : host
        return "\(canonicalHost):\(ctrlPort)"
    }

    public var status: ServingStatus? { try? ServingStatus(raw: statusRaw) }
}

public struct Endpoint: Codable, Equatable {
    public let baseURL: String
    public let model: String
    public let requiresKey: Bool

    enum CodingKeys: String, CodingKey {
        case model
        case baseURL = "base_url"
        case requiresKey = "requires_key"
    }

    // Swift's synthesized memberwise initializer for a struct is always
    // `internal`, even when every stored property is `public` — so this
    // couldn't be constructed from outside the module (e.g. the test
    // target's `FakeTTClient`) without an explicit public init. (Declared
    // here, not in an extension: a same-signature init added via extension
    // collides with the compiler's own synthesized memberwise init and is
    // rejected as an "invalid redeclaration".)
    public init(baseURL: String, model: String, requiresKey: Bool) {
        self.baseURL = baseURL; self.model = model; self.requiresKey = requiresKey
    }
}

/// One currently-serving `/v1` endpoint discovered by `tt --json serving`.
///
/// Unlike `Endpoint` (the single endpoint the agent itself last launched),
/// a `ServingEntry` can describe a container this box's agent did *not*
/// start — `source == "external"` marks those (e.g. one launched by
/// tt-studio) so operators can tell agent-launched from externally-launched
/// models. Mirrors `Endpoint`'s snake_case ↔ camelCase CodingKeys approach.
public struct ServingEntry: Codable, Equatable {
    public let model: String
    public let baseURL: String
    public let hostPort: Int
    public let container: String
    /// `"agent"` (launched by this box's agent) or `"external"` (e.g. tt-studio).
    public let source: String

    enum CodingKeys: String, CodingKey {
        case model, container, source
        case baseURL = "base_url"
        case hostPort = "host_port"
    }

    // Explicit public init for the same reason `Endpoint` has one: the
    // synthesized memberwise init is `internal`, so the test target's
    // `FakeTTClient` couldn't construct one without this.
    public init(model: String, baseURL: String, hostPort: Int, container: String, source: String) {
        self.model = model; self.baseURL = baseURL; self.hostPort = hostPort
        self.container = container; self.source = source
    }
}

/// Response from `tt --json serving --host <host:port>` — the list of every
/// currently-serving `/v1` endpoint on a box. Empty when nothing is serving.
public struct ServingList: Codable, Equatable {
    public let serving: [ServingEntry]

    public init(serving: [ServingEntry]) { self.serving = serving }
}

public struct ModelInfo: Codable, Equatable {
    public let name: String
    public let devices: [String]

    public init(name: String, devices: [String]) { self.name = name; self.devices = devices }
}

public struct ModelsResponse: Codable, Equatable {
    public let releaseVersion: String?
    public let models: [ModelInfo]

    enum CodingKeys: String, CodingKey {
        case models
        case releaseVersion = "release_version"
    }

    public init(releaseVersion: String?, models: [ModelInfo]) {
        self.releaseVersion = releaseVersion; self.models = models
    }
}

public struct PairResult: Codable, Equatable {
    public let host: String
    public let paired: Bool

    public init(host: String, paired: Bool) { self.host = host; self.paired = paired }
}

/// Response from `tt --json pair-init <host>`. The agent may also send back
/// `host` in the same payload, but we only need `pair_id` here — the extra
/// key is simply ignored by `JSONDecoder`.
public struct PairInitResult: Codable, Equatable {
    public let pairId: String

    enum CodingKeys: String, CodingKey {
        case pairId = "pair_id"
    }

    public init(pairId: String) { self.pairId = pairId }
}

public struct StatusResponse: Codable, Equatable {
    public let status: String
}

/// Response from `tt --json config --host <host:port>` — the box's resolved
/// serving configuration (profile-derived where applicable). Unauthed, like
/// `models`/`status`/`serving`, so it's safe to fetch regardless of pairing.
public struct BoxConfig: Codable, Equatable {
    public let activeProfile: String?
    public let availableProfiles: [String]
    public let backend: String
    public let servingHost: String
    public let servingPort: Int
    public let servingImage: String?
    public let ttInferenceRepo: String?
    public let ttDevice: String?

    enum CodingKeys: String, CodingKey {
        case backend
        case activeProfile = "active_profile"
        case availableProfiles = "available_profiles"
        case servingHost = "serving_host"
        case servingPort = "serving_port"
        case servingImage = "serving_image"
        case ttInferenceRepo = "tt_inference_repo"
        case ttDevice = "tt_device"
    }

    // Explicit public init for the same reason `Endpoint`/`ServingEntry` have
    // one: the synthesized memberwise init is `internal`, so the test
    // target's `FakeTTClient` couldn't construct one without this.
    public init(
        activeProfile: String?,
        availableProfiles: [String],
        backend: String,
        servingHost: String,
        servingPort: Int,
        servingImage: String?,
        ttInferenceRepo: String?,
        ttDevice: String?
    ) {
        self.activeProfile = activeProfile
        self.availableProfiles = availableProfiles
        self.backend = backend
        self.servingHost = servingHost
        self.servingPort = servingPort
        self.servingImage = servingImage
        self.ttInferenceRepo = ttInferenceRepo
        self.ttDevice = ttDevice
    }
}

/// Response from `tt --json ssh-authorize --host <host:port>`. The CLI also
/// emits `public_key_path`, but we only surface the fields the pair-flow UI
/// needs — the extra key is simply ignored by `JSONDecoder`, same as
/// `PairInitResult` ignoring an echoed `host`.
public struct SshAuthorizeInfo: Codable, Equatable {
    public let authorized: Bool
    public let sshUser: String
    public let alreadyPresent: Bool

    enum CodingKeys: String, CodingKey {
        case authorized
        case sshUser = "ssh_user"
        case alreadyPresent = "already_present"
    }

    public init(authorized: Bool, sshUser: String, alreadyPresent: Bool) {
        self.authorized = authorized; self.sshUser = sshUser; self.alreadyPresent = alreadyPresent
    }
}

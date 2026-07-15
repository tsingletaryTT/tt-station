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
    /// This box's detected primary NIC MAC (`"aa:bb:cc:dd:ee:ff"`), passed
    /// through verbatim from the agent's `/status`/mDNS TXT `mac` field —
    /// mirrors `libttstation::model::BoxRecord.mac` on the Rust side (Task
    /// 3), which the CLI's `tt --json discover`/`status` output already
    /// carries. `nil` when detection failed/didn't run, or for an agent that
    /// predates this field — same back-compat shape as `deviceMesh`. This is
    /// the Wake-on-LAN target `PowerMenuView`/`BoxViewModel.wakeBox()` send.
    public let mac: String?

    enum CodingKeys: String, CodingKey {
        case name, host, chips, apiver, mac
        case ctrlPort = "ctrl_port"
        case statusRaw = "status"
        case deviceMesh = "device_mesh"
    }

    public init(name: String, host: String, ctrlPort: Int, chips: String, statusRaw: String, apiver: Int, deviceMesh: String? = nil, mac: String? = nil) {
        self.name = name; self.host = host; self.ctrlPort = ctrlPort
        self.chips = chips; self.statusRaw = statusRaw; self.apiver = apiver
        self.deviceMesh = deviceMesh
        self.mac = mac
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
    /// Whether the model's weights are already downloaded on the box (so a
    /// serve starts fast instead of triggering a large first-run download).
    /// Decoded defensively — an older agent that omits the field reads as
    /// `false` (matches the Rust `#[serde(default)]`).
    public let downloaded: Bool

    public init(name: String, devices: [String], downloaded: Bool = false) {
        self.name = name; self.devices = devices; self.downloaded = downloaded
    }

    enum CodingKeys: String, CodingKey { case name, devices, downloaded }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        devices = try c.decode([String].self, forKey: .devices)
        downloaded = try c.decodeIfPresent(Bool.self, forKey: .downloaded) ?? false
    }
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

/// One model in the box's catalog — a curated entry describing whether/how a
/// model runs on *this* box, independent of whether it's currently serving.
/// Part of `BoxCatalog`; see that type's doc comment for the three tiers this
/// entry can appear in.
public struct CatalogEntry: Codable, Equatable {
    public let id: String
    public let displayName: String
    public let family: String
    public let size: String?
    public let software: [String]
    public let meshes: [String]
    public let neededHardware: [String]
    public let availableNow: Bool
    /// Whether the model's weights are already downloaded on the box. While
    /// `availableNow` means "the box can serve this" (it's in the live
    /// registry), `downloaded` means "the weights are already on disk, so it
    /// starts fast rather than triggering a large first-run download."
    /// Decoded defensively (missing → `false`).
    public let downloaded: Bool
    public let statusHere: String

    enum CodingKeys: String, CodingKey {
        case id, family, size, software, meshes, downloaded
        case displayName = "display_name"
        case neededHardware = "needed_hardware"
        case availableNow = "available_now"
        case statusHere = "status_here"
    }

    // Explicit public init for the same reason `Endpoint`/`ServingEntry`/
    // `BoxConfig` have one: the synthesized memberwise init is `internal`, so
    // the test target's `FakeTTClient` couldn't construct one without this.
    public init(
        id: String,
        displayName: String,
        family: String,
        size: String?,
        software: [String],
        meshes: [String],
        neededHardware: [String],
        availableNow: Bool,
        downloaded: Bool = false,
        statusHere: String
    ) {
        self.id = id
        self.displayName = displayName
        self.family = family
        self.size = size
        self.software = software
        self.meshes = meshes
        self.neededHardware = neededHardware
        self.availableNow = availableNow
        self.downloaded = downloaded
        self.statusHere = statusHere
    }

    // Custom decode so an older `tt catalog` payload without `downloaded`
    // still decodes (defaulting to `false`), matching the Rust
    // `#[serde(default)]` on the wire type.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        displayName = try c.decode(String.self, forKey: .displayName)
        family = try c.decode(String.self, forKey: .family)
        size = try c.decodeIfPresent(String.self, forKey: .size)
        software = try c.decode([String].self, forKey: .software)
        meshes = try c.decode([String].self, forKey: .meshes)
        neededHardware = try c.decode([String].self, forKey: .neededHardware)
        availableNow = try c.decode(Bool.self, forKey: .availableNow)
        downloaded = try c.decodeIfPresent(Bool.self, forKey: .downloaded) ?? false
        statusHere = try c.decode(String.self, forKey: .statusHere)
    }
}

/// Response from `tt --json catalog --host <host:port>` — the box's curated
/// model catalog, split into three tiers for the model browser:
///   - `runsHere`: models that run on this box's actual mesh right now.
///   - `experimental`: models that might run here but aren't fully verified.
///   - `otherHardware`: models that need hardware this box doesn't have
///     (see each entry's `neededHardware`).
/// Unauthed, like `models`/`status`/`serving`/`config`, so it's safe to fetch
/// regardless of pairing. `catalogAvailable`/`catalogStale` let the UI signal
/// when the curated catalog itself is missing or out of date on the box.
public struct BoxCatalog: Codable, Equatable {
    public let boxMesh: String?
    public let catalogAvailable: Bool
    public let catalogStale: Bool
    public let runsHere: [CatalogEntry]
    public let experimental: [CatalogEntry]
    public let otherHardware: [CatalogEntry]

    enum CodingKeys: String, CodingKey {
        case experimental
        case boxMesh = "box_mesh"
        case catalogAvailable = "catalog_available"
        case catalogStale = "catalog_stale"
        case runsHere = "runs_here"
        case otherHardware = "other_hardware"
    }

    // Explicit public init for the same reason `Endpoint`/`ServingEntry`/
    // `BoxConfig` have one: the synthesized memberwise init is `internal`, so
    // the test target's `FakeTTClient` couldn't construct one without this.
    public init(
        boxMesh: String?,
        catalogAvailable: Bool,
        catalogStale: Bool,
        runsHere: [CatalogEntry],
        experimental: [CatalogEntry],
        otherHardware: [CatalogEntry]
    ) {
        self.boxMesh = boxMesh
        self.catalogAvailable = catalogAvailable
        self.catalogStale = catalogStale
        self.runsHere = runsHere
        self.experimental = experimental
        self.otherHardware = otherHardware
    }
}

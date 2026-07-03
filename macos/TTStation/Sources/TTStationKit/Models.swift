import Foundation

public struct BoxRecord: Codable, Equatable {
    public let name: String
    public let host: String
    public let ctrlPort: Int
    public let chips: String
    public let statusRaw: String
    public let apiver: Int

    enum CodingKeys: String, CodingKey {
        case name, host, chips, apiver
        case ctrlPort = "ctrl_port"
        case statusRaw = "status"
    }

    public init(name: String, host: String, ctrlPort: Int, chips: String, statusRaw: String, apiver: Int) {
        self.name = name; self.host = host; self.ctrlPort = ctrlPort
        self.chips = chips; self.statusRaw = statusRaw; self.apiver = apiver
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

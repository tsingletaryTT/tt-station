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
    public var hostPort: String { "\(host):\(ctrlPort)" }

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

public struct StatusResponse: Codable, Equatable {
    public let status: String
}

import Foundation

/// What `tt-station --json local` reports about Tenstorrent cards attached to THIS Mac.
///
/// The Rust side (`crates/tt-station/src/local.rs`) reads the macOS IORegistry, with no driver
/// needed, then asks the official `tt` CLI which models are right-sized for the device config
/// (`tt model list --hw <config>`). Everything here is read-only observation. Nothing talks to
/// the card itself yet, because that needs the DriverKit extension in `macos/TTStationDriver/`.
///
/// Field names mirror the Rust `Report` / `LocalCard` / `RightSized` structs (snake_case JSON).
public struct LocalReport: Codable, Equatable {
    public let cards: [LocalCard]
    /// The official device-config name for the attached cards (`"p100"`), or `nil` when there
    /// are none, they're mixed, or there's no confirmed mapping ("don't guess").
    public let deviceMesh: String?
    public let officialTT: OfficialTT?
    /// Right-sized models per the official CLI; `nil` when right-sizing didn't run (see `modelsError`).
    public let models: [RightSizedModel]?
    public let modelsError: String?

    enum CodingKeys: String, CodingKey {
        case cards, models
        case deviceMesh = "device_mesh"
        case officialTT = "official_tt"
        case modelsError = "models_error"
    }

    public init(cards: [LocalCard], deviceMesh: String?, officialTT: OfficialTT?, models: [RightSizedModel]?, modelsError: String?) {
        self.cards = cards; self.deviceMesh = deviceMesh; self.officialTT = officialTT
        self.models = models; self.modelsError = modelsError
    }

    /// True when at least one Tenstorrent card is attached. The app only shows "This Mac"
    /// when this holds.
    public var hasCards: Bool { !cards.isEmpty }

    /// One-line summary for the popover / sidebar, e.g. "P100A via Thunderbolt · 2 right-sized models".
    public var summary: String {
        guard let first = cards.first else { return "No Tenstorrent card attached" }
        var parts = [cards.count == 1 ? first.boardLabel : "\(cards.count)× \(first.boardLabel)"]
        if first.tunnelled { parts[0] += " via Thunderbolt" }
        if let models { parts.append("\(models.count) right-sized model\(models.count == 1 ? "" : "s")") }
        return parts.joined(separator: " · ")
    }
}

public struct LocalCard: Codable, Equatable, Identifiable {
    public let name: String
    public let location: String?
    public let vendorID: Int
    public let deviceID: Int
    public let subsystemVendorID: Int
    public let subsystemID: Int
    public let chip: String?
    public let boardType: String?
    public let tunnelled: Bool
    public let link: PCIeLink?
    /// Byte lengths of the memory ranges macOS assigned, in IORegistry order. Not indexed by
    /// BAR number: an unassigned BAR is simply missing.
    public let memoryRanges: [UInt64]

    public var id: String { location ?? name }

    enum CodingKeys: String, CodingKey {
        case name, location, chip, tunnelled, link
        case vendorID = "vendor_id"
        case deviceID = "device_id"
        case subsystemVendorID = "subsystem_vendor_id"
        case subsystemID = "subsystem_id"
        case boardType = "board_type"
        case memoryRanges = "memory_ranges"
    }

    public init(name: String, location: String?, vendorID: Int, deviceID: Int, subsystemVendorID: Int, subsystemID: Int,
                chip: String?, boardType: String?, tunnelled: Bool, link: PCIeLink?, memoryRanges: [UInt64]) {
        self.name = name; self.location = location; self.vendorID = vendorID; self.deviceID = deviceID
        self.subsystemVendorID = subsystemVendorID; self.subsystemID = subsystemID; self.chip = chip
        self.boardType = boardType; self.tunnelled = tunnelled; self.link = link; self.memoryRanges = memoryRanges
    }

    /// "P100A", or "Tenstorrent card" when the board type is unknown.
    public var boardLabel: String { boardType?.uppercased() ?? "Tenstorrent card" }

    /// "Blackhole", "Wormhole", …
    public var chipLabel: String { chip.map { $0.prefix(1).uppercased() + $0.dropFirst() } ?? "Unknown chip" }

    /// "1e52:b140"
    public var pciID: String { String(format: "%04x:%04x", vendorID, deviceID) }

    /// "0x43"
    public var subsystemLabel: String { String(format: "0x%02x", subsystemID) }

    /// "PCIe Gen4 ×4 (≈ 7.9 GB/s)", or "Link down".
    public var linkLabel: String {
        guard let link else { return "Link down" }
        return "PCIe Gen\(link.gen) ×\(link.width)" + (link.approxGBps.map { String(format: " (≈ %.1f GB/s raw)", $0) } ?? "")
    }

    /// "512 MiB · 1 MiB · 16 B"
    public var memorySummary: String {
        memoryRanges.isEmpty ? "none assigned" : memoryRanges.map(Self.formatBytes).joined(separator: " · ")
    }

    /// True when a range big enough to hold a Blackhole 4 GiB TLB window was assigned. Over a
    /// Thunderbolt bridge it typically isn't (BAR4 unassigned), which limits the card to its
    /// 2 MiB windows. That's fine for everything planned so far, and worth saying out loud.
    public var hasLargeWindows: Bool { memoryRanges.contains { $0 >= (UInt64(1) << 32) } }

    static func formatBytes(_ n: UInt64) -> String {
        switch n {
        case (UInt64(1) << 30)...: return "\(n >> 30) GiB"
        case (UInt64(1) << 20)...: return "\(n >> 20) MiB"
        case (UInt64(1) << 10)...: return "\(n >> 10) KiB"
        default: return "\(n) B"
        }
    }
}

public struct PCIeLink: Codable, Equatable {
    public let gen: Int
    public let width: Int
    public init(gen: Int, width: Int) { self.gen = gen; self.width = width }

    /// Raw per-direction link bandwidth in GB/s (per-lane rate after line encoding × lanes).
    /// Before Thunderbolt's own tunnelling limits, which cap real throughput lower.
    public var approxGBps: Double? {
        let perLane: [Int: Double] = [1: 0.25, 2: 0.5, 3: 0.985, 4: 1.969, 5: 3.938]
        return perLane[gen].map { $0 * Double(width) }
    }
}

public struct OfficialTT: Codable, Equatable {
    public let bin: String
    public let version: String
    public init(bin: String, version: String) { self.bin = bin; self.version = version }
}

public struct RightSizedModel: Codable, Equatable, Identifiable {
    public let name: String
    public let modelType: String?
    public let engines: [String]
    public let status: String?
    public let maxContext: Int?

    public var id: String { name }

    enum CodingKeys: String, CodingKey {
        case name, engines, status
        case modelType = "model_type"
        case maxContext = "max_context"
    }

    public init(name: String, modelType: String?, engines: [String], status: String?, maxContext: Int?) {
        self.name = name; self.modelType = modelType; self.engines = engines; self.status = status; self.maxContext = maxContext
    }

    /// "64K context"
    public var contextLabel: String? {
        maxContext.map { $0 >= 1024 ? "\($0 / 1024)K context" : "\($0) context" }
    }
}

/// What this Mac can and can't do with its local card today, derived from the report. It's
/// kept pure so the honesty of the UI's "not yet" list is unit-tested, not left to the view.
public enum LocalCapability: Equatable, Identifiable {
    case detected(String)
    case rightSized(String)
    case rightSizingUnavailable(String)
    case notYet(title: String, reason: String)

    public var id: String {
        switch self {
        case let .detected(s), let .rightSized(s), let .rightSizingUnavailable(s): return s
        case let .notYet(title, _): return title
        }
    }

    public static func list(for report: LocalReport) -> [LocalCapability] {
        guard report.hasCards else { return [] }
        var out: [LocalCapability] = [.detected("Identified from the IORegistry. No driver needed.")]
        if let models = report.models, let tt = report.officialTT {
            out.append(.rightSized("\(models.count) right-sized model\(models.count == 1 ? "" : "s") from the official tt \(tt.version)"))
        } else {
            out.append(.rightSizingUnavailable(report.modelsError ?? "Right-sizing didn't run"))
        }
        out.append(.notYet(title: "Live telemetry",
                           reason: "Temperature, power and clocks come from the chip's ARC firmware, which needs the TTStationDriver extension loaded."))
        out.append(.notYet(title: "Serving on this Mac",
                           reason: "Needs the driver plus a Mac-side serving path. `tt serve` targets Linux hosts today."))
        return out
    }
}

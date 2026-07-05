import Foundation

/// One device's readings from a single `tt-smi -s` telemetry frame.
///
/// `tempC` and `utilization` are optional because the upstream `tt-smi -s`
/// JSON shape is not strictly guaranteed across firmware/tool versions —
/// a missing `telemetry` object, or a missing/renamed key inside it, must
/// decode to `nil` rather than crash or drop the device entirely.
public struct DeviceReading: Equatable {
    public let index: Int
    public let boardType: String
    public let tempC: Double?
    public let utilization: Double?

    public init(index: Int, boardType: String, tempC: Double?, utilization: Double?) {
        self.index = index
        self.boardType = boardType
        self.tempC = tempC
        self.utilization = utilization
    }
}

/// A decoded snapshot of one `/telemetry` WebSocket frame (verbatim `tt-smi -s` JSON).
public struct TelemetrySnapshot: Equatable {
    public let devices: [DeviceReading]

    public init(devices: [DeviceReading]) {
        self.devices = devices
    }

    /// Tolerant, never-throwing decode of a raw `tt-smi -s` JSON frame string.
    ///
    /// The live `/telemetry` stream (Task 10) holds a long-lived WebSocket and feeds
    /// every incoming frame through this decoder. A single malformed/partial frame
    /// (mid-stream truncation, an unexpected schema change, etc.) must never throw or
    /// crash the stream — any failure at any stage collapses to an empty snapshot,
    /// and the caller simply skips rendering that tick.
    public static func decode(_ frame: String) -> TelemetrySnapshot {
        guard let data = frame.data(using: .utf8) else {
            return TelemetrySnapshot(devices: [])
        }
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let deviceInfo = root["device_info"] as? [[String: Any]] else {
            return TelemetrySnapshot(devices: [])
        }

        let devices = deviceInfo.enumerated().map { index, entry -> DeviceReading in
            let boardInfo = entry["board_info"] as? [String: Any]
            let boardType = boardInfo?["board_type"] as? String ?? ""

            let telemetry = entry["telemetry"] as? [String: Any]
            let tempC = Self.parseDouble(telemetry?["asic_temperature"])

            // Utilization field naming varies (and is absent from the canonical
            // frame this task was built against), so we don't guess a key —
            // leave it nil unless a clearly-named field is present. No such key
            // is known yet, so this always resolves to nil for now.
            let utilization = Self.parseDouble(telemetry?["utilization"])

            return DeviceReading(index: index, boardType: boardType, tempC: tempC, utilization: utilization)
        }

        return TelemetrySnapshot(devices: devices)
    }

    /// Reads a JSON value that may be a String ("61.4"), a Double, or an NSNumber
    /// (JSONSerialization commonly hands back NSNumber for JSON numbers) and
    /// coerces it to a Double. Returns nil for anything else, including absent keys.
    private static func parseDouble(_ value: Any?) -> Double? {
        if let string = value as? String {
            return Double(string)
        }
        if let number = value as? NSNumber {
            return number.doubleValue
        }
        return nil
    }
}

// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import Foundation

/// One chip's live readings from a `tt-smi -s` frame. The lite frame carries
/// temperature, power, clock and the thermal limits; the full frame (used by
/// the tt-toplike modes) adds current, voltage, fan and the memory channels.
struct ChipReading: Equatable, Identifiable {
    let index: Int
    let boardType: String?
    /// ASIC (die) temperature, °C -- tt-smi always reports Celsius.
    let tempC: Double?
    let powerW: Double?
    let aiclkMHz: Double?
    /// The hottest GDDR channel, °C: a second sensor to read the ASIC against.
    var memTempC: Double? = nil
    /// The chip slows itself down above this (`therm_trip_l1_limit`), °C.
    var throttleC: Double? = nil
    /// The chip shuts down above this (`thm_limit`), °C.
    var shutdownC: Double? = nil
    // Full frame only.
    var currentA: Double? = nil
    var voltageV: Double? = nil
    var fanPct: Double? = nil
    /// Per GDDR channel (8 on Blackhole): trained, from `DDR_STATUS` nibbles (0x5 or 2).
    var ddrTrained: [Bool] = []
    /// Per GDDR channel temperature, °C (tt-toplike's channel order).
    var gddrTempsC: [Double] = []
    /// GDDR channels present (ENABLED_GDDR), correctable errors per channel pair, uncorrectable total.
    var gddrEnabled: [Bool] = []
    var gddrCorrErrs: [Int?] = []
    var gddrUncorrErrs: Int? = nil
    /// The chip's own limits: TDP (W), TDC (A), top clock (MHz).
    var tdpW: Double? = nil
    var tdcA: Double? = nil
    var fmaxMHz: Double? = nil
    /// ARC firmware heartbeat: ticks about once a second while the firmware is alive.
    var heartbeat: Double? = nil
    /// The p300c board it sits on (two chips per board), and its PCIe address.
    var boardId: String? = nil
    var busId: String? = nil
    /// 14 Tensix columns; false = harvested (ENABLED_TENSIX_COL bit clear).
    var tensixCols: [Bool] = []
    /// Ethernet ports present (ENABLED_ETH); live per port when the mask maps cleanly; live count.
    var ethEnabled: [Bool] = []
    var ethLive: [Bool] = []
    var ethLiveCount: Int? = nil
    /// PCIe lanes in use now (PCIE_USAGE), negotiated width and generation.
    var pcieLanesUsed: Int? = nil
    var pcieWidth: Int? = nil
    var pcieGen: Int? = nil
    /// The four L2 CPUs: present, clock (MHz).
    var l2cpu: [(on: Bool, mhz: Double?)] = []
    /// The lowest power this chip has drawn since the app connected: its idle.
    var idleW: Double? = nil

    var id: Int { index }

    static func == (a: ChipReading, b: ChipReading) -> Bool {
        a.index == b.index && a.tempC == b.tempC && a.powerW == b.powerW && a.currentA == b.currentA
            && a.heartbeat == b.heartbeat && a.gddrTempsC == b.gddrTempsC && a.ddrTrained == b.ddrTrained
            && a.ethLive == b.ethLive && a.idleW == b.idleW
    }
}

/// A program holding a chip open, from tt-smi's process list.
struct ChipProcess: Equatable, Identifiable {
    let pid: Int
    let device: Int
    let cmdline: String
    var id: String { "\(pid):\(device)" }

    /// What a person would call it.
    var name: String {
        if cmdline.contains("tt-smi") { return "tt-smi (TT Station reading telemetry)" }
        if cmdline.contains("container_app_user") || cmdline.contains("uvicorn") { return "inference server (Docker)" }
        if cmdline.contains("tt-lab") || cmdline.contains("gpt-oss") { return "Lab (tt-lab)" }
        if cmdline.contains("orchard") { return "Orchard bring-up" }
        return (cmdline.split(separator: " ").first.map { String($0.split(separator: "/").last ?? $0) }) ?? "pid \(pid)"
    }
}

/// A process on the box, from the full frame's `tt_toplike.processes`.
struct BoxProcess: Equatable, Identifiable {
    let pid: Int
    let name: String
    let cmd: String
    let usesTT: Bool
    let cpuPct: Double
    let memBytes: Double
    var id: Int { pid }
}

/// The served model as tt-toplike sees it (`tt_toplike.inference[0]`).
struct InferenceSnapshot: Equatable {
    let label: String
    /// down | compiling | loading | ready | alarm
    let phase: String
    let progress: Double?
    let generationTps: Double?
    let promptTps: Double?
    let requestsRunning: Int?
    let requestsWaiting: Int?
    let kvCacheUsage: Double?
    let ttftS: Double?
}

/// Temperatures, shown in the unit the owner picked (`tt.mobile.tempUnit`).
/// Readings stay in °C everywhere else.
enum Temp {
    static let key = "tt.mobile.tempUnit"
    static var fahrenheit: Bool { UserDefaults.standard.string(forKey: key) == "f" }
    static var symbol: String { fahrenheit ? "°F" : "°C" }
    static func value(_ celsius: Double) -> Double { fahrenheit ? celsius * 9 / 5 + 32 : celsius }
    /// "36 °C" / "97 °F", or "–".
    static func string(_ celsius: Double?, decimals: Int = 0) -> String {
        guard let c = celsius else { return "–" }
        return String(format: "%.\(decimals)f %@", value(c), symbol)
    }
    /// "36°" / "97°" for tight spots; the unit is shown nearby.
    static func short(_ celsius: Double?) -> String {
        guard let c = celsius else { return "–" }
        return "\(Int(value(c).rounded()))°"
    }
}

/// Live per-chip readings from the agent's `GET /telemetry` (`wss://`, pinned)
/// WebSocket: the lite frame for the Clean dashboard, the full frame (with
/// memory channels, processes and the served model) for the tt-toplike
/// modes. Reconnects every few seconds if the socket drops.
///
/// Readings are only shown as live while frames keep arriving: when the box
/// can't read its chips (a Lab run or an Orchard bring-up owns them, or
/// tt-smi failed) the agent sends an error frame, kept in `unavailable`, and
/// the last readings are marked stale rather than shown as current.
@Observable
final class TelemetryFeed {
    private(set) var chips: [ChipReading] = []
    private(set) var updatedAt: Date?
    /// Why the box isn't reading its chips right now (the agent's error frame).
    private(set) var unavailable: String?
    private(set) var processes: [BoxProcess] = []
    /// Programs holding each chip open (tt-smi's own list).
    private(set) var chipProcesses: [ChipProcess] = []
    /// Each chip's lowest power since connecting, for "activity above idle".
    private var idle: [Int: Double] = [:]
    private(set) var inference: InferenceSnapshot?
    /// The last `historyLength` frames, oldest first, for sparklines.
    private(set) var history: [[ChipReading]] = []
    /// Generation tokens/s over the same frames, for the inference monitor.
    private(set) var tpsHistory: [Double] = []
    static let historyLength = 90
    private var loop: Task<Void, Never>?
    /// The active box's pinned HTTPS client; the socket is `wss://` on its session.
    private var client: AgentClient?
    private(set) var detailed = false

    var temps: [Double] { chips.compactMap(\.tempC) }
    var hottest: Double? { temps.max() }
    var totalPower: Double? {
        let watts = chips.compactMap(\.powerW)
        return watts.isEmpty ? nil : watts.reduce(0, +)
    }

    /// Readings are current: a frame arrived in the last 10 s and the box
    /// isn't reporting that it can't read the chips.
    func isLive(at now: Date = .now) -> Bool {
        guard unavailable == nil, let updatedAt else { return false }
        return now.timeIntervalSince(updatedAt) < 10
    }

    func start(client: AgentClient) {
        self.client = client
        connect()
    }

    /// Switch between the lite and the full frame (reconnects if it changes).
    func setDetailed(_ detailed: Bool) {
        guard detailed != self.detailed else { return }
        self.detailed = detailed
        if client != nil { connect() }
    }

    private func connect() {
        stop()
        chips = []
        history = []
        tpsHistory = []
        unavailable = nil
        guard let client else { return }
        let query: [URLQueryItem] = detailed ? [] : [URLQueryItem(name: "view", value: "lite")]
        loop = Task { [weak self] in
            while !Task.isCancelled {
                guard let socket = client.webSocket("/telemetry", query: query) else { return }
                socket.maximumMessageSize = 4 << 20
                socket.resume()
                while !Task.isCancelled {
                    guard let message = try? await socket.receive() else { break }
                    if case let .string(text) = message {
                        let frame = TelemetryFeed.decodeFrame(text)
                        await MainActor.run { self?.apply(frame) }
                    }
                }
                socket.cancel(with: .goingAway, reason: nil)
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    func stop() {
        loop?.cancel()
        loop = nil
    }

    private func apply(_ frame: Frame) {
        if let error = frame.error {
            unavailable = error
            return
        }
        guard !frame.chips.isEmpty else { return }
        unavailable = nil
        var chips = frame.chips
        for i in chips.indices {
            if let p = chips[i].powerW { idle[chips[i].index] = min(idle[chips[i].index] ?? p, p) }
            chips[i].idleW = idle[chips[i].index]
        }
        self.chips = chips
        processes = frame.processes
        chipProcesses = frame.chipProcesses
        inference = frame.inference
        updatedAt = Date()
        history.append(chips)
        tpsHistory.append(frame.inference?.generationTps ?? 0)
        if history.count > Self.historyLength {
            history.removeFirst(history.count - Self.historyLength)
            tpsHistory.removeFirst(max(0, tpsHistory.count - Self.historyLength))
        }
    }

    /// Chip `index`'s power over the recorded history, oldest first.
    func powerHistory(chip index: Int) -> [Double] {
        history.compactMap { frame in frame.first { $0.index == index }?.powerW }
    }

    struct Frame {
        var chips: [ChipReading] = []
        var error: String?
        var processes: [BoxProcess] = []
        var chipProcesses: [ChipProcess] = []
        var inference: InferenceSnapshot?
    }

    static func decode(_ frame: String) -> [ChipReading] { decodeFrame(frame).chips }

    static func decodeFrame(_ text: String) -> Frame {
        guard let data = text.data(using: .utf8),
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return Frame() }
        if let error = root["error"] as? String, root["device_info"] == nil {
            return Frame(error: error)
        }
        guard let devices = root["device_info"] as? [[String: Any]] else { return Frame() }
        var out = Frame()
        out.chips = devices.enumerated().map { (index, device) -> ChipReading in
            let telemetry = device["telemetry"] as? [String: Any]
            let smbus = device["smbus_telem"] as? [String: Any]
            let limits = device["limits"] as? [String: Any]
            let board = (device["board_info"] as? [String: Any])?["board_type"] as? String
            var chip = ChipReading(index: index, boardType: board,
                                   tempC: number(telemetry?["asic_temperature"]),
                                   powerW: number(telemetry?["power"]),
                                   aiclkMHz: number(telemetry?["aiclk"]))
            chip.throttleC = number(limits?["therm_trip_l1_limit"])
            chip.shutdownC = number(limits?["thm_limit"])
            chip.currentA = number(telemetry?["current"])
            chip.voltageV = number(telemetry?["voltage"])
            chip.fanPct = number(telemetry?["fan_speed"]).flatMap { $0 > 100 ? nil : $0 }
            // GDDR temperature registers (tt-smi: GDDR_0_1_TEMP …): four packed bytes,
            // little-endian, two sensors per channel; all-ones = no reading (as tt-toplike).
            var gddr: [Double] = []
            for reg in ["GDDR_0_1_TEMP", "GDDR_2_3_TEMP", "GDDR_4_5_TEMP", "GDDR_6_7_TEMP"] {
                guard let raw = integer(smbus?[reg]), raw != 0xFFFF_FFFF else { continue }
                let b = (0..<4).map { Double((raw >> ($0 * 8)) & 0xFF) }
                gddr.append(max(b[0], b[1]))
                gddr.append(max(b[2], b[3]))
            }
            chip.gddrTempsC = gddr
            chip.gddrCorrErrs = ["GDDR_0_1_CORR_ERRS", "GDDR_2_3_CORR_ERRS", "GDDR_4_5_CORR_ERRS", "GDDR_6_7_CORR_ERRS"]
                .map { integer(smbus?[$0]).map { Int($0) } }
            chip.gddrUncorrErrs = integer(smbus?["GDDR_UNCORR_ERRS"]).map { Int($0) }
            chip.gddrEnabled = bits(integer(smbus?["ENABLED_GDDR"]), 8)
            chip.tdpW = number(limits?["tdp_limit"])
            chip.tdcA = number(limits?["tdc_limit"])
            chip.fmaxMHz = number(limits?["asic_fmax"])
            chip.heartbeat = number(telemetry?["heartbeat"]) ?? integer(smbus?["TIMER_HEARTBEAT"]).map(Double.init)
            let info = device["board_info"] as? [String: Any]
            chip.boardId = info?["board_id"] as? String
            chip.busId = info?["bus_id"] as? String
            chip.pcieWidth = number(info?["pcie_width"]).map { Int($0) }
            chip.pcieGen = number(info?["pcie_speed"]).map { Int($0) }
            chip.pcieLanesUsed = integer(smbus?["PCIE_USAGE"]).map { Int($0) }
            chip.tensixCols = bits(integer(smbus?["ENABLED_TENSIX_COL"]), 14)
            let enabledEth = integer(smbus?["ENABLED_ETH"])
            chip.ethEnabled = bits(enabledEth, 14)
            if let live = integer(smbus?["ETH_LIVE_STATUS"]) {
                // tt-smi's triage counts the set bits as live links; on this firmware the
                // live bits sit in the upper 16 (bit N+16 = port N). Map per port only when
                // every live bit lands on a port ENABLED_ETH says exists.
                chip.ethLiveCount = live.nonzeroBitCount
                let low = live & 0xFFFF, high = (live >> 16) & 0xFFFF
                let fits = { (m: UInt64) in enabledEth.map { m & ~$0 == 0 } ?? false }
                if low != 0, fits(low) { chip.ethLive = bits(low, 14) }
                else if low == 0, high != 0, fits(high) { chip.ethLive = bits(high, 14) }
            }
            let l2 = integer(smbus?["ENABLED_L2CPU"])
            chip.l2cpu = bits(l2, 4).enumerated().map { (i, on) in (on, integer(smbus?["L2CPUCLK\(i)"]).map(Double.init)) }
            if let max = integer(smbus?["MAX_GDDR_TEMP"]), max > 0, max < 0xFF {
                chip.memTempC = Double(max)
            } else {
                chip.memTempC = gddr.filter { $0 > 0 }.max()
            }
            if let status = integer(smbus?["DDR_STATUS"]) {
                chip.ddrTrained = (0..<8).map { ch in
                    let nibble = (status >> (ch * 4)) & 0xF
                    return nibble == 0x5 || nibble == 2
                }
            }
            return chip
        }
        out.chipProcesses = (root["processes"] as? [[String: Any]] ?? []).compactMap { p in
            guard let pid = (p["pid"] as? NSNumber)?.intValue, let device = (p["device"] as? NSNumber)?.intValue else { return nil }
            return ChipProcess(pid: pid, device: device, cmdline: p["cmdline"] as? String ?? "")
        }
        if let ext = root["tt_toplike"] as? [String: Any] {
            out.processes = (ext["processes"] as? [[String: Any]] ?? []).compactMap { p in
                guard let pid = p["pid"] as? Int else { return nil }
                return BoxProcess(pid: pid, name: p["name"] as? String ?? "?", cmd: p["cmd"] as? String ?? "",
                                  usesTT: p["uses_tt"] as? Bool ?? false, cpuPct: number(p["cpu_pct"]) ?? 0,
                                  memBytes: number(p["mem_bytes"]) ?? 0)
            }
            if let i = (ext["inference"] as? [[String: Any]])?.first {
                let s = i["serving"] as? [String: Any]
                out.inference = InferenceSnapshot(
                    label: i["label"] as? String ?? "model", phase: i["phase"] as? String ?? "down",
                    progress: number(i["progress"]), generationTps: number(s?["generation_tps"]),
                    promptTps: number(s?["prompt_tps"]), requestsRunning: (s?["requests_running"] as? NSNumber)?.intValue,
                    requestsWaiting: (s?["requests_waiting"] as? NSNumber)?.intValue,
                    kvCacheUsage: number(s?["kv_cache_usage"]), ttftS: number(s?["ttft_avg_s"]))
            }
        }
        return out
    }

    private static func bits(_ mask: UInt64?, _ n: Int) -> [Bool] {
        guard let mask else { return [] }
        return (0..<n).map { (mask >> UInt64($0)) & 1 == 1 }
    }

    /// tt-smi reports numbers as strings ("50.2"); accept either.
    private static func number(_ value: Any?) -> Double? {
        if let number = value as? Double { return number }
        if let number = value as? Int { return Double(number) }
        if let text = value as? String { return Double(text.trimmingCharacters(in: .whitespaces)) }
        return nil
    }

    /// "0x55555555" or "1234" or a number.
    private static func integer(_ value: Any?) -> UInt64? {
        if let n = value as? Int { return UInt64(clamping: n) }
        guard let text = (value as? String)?.trimmingCharacters(in: .whitespaces), !text.isEmpty else { return nil }
        if text.hasPrefix("0x") || text.hasPrefix("0X") { return UInt64(text.dropFirst(2), radix: 16) }
        return UInt64(text)
    }
}

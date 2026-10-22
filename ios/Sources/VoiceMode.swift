// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import AVFoundation
import Speech
import SwiftUI
import UIKit

/// The voice conversation's audio, on one `AVAudioEngine` that stays running
/// for the whole conversation:
///
/// - **One session, set once.** `.playAndRecord` in the default mode (not
///   `.voiceChat`: its call processing pumps the output volume up and down),
///   speaker by default, AirPods in A2DP quality. No deactivating between
///   turns, so no route changes or clicks.
/// - **Gapless speech.** `AVSpeechSynthesizer.write` renders each chunk to PCM,
///   converted to one format and queued on a single player node, back to back.
///   The first sentence goes out as soon as it's written; later ones are
///   batched, so there are no pauses between sentences.
/// - **Endpointing from the audio.** The turn ends when you've spoken and
///   then been quiet for a moment (with an adaptive noise floor), not on a
///   fixed timer.
/// - **Real levels.** The orb follows your voice while listening and the
///   reply's audio while speaking.
@MainActor
@Observable
final class VoiceEngine {
    enum Phase: Equatable { case idle, listening, thinking, speaking }

    private(set) var phase: Phase = .idle
    /// 0...1, smoothed: your voice while listening, the reply while speaking.
    private(set) var level: Double = 0
    private(set) var partial = ""
    private(set) var error: String?

    /// Called with what you said when you finish a turn.
    @ObservationIgnored var onUtterance: ((String) -> Void)?
    /// Called when the reply has been spoken to the end.
    @ObservationIgnored var onSpoken: (() -> Void)?

    @ObservationIgnored private let engine = AVAudioEngine()
    @ObservationIgnored private let player = AVAudioPlayerNode()
    @ObservationIgnored private let synthesizer = AVSpeechSynthesizer()
    @ObservationIgnored private let recognizer = SFSpeechRecognizer(locale: Locale.current)
        ?? SFSpeechRecognizer(locale: Locale(identifier: "en-US"))
    @ObservationIgnored private var task: SFSpeechRecognitionTask?
    @ObservationIgnored private let shared = AudioShared()
    @ObservationIgnored private var running = false
    @ObservationIgnored private var endpointTimer: Task<Void, Never>?
    @ObservationIgnored private var observers: [NSObjectProtocol] = []

    // Speech queue.
    @ObservationIgnored private let playFormat = AVAudioFormat(standardFormatWithSampleRate: 24_000, channels: 1)!
    @ObservationIgnored private var chunks: [String] = []
    @ObservationIgnored private var writing = false
    @ObservationIgnored private var scheduled = 0
    @ObservationIgnored private var streamDone = true
    @ObservationIgnored private var spokenUpTo = 0
    @ObservationIgnored private var generation = 0

    /// An iOS voice identifier, or "box:<id>" for the box's own voice.
    @ObservationIgnored var voiceIdentifier: String?
    /// Fetches the box's voice for some text (24 kHz 16-bit mono PCM).
    @ObservationIgnored var boxSpeak: ((String, String) async throws -> Data)?

    static let boxPrefix = "box:"
    private var boxVoice: String? {
        guard let id = voiceIdentifier, id.hasPrefix(Self.boxPrefix), boxSpeak != nil else { return nil }
        return String(id.dropFirst(Self.boxPrefix.count))
    }

    // MARK: lifecycle

    func start() async {
        error = nil
        guard await Self.authorize() else {
            error = "Allow the microphone and speech recognition for TT Station in Settings."
            return
        }
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playAndRecord, mode: .default,
                                    options: [.defaultToSpeaker, .allowBluetoothA2DP, .allowBluetoothHFP])
            try session.setActive(true)
            try startEngine()
            observe()
            UIApplication.shared.isIdleTimerDisabled = true
        } catch {
            self.error = "Couldn't start audio: \(error.localizedDescription)"
        }
    }

    func stop() {
        generation += 1
        endpointTimer?.cancel()
        task?.cancel()
        task = nil
        shared.set(request: nil)
        synthesizer.stopSpeaking(at: .immediate)
        player.stop()
        if running {
            engine.inputNode.removeTap(onBus: 0)
            engine.mainMixerNode.removeTap(onBus: 0)
            engine.stop()
            running = false
        }
        for o in observers { NotificationCenter.default.removeObserver(o) }
        observers = []
        phase = .idle
        level = 0
        UIApplication.shared.isIdleTimerDisabled = false
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    private func startEngine() throws {
        if !engine.attachedNodes.contains(player) { engine.attach(player) }
        engine.connect(player, to: engine.mainMixerNode, format: playFormat)
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        input.removeTap(onBus: 0)
        let shared = self.shared
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
            shared.append(buffer)
            let db = AudioShared.decibels(buffer)
            Task { @MainActor in self?.heard(db) }
        }
        engine.mainMixerNode.removeTap(onBus: 0)
        engine.mainMixerNode.installTap(onBus: 0, bufferSize: 1024, format: nil) { [weak self] buffer, _ in
            let db = AudioShared.decibels(buffer)
            Task { @MainActor in self?.played(db) }
        }
        engine.prepare()
        try engine.start()
        running = true
    }

    private func observe() {
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main) { [weak self] _ in
            // A route change (AirPods in or out) stops the engine: start it again.
            Task { @MainActor in
                guard let self, self.running else { return }
                try? self.startEngine()
            }
        })
        observers.append(center.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) { [weak self] note in
            let type = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt).flatMap(AVAudioSession.InterruptionType.init)
            Task { @MainActor in
                guard let self else { return }
                if type == .began {
                    self.interrupt()
                } else if type == .ended {
                    try? AVAudioSession.sharedInstance().setActive(true)
                    try? self.startEngine()
                }
            }
        })
    }

    // MARK: listening

    @ObservationIgnored private var noiseFloor: Double = -50
    @ObservationIgnored private var spokeAt: Date?
    @ObservationIgnored private var loudAt = Date.distantPast
    @ObservationIgnored private var listenedAt = Date()

    func listen() {
        guard running, let recognizer, recognizer.isAvailable else {
            if running { error = "Speech recognition isn't available right now." }
            return
        }
        generation += 1
        player.stop()
        partial = ""
        spokeAt = nil
        listenedAt = .now
        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        request.addsPunctuation = true
        if recognizer.supportsOnDeviceRecognition { request.requiresOnDeviceRecognition = true }
        task?.cancel()
        task = recognizer.recognitionTask(with: request) { [weak self] result, _ in
            let text = result?.bestTranscription.formattedString
            Task { @MainActor in
                guard let self, self.phase == .listening, let text else { return }
                self.partial = text
            }
        }
        shared.set(request: request)
        phase = .listening
        Haptics.soft()
        endpointTimer?.cancel()
        endpointTimer = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(100))
                self?.checkEndpoint()
            }
        }
    }

    /// Mic level (dBFS) from the input tap.
    private func heard(_ db: Double) {
        guard phase == .listening else { return }
        // Track the room's noise floor; speech is well above it.
        noiseFloor = db < noiseFloor ? db : noiseFloor * 0.995 + db * 0.005
        if db > max(noiseFloor + 12, -45) {
            loudAt = .now
            if spokeAt == nil { spokeAt = .now }
        }
        smooth(to: Self.unit(db))
    }

    /// Output level (dBFS) from the mixer tap.
    private func played(_ db: Double) {
        guard phase == .speaking else { return }
        smooth(to: Self.unit(db))
    }

    private func smooth(to target: Double) {
        // Quick to rise, slow to fall: lively without flicker.
        level += (target - level) * (target > level ? 0.5 : 0.12)
    }

    private func checkEndpoint() {
        guard phase == .listening else { return }
        let now = Date.now
        let words = partial.trimmingCharacters(in: .whitespacesAndNewlines)
        if spokeAt != nil, !words.isEmpty, now.timeIntervalSince(loudAt) > 1.0 {
            finishListening()
        } else if spokeAt == nil, now.timeIntervalSince(listenedAt) > 8 {
            // Nothing said: stop listening until the orb is tapped.
            endListening()
            phase = .idle
        } else if now.timeIntervalSince(listenedAt) > 45 {
            finishListening()
        }
    }

    /// End the turn now and hand over what was heard.
    func finishListening() {
        guard phase == .listening else { return }
        let text = partial.trimmingCharacters(in: .whitespacesAndNewlines)
        endListening()
        if text.isEmpty {
            phase = .idle
        } else {
            phase = .thinking
            Haptics.light()
            onUtterance?(text)
        }
    }

    /// Stop listening without sending anything (muted).
    func cancelListening() {
        guard phase == .listening else { return }
        endListening()
        phase = .idle
    }

    private func endListening() {
        endpointTimer?.cancel()
        shared.set(request: nil)
        task?.finish()
        task = nil
        level = 0
    }

    // MARK: speaking

    /// A reply is about to stream in.
    func beginReply() {
        generation += 1
        chunks = []
        writing = false
        scheduled = 0
        streamDone = false
        spokenUpTo = 0
    }

    /// The reply so far: queue whatever complete sentences are new. The first
    /// sentence goes alone, so speech starts quickly; after that, sentences are
    /// batched into longer chunks.
    func replyText(_ text: String) {
        guard text.count > spokenUpTo else { return }
        let unspoken = String(text.dropFirst(spokenUpTo))
        guard let cut = Self.lastSentenceEnd(in: unspoken) else { return }
        let ready = String(unspoken[..<cut])
        if spokenUpTo == 0 || ready.count >= 140 || ready.contains("\n") {
            spokenUpTo += ready.count
            enqueue(ready)
        }
    }

    func replyFinished(_ text: String) {
        if text.count > spokenUpTo {
            enqueue(String(text.dropFirst(spokenUpTo)))
            spokenUpTo = text.count
        }
        streamDone = true
        drainIfDone()
    }

    /// Stop talking (a tap on the orb, or an interruption).
    func interrupt() {
        generation += 1
        chunks = []
        writing = false
        synthesizer.stopSpeaking(at: .immediate)
        player.stop()
        scheduled = 0
        streamDone = true
        level = 0
        if phase == .speaking || phase == .thinking { phase = .idle }
    }

    private func enqueue(_ markdown: String) {
        let text = VoiceOutput.speakable(markdown)
        guard !text.isEmpty else { return }
        chunks.append(text)
        writeNext()
    }

    private func writeNext() {
        guard !writing, !chunks.isEmpty else { return }
        writing = true
        let text = chunks.removeFirst()
        if let voice = boxVoice, let fetch = boxSpeak {
            // The box speaks: fetch this chunk while the previous one plays.
            let gen = generation
            let format = playFormat
            Task { @MainActor in
                var buffer: AVAudioPCMBuffer?
                do {
                    buffer = Self.pcmBuffer(try await fetch(text, voice), format: format)
                } catch {
                    // Can't reach the box's voice: say it with the phone's instead.
                    guard gen == self.generation else { return }
                    self.writing = false
                    self.chunks.insert(text, at: 0)
                    self.voiceIdentifier = nil
                    self.error = "Tori's voice on the box isn't answering, so the phone is speaking."
                    self.writeNext()
                    return
                }
                guard gen == self.generation else { return }
                if let buffer { self.schedule(buffer, gen: gen) }
                self.writing = false
                self.writeNext()
                self.drainIfDone()
            }
            return
        }
        let utterance = AVSpeechUtterance(string: text)
        utterance.voice = Self.voice(voiceIdentifier)
        utterance.rate = AVSpeechUtteranceDefaultSpeechRate * 1.04
        utterance.preUtteranceDelay = 0
        utterance.postUtteranceDelay = 0
        let gen = generation
        let shared = self.shared
        let format = playFormat
        synthesizer.write(utterance) { [weak self] buffer in
            guard let pcm = buffer as? AVAudioPCMBuffer else { return }
            let last = pcm.frameLength == 0
            let converted = last ? nil : shared.convert(pcm, to: format)
            Task { @MainActor in
                guard let self, gen == self.generation else { return }
                if let converted { self.schedule(converted, gen: gen) }
                if last {
                    self.writing = false
                    self.writeNext()
                    self.drainIfDone()
                }
            }
        }
    }

    private func schedule(_ buffer: AVAudioPCMBuffer, gen: Int) {
        scheduled += 1
        if phase == .thinking {
            phase = .speaking
            Haptics.soft()
        }
        player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
            Task { @MainActor in
                guard let self, gen == self.generation else { return }
                self.scheduled -= 1
                self.drainIfDone()
            }
        }
        if !player.isPlaying { player.play() }
    }

    private func drainIfDone() {
        guard streamDone, !writing, chunks.isEmpty, scheduled == 0 else { return }
        guard phase == .speaking || phase == .thinking else { return }
        level = 0
        phase = .idle
        onSpoken?()
    }

    // MARK: helpers

    /// 16-bit little-endian mono PCM → a float buffer in `format` (same rate).
    static func pcmBuffer(_ data: Data, format: AVAudioFormat) -> AVAudioPCMBuffer? {
        let frames = data.count / 2
        guard frames > 0, let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)),
              let out = buffer.floatChannelData?[0] else { return nil }
        data.withUnsafeBytes { raw in
            let samples = raw.bindMemory(to: Int16.self)
            for i in 0..<frames { out[i] = Float(Int16(littleEndian: samples[i])) / 32768 }
        }
        buffer.frameLength = AVAudioFrameCount(frames)
        return buffer
    }

    /// The end of the last complete sentence (after `.`, `!`, `?` or a line
    /// break, followed by whitespace, so "3.5" doesn't split), or nil.
    static func lastSentenceEnd(in s: String) -> String.Index? {
        var cut: String.Index?
        var i = s.startIndex
        while i < s.endIndex {
            let next = s.index(after: i)
            if "\n.!?".contains(s[i]), next == s.endIndex || s[next].isWhitespace {
                cut = next
            }
            i = next
        }
        return cut
    }

    /// dBFS → 0...1 for drawing.
    static func unit(_ db: Double) -> Double { min(1, max(0, (db + 55) / 45)) }

    static func voice(_ identifier: String?) -> AVSpeechSynthesisVoice? {
        if let identifier, let v = AVSpeechSynthesisVoice(identifier: identifier) { return v }
        return bestVoices().first ?? AVSpeechSynthesisVoice(language: AVSpeechSynthesisVoice.currentLanguageCode())
    }

    /// Installed voices for this language, best first (premium, enhanced, default).
    static func bestVoices() -> [AVSpeechSynthesisVoice] {
        let language = AVSpeechSynthesisVoice.currentLanguageCode()
        let prefix = String(language.prefix(2))
        return AVSpeechSynthesisVoice.speechVoices()
            .filter { $0.language.hasPrefix(prefix) && !$0.voiceTraits.contains(.isNoveltyVoice) }
            .sorted { a, b in
                if a.quality.rawValue != b.quality.rawValue { return a.quality.rawValue > b.quality.rawValue }
                return (a.language == language ? 1 : 0) > (b.language == language ? 1 : 0)
            }
    }

    private static func authorize() async -> Bool {
        let speech = await withCheckedContinuation { c in
            SFSpeechRecognizer.requestAuthorization { c.resume(returning: $0 == .authorized) }
        }
        guard speech else { return false }
        return await AVAudioApplication.requestRecordPermission()
    }
}

/// What the audio threads share with the main actor: the live recognition
/// request (input buffers go to it only while listening) and format converters.
private final class AudioShared: @unchecked Sendable {
    private let lock = NSLock()
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var converters: [String: AVAudioConverter] = [:]

    func set(request: SFSpeechAudioBufferRecognitionRequest?) {
        lock.lock()
        if request == nil { self.request?.endAudio() }
        self.request = request
        lock.unlock()
    }

    func append(_ buffer: AVAudioPCMBuffer) {
        lock.lock()
        let r = request
        lock.unlock()
        r?.append(buffer)
    }

    /// The synthesizer's PCM (often 22.05 kHz Int16) in the player's format.
    func convert(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) -> AVAudioPCMBuffer? {
        if buffer.format == format { return buffer }
        let key = "\(buffer.format.sampleRate)-\(buffer.format.commonFormat.rawValue)-\(buffer.format.channelCount)"
        lock.lock()
        let converter = converters[key] ?? AVAudioConverter(from: buffer.format, to: format)
        converters[key] = converter
        lock.unlock()
        guard let converter else { return nil }
        let ratio = format.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 1024
        guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return nil }
        var fed = false
        var err: NSError?
        converter.convert(to: out, error: &err) { _, status in
            if fed {
                status.pointee = .noDataNow
                return nil
            }
            fed = true
            status.pointee = .haveData
            return buffer
        }
        return err == nil && out.frameLength > 0 ? out : nil
    }

    /// RMS level in dBFS.
    static func decibels(_ buffer: AVAudioPCMBuffer) -> Double {
        let n = Int(buffer.frameLength)
        guard n > 0 else { return -80 }
        var sum: Float = 0
        if let f = buffer.floatChannelData?[0] {
            for i in 0..<n { sum += f[i] * f[i] }
        } else if let s = buffer.int16ChannelData?[0] {
            for i in 0..<n {
                let v = Float(s[i]) / 32768
                sum += v * v
            }
        } else {
            return -80
        }
        return Double(20 * log10(max(sqrt(sum / Float(n)), 1e-5)))
    }
}

enum Haptics {
    static func light() { UIImpactFeedbackGenerator(style: .light).impactOccurred() }
    static func soft() { UIImpactFeedbackGenerator(style: .soft).impactOccurred(intensity: 0.6) }
}

// MARK: - The orb

/// An RGB color that can be blended, for cross-fading between phases.
private struct RGB {
    var r, g, b: Double
    init(_ hex: UInt32) {
        r = Double((hex >> 16) & 0xFF) / 255
        g = Double((hex >> 8) & 0xFF) / 255
        b = Double(hex & 0xFF) / 255
    }
    init(r: Double, g: Double, b: Double) { self.r = r; self.g = g; self.b = b }
    func mix(_ o: RGB, _ t: Double) -> RGB { RGB(r: r + (o.r - r) * t, g: g + (o.g - g) * t, b: b + (o.b - b) * t) }
    func color(_ a: Double) -> Color { Color(red: r, green: g, blue: b, opacity: a) }
}

/// Four colors per phase, from Vesper: teal and mint while listening, purple
/// and pink while thinking, mint and teal while speaking.
private func palette(_ phase: VoiceEngine.Phase) -> [RGB] {
    switch phase {
    case .idle: return [RGB(0x1A8FB2), RGB(0x136B86), RGB(0x395651), RGB(0x46AFD2)]
    case .listening: return [RGB(0x46AFD2), RGB(0x39C6AC), RGB(0x1A8FB2), RGB(0x82EDD9)]
    case .thinking: return [RGB(0x9C91E3), RGB(0x1A8FB2), RGB(0xE34FAA), RGB(0x7E71D0)]
    case .speaking: return [RGB(0x39C6AC), RGB(0x46AFD2), RGB(0x9C91E3), RGB(0x4CE5C9)]
    }
}

/// Everything the orb draws, advanced once per frame by integrating rates
/// over the frame's duration, never computed from absolute time. (Angles
/// computed as `time × rate` jump whenever the rate changes, because the time
/// since 2001 is huge: that was the jitter.) Every value can only change at a
/// bounded speed, so nothing ever jumps.
private final class OrbMotion {
    private var last: Double?
    /// Smoothed, shaped audio level.
    private(set) var level = 0.0
    /// How lively the current phase is (0 calm … 1 busy), eased.
    private(set) var energy = 0.15
    /// Each blob's orbit angle, accumulated.
    private(set) var angles = [0.0, 2.1, 4.2]
    /// The breathing phase, accumulated.
    private(set) var breath = 0.0
    private(set) var colors = palette(.idle)

    func step(t: Double, level target: Double, phase: VoiceEngine.Phase, calm: Bool) {
        guard let previous = last else {
            last = t
            return
        }
        let raw = t - previous
        guard raw > 0 else { return }       // the same frame drawn twice
        last = t
        let dt = min(raw, 1.0 / 30)         // a dropped frame never becomes a leap

        // Level: compressed (quiet speech still shows), slow to rise, slower to fall.
        let shaped = pow(min(max(target, 0), 1), 0.7)
        let k = shaped > level ? 6.0 : 2.5
        level += (shaped - level) * (1 - exp(-dt * k))

        let goal: Double = switch phase {
        case .idle: 0.1
        case .listening: 0.4
        case .thinking: 0.75
        case .speaking: 0.55
        }
        energy += (goal - energy) * (1 - exp(-dt * 1.2))

        let pace = calm ? 0.35 : 1.0
        for i in angles.indices {
            // Slow, alternating directions: a full turn takes 30–60 s.
            let rate = (0.10 + 0.04 * Double(i)) * (0.6 + 0.8 * energy) * pace
            angles[i] += dt * rate * (i.isMultiple(of: 2) ? 1 : -1)
        }
        breath += dt * (0.5 + 0.5 * energy) * pace   // about one breath every 5–10 s

        let f = 1 - exp(-dt * 1.8)
        colors = zip(colors, palette(phase)).map { $0.mix($1, f) }
    }
}

/// A calm, living light. Every layer is a radial gradient that fades to
/// nothing well inside its canvas (twice the orb's size), so there's never an
/// edge. Three soft lobes drift slowly around the center and swell gently with
/// the voice; colors cross-fade between phases.
struct VoiceOrb: View {
    let phase: VoiceEngine.Phase
    /// Read every frame, inside the drawing, so level changes redraw only the orb.
    let level: () -> Double
    var size: CGFloat = 280
    @State private var motion = OrbMotion()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation) { context in
            Canvas { gc, canvas in
                motion.step(t: context.date.timeIntervalSinceReferenceDate, level: level(),
                            phase: phase, calm: reduceMotion)
                let m = motion
                let c = CGPoint(x: canvas.width / 2, y: canvas.height / 2)
                let R = size / 2
                gc.blendMode = .plusLighter

                // The aura: wide and faint, breathing slowly.
                soft(&gc, c, R * (1.35 + 0.10 * m.level + 0.03 * sin(m.breath)),
                     m.colors[0].color(0.10 + 0.08 * m.level))

                // Three lobes on slow orbits close to the center.
                for i in 0..<3 {
                    let drift = R * (0.06 + 0.06 * m.energy)
                    let p = CGPoint(x: c.x + cos(m.angles[i]) * drift, y: c.y + sin(m.angles[i]) * drift)
                    let r = R * (0.66 + 0.035 * sin(m.breath + Double(i) * 2.1) + 0.12 * m.level)
                    soft(&gc, p, r, m.colors[i + 1].color(0.36))
                }

                // The core, a little brighter with the voice.
                soft(&gc, c, R * (0.42 + 0.06 * m.level), Vesper.staticWhite.opacity(0.12 + 0.16 * m.level))
            }
            .frame(width: size * 2, height: size * 2)
        }
        .frame(width: size, height: size)   // lays out at the orb's size; the glow overflows softly
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }

    /// A disc that fades smoothly from `color` at the center to clear at `radius`.
    private func soft(_ gc: inout GraphicsContext, _ center: CGPoint, _ radius: CGFloat, _ color: Color) {
        let rect = CGRect(x: center.x - radius, y: center.y - radius, width: radius * 2, height: radius * 2)
        gc.fill(Path(ellipseIn: rect), with: .radialGradient(
            Gradient(stops: [.init(color: color, location: 0), .init(color: color.opacity(0.55), location: 0.4),
                             .init(color: color.opacity(0.15), location: 0.75), .init(color: .clear, location: 1)]),
            center: center, startRadius: 0, endRadius: radius))
    }
}

/// The whole screen's light: a large, slow glow in the orb's colors behind
/// everything, over Vesper's dot grid.
private struct Ambient: View {
    let phase: VoiceEngine.Phase

    var body: some View {
        ZStack {
            Vesper.stone0
            DotGrid().opacity(0.45)
            RadialGradient(colors: [glow.opacity(0.32), glow.opacity(0.08), .clear],
                           center: UnitPoint(x: 0.5, y: 0.42), startRadius: 0, endRadius: 520)
                .animation(.smooth(duration: 1.2), value: phase)
        }
        .ignoresSafeArea()
    }

    private var glow: Color {
        switch phase {
        case .idle: return Vesper.teal500
        case .listening: return Vesper.teal600
        case .thinking: return Vesper.purple
        case .speaking: return Vesper.mint
        }
    }
}

// MARK: - Voice mode

/// Hands-free conversation with the box, entirely on your own hardware:
/// speech recognition on the phone, the answer from the box's model, speech
/// synthesis on the phone. Tap the orb to talk, to send early, or to interrupt.
struct VoiceModeView: View {
    let session: BoxSession
    @Environment(\.dismiss) private var dismiss
    @State private var engine = VoiceEngine()
    @State private var heard = ""
    @State private var answer = ""
    @State private var muted = false
    @State private var startingChat = false
    @State private var error: String?
    @State private var showVoices = false
    @AppStorage("tt.voice.id") private var voiceID = ""

    var body: some View {
        ZStack {
            Ambient(phase: engine.phase)
            VStack(spacing: 0) {
                topBar
                Spacer(minLength: 8)
                if session.canChat {
                    Button(action: tapOrb) {
                        VoiceOrb(phase: engine.phase, level: { engine.level })
                            .padding(10)
                            .contentShape(Circle())
                    }
                    .buttonStyle(OrbPress())
                    .accessibilityLabel(accessibility)
                    captions
                        .padding(.top, 36)
                } else {
                    noChat
                }
                Spacer(minLength: 8)
                if let message = error ?? engine.error {
                    Text(message)
                        .font(.tt(.footnote))
                        .foregroundStyle(Vesper.warning)
                        .multilineTextAlignment(.center)
                        .padding(.bottom, 12)
                        .transition(.opacity)
                }
                if session.canChat { controls }
            }
            .padding(.horizontal, 24)
            .padding(.bottom, 12)
        }
        .preferredColorScheme(.dark)
        .task {
            engine.boxSpeak = { [session] text, voice in try await session.speak(text, voice: voice) }
            if voiceID.isEmpty, let box = try? await session.voice(), box.ready {
                // The box's own voice is the default once it has one.
                voiceID = VoiceEngine.boxPrefix + box.voice
            }
            engine.voiceIdentifier = voiceID.isEmpty ? nil : voiceID
            engine.onUtterance = { text in ask(text) }
            engine.onSpoken = {
                // A short beat after the answer, then the next turn.
                Task {
                    try? await Task.sleep(for: .milliseconds(350))
                    if engine.phase == .idle && !muted && session.canChat { engine.listen() }
                }
            }
            await engine.start()
            if session.canChat { engine.listen() }
        }
        .onDisappear {
            session.onAnswerText = nil
            if session.isThinking { session.stopGenerating() }
            engine.stop()
        }
        .onChange(of: session.canChat) { _, can in
            if can && engine.phase == .idle && !muted { engine.listen() }
        }
        .sheet(isPresented: $showVoices) {
            VoicePicker(session: session, selection: $voiceID) { engine.voiceIdentifier = $0 }
        }
    }

    // MARK: pieces

    private var topBar: some View {
        HStack(spacing: 12) {
            HStack(spacing: 8) {
                Circle()
                    .fill(stateColor)
                    .frame(width: 7, height: 7)
                    .shadow(color: stateColor.opacity(0.8), radius: 4)
                Text(statusLabel)
                    .font(.ttMono(11, .medium))
                    .tracking(1.1)
                    .foregroundStyle(Vesper.textSecondary)
                    .contentTransition(.opacity)
            }
            .padding(.horizontal, 12)
            .frame(height: 32)
            .glassEffect(.regular, in: .capsule)
            .animation(.smooth(duration: 0.4), value: engine.phase)
            Spacer()
            Text(session.box.name)
                .font(.tt(.subheadline))
                .foregroundStyle(Vesper.textTertiary)
                .lineLimit(1)
        }
        .padding(.top, 10)
    }

    private var captions: some View {
        VStack(spacing: 16) {
            let you = engine.phase == .listening ? engine.partial : heard
            Text(you.isEmpty ? (engine.phase == .listening ? "Go ahead, I'm listening." : " ") : you)
                .font(.tt(.callout))
                .foregroundStyle(Vesper.textTertiary)
                .multilineTextAlignment(.center)
                .lineLimit(3)
                .frame(maxWidth: .infinity)
            ZStack {
                if engine.phase != .listening, !answer.isEmpty {
                    // Pinned to its newest line as it grows: no per-word scroll animation.
                    ScrollView(showsIndicators: false) {
                        Text(VoiceOutput.speakable(answer))
                            .font(.tt(.title3))
                            .lineSpacing(4)
                            .foregroundStyle(Vesper.textPrimary)
                            .multilineTextAlignment(.center)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 12)
                    }
                    .defaultScrollAnchor(.bottom)
                    .mask(LinearGradient(stops: [.init(color: .clear, location: 0), .init(color: .black, location: 0.18),
                                                 .init(color: .black, location: 0.85), .init(color: .clear, location: 1)],
                                         startPoint: .top, endPoint: .bottom))
                    .transition(.opacity)
                }
            }
            .frame(height: 170)
        }
        .frame(maxWidth: 560)
        .animation(.smooth(duration: 0.45), value: engine.phase)
    }

    /// Thumb-reach controls: mute, end, voice.
    private var controls: some View {
        HStack(spacing: 28) {
            ControlButton(icon: muted ? "mic.slash.fill" : "mic.fill",
                          label: muted ? "Unmute" : "Mute", tint: muted ? Vesper.warning : nil) {
                muted.toggle()
                Haptics.light()
                if muted { engine.cancelListening() } else if engine.phase == .idle { engine.listen() }
            }
            Button {
                Haptics.light()
                dismiss()
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 22, weight: .semibold))
                    .foregroundStyle(.white)
                    .frame(width: 68, height: 68)
                    .background(Vesper.error.gradient, in: .circle)
                    .shadow(color: Vesper.error.opacity(0.35), radius: 14, y: 4)
            }
            .buttonStyle(OrbPress())
            .accessibilityLabel("End voice")
            ControlButton(icon: "person.wave.2", label: "Voice", tint: nil) { showVoices = true }
        }
        .padding(.bottom, 4)
    }

    private var noChat: some View {
        VStack(spacing: 16) {
            VoiceOrb(phase: .idle, level: { 0 }, size: 180)
            Text("Voice needs a chat model").font(.tt(.title3))
            Text(session.startingModel != nil
                 ? "A model is loading. Voice starts as soon as it's ready."
                 : "The box is serving \(session.status?.servingModel.map(Brand.shortModel) ?? "nothing") right now.")
                .font(.tt(.callout)).foregroundStyle(Vesper.textTertiary).multilineTextAlignment(.center)
            if session.startingModel == nil {
                Button {
                    Task {
                        startingChat = true
                        defer { startingChat = false }
                        do {
                            if let chat = try await session.modes().first(where: { $0.id == "chat" }) {
                                try await session.startMode(chat)
                            }
                        } catch { self.error = error.localizedDescription }
                    }
                } label: {
                    Label("Switch to chat", systemImage: "bubble.left.and.text.bubble.right")
                }
                .buttonStyle(.glassProminent)
                .disabled(startingChat)
            } else {
                ProgressView()
            }
            Button("Close") { dismiss() }.padding(.top, 8)
        }
    }

    // MARK: state

    private var statusLabel: String {
        if muted && engine.phase == .idle { return "MUTED" }
        switch engine.phase {
        case .idle: return "TAP TO TALK"
        case .listening: return "LISTENING"
        case .thinking: return "THINKING"
        case .speaking: return "SPEAKING"
        }
    }

    private var stateColor: Color {
        switch engine.phase {
        case .idle: return muted ? Vesper.warning : Vesper.stone600
        case .listening: return Vesper.mint
        case .thinking: return Vesper.purple
        case .speaking: return Vesper.teal700
        }
    }

    private var accessibility: String {
        switch engine.phase {
        case .idle: return "Talk"
        case .listening: return "Send"
        case .thinking: return "Thinking"
        case .speaking: return "Interrupt"
        }
    }

    private func tapOrb() {
        Haptics.light()
        muted = false
        switch engine.phase {
        case .idle: engine.listen()
        case .listening: engine.finishListening()
        case .thinking, .speaking:
            if session.isThinking { session.stopGenerating() }
            engine.interrupt()
            engine.listen()
        }
    }

    private func ask(_ text: String) {
        heard = text
        withAnimation(.smooth) { answer = "" }
        engine.beginReply()
        session.onAnswerText = { text in
            answer = text
            engine.replyText(text)
        }
        Task {
            let reply = await session.send(text, spoken: true)
            session.onAnswerText = nil
            if let reply { answer = reply }
            engine.replyFinished(answer)
        }
    }
}

/// A gentle press: shrinks a touch and springs back.
private struct OrbPress: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.94 : 1)
            .animation(.spring(response: 0.3, dampingFraction: 0.7), value: configuration.isPressed)
    }
}

private struct ControlButton: View {
    let icon: String
    let label: String
    let tint: Color?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 6) {
                Image(systemName: icon)
                    .font(.system(size: 20, weight: .semibold))
                    .foregroundStyle(tint ?? Vesper.textPrimary)
                    .frame(width: 56, height: 56)
                    .glassEffect(.regular.interactive(), in: .circle)
                    .contentTransition(.symbolEffect(.replace))
                Text(label).font(.ttMono(10, .medium)).foregroundStyle(Vesper.textTertiary)
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
    }
}

/// Pick the voice the box speaks with: the box's own natural voices (Kokoro,
/// on its CPU), or one of the phone's.
private struct VoicePicker: View {
    let session: BoxSession
    @Binding var selection: String
    let onPick: (String?) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var synth = AVSpeechSynthesizer()
    @State private var box: BoxVoiceStatus?
    @State private var player: AVAudioPlayer?
    private let sample = "Hi, I'm Tori. This is how I sound."

    var body: some View {
        NavigationStack {
            List {
                Section {
                    if let box, box.ready {
                        ForEach(box.voices.filter(\.english)) { voice in
                            let id = VoiceEngine.boxPrefix + voice.id
                            row(title: voice.name + (voice.favourite ? " ★" : ""),
                                detail: "\(voice.accent) \(voice.gender)", selected: selection == id) {
                                selection = id
                                onPick(id)
                                synth.stopSpeaking(at: .immediate)
                                Task {
                                    try? await session.setVoice(voice.id)
                                    if let data = try? await session.speak(sample, voice: voice.id, pcm: false) {
                                        player = try? AVAudioPlayer(data: data)
                                        player?.play()
                                    }
                                }
                            }
                        }
                    } else if box != nil {
                        Text("Set up Tori's voice in the desktop app (Settings → Tori's voice).")
                            .font(.tt(.footnote)).foregroundStyle(.secondary)
                    } else {
                        ProgressView()
                    }
                } header: {
                    Text("Tori's voice (on your box)")
                } footer: {
                    Text("Natural voices that run on your box, not on the phone.")
                }
                Section {
                    ForEach(VoiceEngine.bestVoices(), id: \.identifier) { voice in
                        row(title: voice.name, detail: "\(quality(voice)) · \(voice.language)",
                            selected: voice.identifier == selection) {
                            selection = voice.identifier
                            onPick(voice.identifier)
                            player?.stop()
                            synth.stopSpeaking(at: .immediate)
                            let u = AVSpeechUtterance(string: sample)
                            u.voice = voice
                            synth.speak(u)
                        }
                    }
                } header: {
                    Text("On this phone")
                } footer: {
                    Text("Download Premium voices in iOS Settings → Accessibility → Spoken Content → Voices.")
                }
            }
            .navigationTitle("Voice")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task { box = (try? await session.voice()) ?? BoxVoiceStatus(state: "unavailable", voice: "", speed: 1, voices: []) }
        }
        .presentationDetents([.medium, .large])
    }

    private func row(title: String, detail: String, selected: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                    Text(detail).font(.tt(.caption)).foregroundStyle(.secondary)
                }
                Spacer()
                if selected { Image(systemName: "checkmark").foregroundStyle(Brand.accent) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func quality(_ v: AVSpeechSynthesisVoice) -> String {
        switch v.quality {
        case .premium: return "Premium"
        case .enhanced: return "Enhanced"
        default: return "Standard"
        }
    }
}

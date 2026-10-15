import AVFoundation
import Speech
import SwiftUI

/// Talk to your box: on-device speech recognition in, the system voice out.
/// Speech never leaves the phone as audio -- only the transcript goes to the box,
/// the same as typing it.
@MainActor
@Observable
final class VoiceInput {
    enum State: Equatable {
        case idle, listening, denied(String)
    }

    private(set) var state: State = .idle
    private(set) var transcript = ""
    /// 0...1 microphone level, for the listening animation.
    private(set) var level: Double = 0

    private let recognizer = SFSpeechRecognizer(locale: Locale.current) ?? SFSpeechRecognizer(locale: Locale(identifier: "en-US"))
    private let engine = AVAudioEngine()
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private var silenceTimer: Task<Void, Never>?
    private var onFinish: ((String) -> Void)?

    /// How long a pause ends the utterance.
    private let silence: Duration = .seconds(1.6)

    var isListening: Bool { state == .listening }

    /// Start listening; `onFinish` gets the final transcript when the speaker
    /// pauses (or `stop()` is called). An empty transcript is not delivered.
    func start(onFinish: @escaping (String) -> Void) async {
        guard state != .listening else { return }
        guard await Self.authorize() else {
            state = .denied("Allow Microphone and Speech Recognition for TT Station in Settings to talk to your box.")
            return
        }
        guard let recognizer, recognizer.isAvailable else {
            state = .denied("Speech recognition isn't available right now.")
            return
        }
        self.onFinish = onFinish
        transcript = ""
        do {
            let session = AVAudioSession.sharedInstance()
            // The same setup the voice mode speaks with (`VoiceEngine`), so
            // listening after an answer doesn't have to switch categories.
            try session.setCategory(.playAndRecord, mode: .voiceChat,
                                    options: [.duckOthers, .defaultToSpeaker, .allowBluetoothHFP])
            try session.setActive(true, options: .notifyOthersOnDeactivation)

            let request = SFSpeechAudioBufferRecognitionRequest()
            request.shouldReportPartialResults = true
            request.addsPunctuation = true
            if recognizer.supportsOnDeviceRecognition { request.requiresOnDeviceRecognition = true }
            self.request = request

            let input = engine.inputNode
            let format = input.outputFormat(forBus: 0)
            input.removeTap(onBus: 0)
            input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
                request.append(buffer)
                let level = Self.rms(buffer)
                Task { @MainActor in self?.level = level }
            }
            engine.prepare()
            try engine.start()
            state = .listening

            task = recognizer.recognitionTask(with: request) { [weak self] result, error in
                let text = result?.bestTranscription.formattedString
                let final = result?.isFinal ?? false
                Task { @MainActor in
                    guard let self, self.state == .listening else { return }
                    if let text {
                        self.transcript = text
                        self.armSilenceTimer()
                    }
                    if final || error != nil { self.stop() }
                }
            }
            armSilenceTimer(initial: true)
        } catch {
            teardown()
            state = .denied("Couldn't start the microphone: \(error.localizedDescription)")
        }
    }

    /// Stop listening and deliver what was heard.
    func stop() {
        guard state == .listening else { return }
        let heard = transcript.trimmingCharacters(in: .whitespacesAndNewlines)
        let deliver = onFinish
        teardown()
        state = .idle
        if !heard.isEmpty { deliver?(heard) }
    }

    /// Stop without delivering anything.
    func cancel() {
        teardown()
        state = .idle
        transcript = ""
    }

    func clearError() {
        if case .denied = state { state = .idle }
    }

    private func armSilenceTimer(initial: Bool = false) {
        silenceTimer?.cancel()
        // Give a slow starter longer before the first word.
        let wait = initial ? Duration.seconds(6) : silence
        silenceTimer = Task { [weak self] in
            try? await Task.sleep(for: wait)
            guard !Task.isCancelled else { return }
            self?.stop()
        }
    }

    private func teardown() {
        silenceTimer?.cancel()
        silenceTimer = nil
        if engine.isRunning { engine.stop() }
        engine.inputNode.removeTap(onBus: 0)
        request?.endAudio()
        task?.cancel()
        request = nil
        task = nil
        level = 0
        onFinish = nil
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    private static func authorize() async -> Bool {
        let speech = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0 == .authorized) }
        }
        guard speech else { return false }
        return await AVAudioApplication.requestRecordPermission()
    }

    nonisolated private static func rms(_ buffer: AVAudioPCMBuffer) -> Double {
        guard let data = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
        let n = Int(buffer.frameLength)
        var sum: Float = 0
        for i in 0..<n { sum += data[i] * data[i] }
        let rms = sqrt(sum / Float(n))
        // Speech sits around 0.01-0.2; map to 0...1 on a log-ish curve.
        return Double(min(1, max(0, (20 * log10(max(rms, 1e-5)) + 50) / 40)))
    }
}

/// Reads answers aloud.
@MainActor
@Observable
final class VoiceOutput: NSObject, AVSpeechSynthesizerDelegate {
    private let synthesizer = AVSpeechSynthesizer()
    private(set) var isSpeaking = false
    private var onDone: (() -> Void)?

    override init() {
        super.init()
        synthesizer.delegate = self
    }

    func speak(_ markdown: String, then onDone: (() -> Void)? = nil) {
        let text = Self.speakable(markdown)
        guard !text.isEmpty else { onDone?(); return }
        try? AVAudioSession.sharedInstance().setCategory(.playback, mode: .spokenAudio, options: [.duckOthers])
        try? AVAudioSession.sharedInstance().setActive(true)
        synthesizer.stopSpeaking(at: .immediate)
        self.onDone = onDone
        let utterance = AVSpeechUtterance(string: text)
        utterance.voice = Self.bestVoice()
        utterance.rate = AVSpeechUtteranceDefaultSpeechRate * 1.05
        isSpeaking = true
        synthesizer.speak(utterance)
    }

    func stop() {
        onDone = nil
        synthesizer.stopSpeaking(at: .immediate)
        isSpeaking = false
    }

    nonisolated func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didFinish utterance: AVSpeechUtterance) {
        Task { @MainActor in
            self.isSpeaking = false
            let done = self.onDone
            self.onDone = nil
            done?()
        }
    }

    nonisolated func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didCancel utterance: AVSpeechUtterance) {
        Task { @MainActor in self.isSpeaking = false }
    }

    /// The nicest installed voice for the current language (premium > enhanced > default).
    private static func bestVoice() -> AVSpeechSynthesisVoice? {
        let language = AVSpeechSynthesisVoice.currentLanguageCode()
        let voices = AVSpeechSynthesisVoice.speechVoices().filter { $0.language == language }
        return voices.max { $0.quality.rawValue < $1.quality.rawValue }
            ?? AVSpeechSynthesisVoice(language: language)
    }

    /// Markdown to something worth hearing: no code blocks, symbols or URLs.
    nonisolated static func speakable(_ markdown: String) -> String {
        var lines: [String] = []
        var inCode = false
        for raw in markdown.components(separatedBy: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.hasPrefix("```") {
                if !inCode { lines.append("(There's code in the chat.)") }
                inCode.toggle()
                continue
            }
            if inCode || line.hasPrefix("|") && line.contains("---") { continue }
            var cleaned = line
                .replacingOccurrences(of: #"^#{1,6}\s*"#, with: "", options: .regularExpression)
                .replacingOccurrences(of: #"^[-*+]\s+"#, with: "", options: .regularExpression)
                .replacingOccurrences(of: #"\[([^\]]+)\]\([^)]+\)"#, with: "$1", options: .regularExpression)
                .replacingOccurrences(of: #"https?://\S+"#, with: "a link", options: .regularExpression)
            for symbol in ["**", "__", "`", "*", "|", ">"] {
                cleaned = cleaned.replacingOccurrences(of: symbol, with: " ")
            }
            cleaned = cleaned.replacingOccurrences(of: #"\s+"#, with: " ", options: .regularExpression)
                .trimmingCharacters(in: .whitespaces)
            if !cleaned.isEmpty { lines.append(cleaned) }
        }
        return lines.joined(separator: "\n")
    }
}

/// The listening overlay: a pulsing orb that follows your voice, and what's
/// been heard so far.
struct ListeningBar: View {
    let voice: VoiceInput
    let onCancel: () -> Void

    var body: some View {
        HStack(spacing: 14) {
            ZStack {
                Circle()
                    .fill(Brand.accent.opacity(0.25))
                    .frame(width: 46, height: 46)
                    .scaleEffect(1 + voice.level * 0.6)
                    .animation(.easeOut(duration: 0.12), value: voice.level)
                Image(systemName: "waveform")
                    .font(.tt(.body).weight(.semibold))
                    .foregroundStyle(.white)
                    .symbolEffect(.variableColor.iterative, isActive: voice.isListening)
            }
            Text(voice.transcript.isEmpty ? "Listening…" : voice.transcript)
                .lineLimit(3)
                .foregroundStyle(voice.transcript.isEmpty ? .secondary : .primary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentTransition(.opacity)
            Button {
                voice.stop()
            } label: {
                Image(systemName: "arrow.up")
                    .font(.tt(.body).weight(.bold))
                    .foregroundStyle(.white)
                    .frame(width: 40, height: 40)
            }
            .glassEffect(.regular.tint(Brand.accent).interactive(), in: .circle)
            .accessibilityLabel("Send now")
            Button(action: onCancel) {
                Image(systemName: "xmark").frame(width: 32, height: 32)
            }
            .foregroundStyle(.secondary)
            .accessibilityLabel("Cancel")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .glassEffect(.regular, in: .rect(cornerRadius: 28))
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
    }
}

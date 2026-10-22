// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import AVFoundation
import Observation
import SwiftUI
import UIKit

/// Records from the microphone to an AAC file for the box to transcribe.
/// Keeps going with the screen locked (the `audio` background mode), so a
/// meeting can be recorded with the phone face down on the table.
@MainActor
@Observable
final class AudioRecorder {
    private(set) var recording = false
    private(set) var elapsed: TimeInterval = 0
    /// 0...1, for the level meter.
    private(set) var level: Double = 0
    private(set) var error: String?
    private var recorder: AVAudioRecorder?
    private var ticker: Task<Void, Never>?

    func start() async -> Bool {
        error = nil
        guard await AVAudioApplication.requestRecordPermission() else {
            error = "TT Station can't use the microphone. Turn it on in Settings → TT Station."
            return false
        }
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playAndRecord, mode: .default, options: [.defaultToSpeaker, .allowBluetoothHFP])
            try session.setActive(true)
            // Documents, not the temporary folder: a recording stays until the
            // box has it, even if the upload fails or the app is closed.
            let folder = URL.documentsDirectory.appendingPathComponent("Recordings", isDirectory: true)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            guard session.isInputAvailable else {
                error = "No microphone is available right now."
                return false
            }
            let stamp = Int(Date().timeIntervalSince1970)
            var started: AVAudioRecorder?
            var why = "the recorder refused every format"
            // AAC at Whisper's 16 kHz, small (about 15 MB an hour); then
            // fallbacks. The encoder rejects bitrates it can't do at a given
            // rate (64 kb/s at 16 kHz mono made `record()` fail silently), so
            // each format is tried until one actually starts.
            for (ext, settings) in Self.formats {
                let url = folder.appendingPathComponent("recording-\(stamp).\(ext)")
                do {
                    let r = try AVAudioRecorder(url: url, settings: settings)
                    r.isMeteringEnabled = true
                    if r.prepareToRecord(), r.record() {
                        started = r
                        break
                    }
                    r.deleteRecording()
                } catch {
                    why = error.localizedDescription
                }
            }
            guard let recorder = started else {
                error = "The recording couldn't start (\(why)). If another app is using the microphone, close it and try again."
                return false
            }
            self.recorder = recorder
            recording = true
            elapsed = 0
            UIApplication.shared.isIdleTimerDisabled = true
            ticker = Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(for: .milliseconds(100))
                    guard let self, let r = self.recorder else { return }
                    r.updateMeters()
                    self.elapsed = r.currentTime
                    // -50 dB is silence, 0 dB is as loud as it gets.
                    self.level = max(0, min(1, (Double(r.averagePower(forChannel: 0)) + 50) / 50))
                }
            }
            return true
        } catch {
            self.error = error.localizedDescription
            return false
        }
    }

    /// What to record, best first: (file extension, settings).
    private static let formats: [(String, [String: Any])] = [
        ("m4a", [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: 16_000, AVNumberOfChannelsKey: 1,
                 AVEncoderBitRateKey: 32_000]),
        ("m4a", [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: 16_000, AVNumberOfChannelsKey: 1,
                 AVEncoderAudioQualityKey: AVAudioQuality.medium.rawValue]),
        ("m4a", [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: 44_100, AVNumberOfChannelsKey: 1]),
        // Always works; larger (about 115 MB an hour).
        ("wav", [AVFormatIDKey: kAudioFormatLinearPCM, AVSampleRateKey: 16_000, AVNumberOfChannelsKey: 1,
                 AVLinearPCMBitDepthKey: 16, AVLinearPCMIsFloatKey: false, AVLinearPCMIsBigEndianKey: false]),
    ]

    /// Stop and return the file, or nil if nothing was recorded.
    func stop() -> URL? {
        ticker?.cancel()
        ticker = nil
        let url = recorder?.url
        let length = recorder?.currentTime ?? 0
        recorder?.stop()
        recorder = nil
        recording = false
        level = 0
        UIApplication.shared.isIdleTimerDisabled = false
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        return length >= 1 ? url : nil
    }

    func discard() {
        if let url = stop() { try? FileManager.default.removeItem(at: url) }
    }
}

/// A row of bars that follow the input level.
struct LevelBars: View {
    let level: Double
    let active: Bool
    @State private var history: [Double] = Array(repeating: 0, count: 32)

    var body: some View {
        HStack(alignment: .center, spacing: 3) {
            ForEach(history.indices, id: \.self) { i in
                Capsule()
                    .fill(active ? Brand.accent : .white.opacity(0.2))
                    .frame(maxWidth: .infinity)
                    .frame(height: max(4, 44 * history[i]))
            }
        }
        .onChange(of: level) { _, new in
            history.removeFirst()
            history.append(new)
        }
        .animation(.linear(duration: 0.1), value: history)
    }
}

/// Recording in place: the Transcribe screen's bottom bar becomes the
/// recorder (no separate screen to present, so nothing can fail to open).
struct RecordingBar: View {
    let recorder: AudioRecorder
    let onStop: () -> Void
    let onDiscard: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            HStack(spacing: 12) {
                Circle().fill(Vesper.error).frame(width: 10, height: 10)
                    .phaseAnimator([1.0, 0.3]) { dot, o in dot.opacity(o) } animation: { _ in .easeInOut(duration: 0.7) }
                Text(Self.clock(recorder.elapsed))
                    .font(.ttMono(22, .medium, relativeTo: .title2))
                    .contentTransition(.numericText())
                    .animation(.default, value: Int(recorder.elapsed))
                LevelBars(level: recorder.level, active: true)
                    .frame(height: 28)
            }
            HStack(spacing: 12) {
                Button(role: .destructive, action: onDiscard) {
                    Label("Discard", systemImage: "trash").frame(maxWidth: .infinity)
                }
                .buttonStyle(.glass)
                .controlSize(.large)
                Button(action: onStop) {
                    Label("Stop and send", systemImage: "stop.fill").frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.large)
            }
            Text("Keeps recording with the screen locked.").font(.tt(.caption)).foregroundStyle(.secondary)
        }
        .padding(16)
        .card(radius: Vesper.Radius.r6)
    }

    static func clock(_ t: TimeInterval) -> String {
        let s = Int(t)
        return s >= 3600 ? String(format: "%d:%02d:%02d", s / 3600, s % 3600 / 60, s % 60)
                         : String(format: "%d:%02d", s / 60, s % 60)
    }

    /// Recordings saved on the phone that haven't reached the box (they're
    /// deleted once uploaded), newest first.
    static func unsent() -> [URL] {
        let folder = URL.documentsDirectory.appendingPathComponent("Recordings", isDirectory: true)
        let files = (try? FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
        return files.sorted {
            let a = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            let b = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            return a > b
        }
    }
}

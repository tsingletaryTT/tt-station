// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import AppIntents
import Foundation
import UniformTypeIdentifiers

/// Siri, Spotlight, the Action button and Shortcuts: ask the box, check on it,
/// or have it draw -- without opening the app. Uses the active paired box.
private enum ActiveBox {
    static func client() throws -> AgentClient {
        let store = BoxStore()
        let activeID = UserDefaults.standard.string(forKey: "tt.mobile.activeBox")
        guard let box = store.boxes.first(where: { $0.id == activeID }) ?? store.boxes.first,
              let token = store.token(for: box) else {
            throw IntentProblem.notPaired
        }
        return try AgentClient(box: box, token: token)
    }
}

enum IntentProblem: Error, CustomLocalizedStringResourceConvertible {
    case notPaired
    case nothingServing
    case noImage

    var localizedStringResource: LocalizedStringResource {
        switch self {
        case .notPaired: "Pair TT Station with your box first."
        case .nothingServing: "Your box isn't serving a model right now."
        case .noImage: "The box didn't return an image."
        }
    }
}

struct AskBoxIntent: AppIntent {
    static let title: LocalizedStringResource = "Ask My Box"
    static let description = IntentDescription("Ask the assistant on your Tenstorrent box. Runs privately on your own hardware.")

    @Parameter(title: "Question", requestValueDialog: "What do you want to ask?")
    var question: String

    static var parameterSummary: some ParameterSummary { Summary("Ask my box \(\.$question)") }

    func perform() async throws -> some IntentResult & ReturnsValue<String> & ProvidesDialog {
        let client = try ActiveBox.client()
        let response = try await client.ask([ChatTurn(role: "user", content: question)])
        let reply = VoiceOutput.speakable(response.reply)
        return .result(value: reply, dialog: IntentDialog(stringLiteral: reply))
    }
}

struct BoxStatusIntent: AppIntent {
    static let title: LocalizedStringResource = "Box Status"
    static let description = IntentDescription("What your Tenstorrent box is serving, how warm its chips are, and how much power it draws.")

    func perform() async throws -> some IntentResult & ReturnsValue<String> & ProvidesDialog {
        let client = try ActiveBox.client()
        async let serveState = try? client.serveState()
        async let metrics = try? client.metricsNow()
        let (serve, now) = await (serveState, metrics)
        var parts: [String] = []
        if let serve {
            switch serve.phase {
            case "serving":
                parts.append("Serving \(serve.displayModel ?? "a model").")
            case "starting": parts.append("Starting \(serve.displayModel ?? "a model").")
            case "failed": parts.append("The last start failed.")
            default: parts.append("Nothing is serving.")
            }
        }
        if let chips = now?.point?.chips, !chips.isEmpty {
            let temps = chips.compactMap(\.tempC)
            let watts = chips.compactMap(\.powerW).reduce(0, +)
            if let hottest = temps.max() {
                parts.append("Hottest chip \(Int(Temp.value(hottest).rounded())) degrees \(Temp.fahrenheit ? "Fahrenheit" : "Celsius"), \(Int(watts.rounded())) watts in total.")
            }
        }
        if let tps = now?.point?.genTps, tps > 0.5 {
            parts.append("Generating \(Int(tps.rounded())) tokens a second.")
        }
        let text = parts.isEmpty ? "Couldn't read the box's status." : parts.joined(separator: " ")
        return .result(value: text, dialog: IntentDialog(stringLiteral: text))
    }
}

struct MakeImageIntent: AppIntent {
    static let title: LocalizedStringResource = "Make an Image"
    static let description = IntentDescription("Draw a picture with the image model on your Tenstorrent box.")

    @Parameter(title: "Prompt", requestValueDialog: "What should it draw?")
    var prompt: String

    static var parameterSummary: some ParameterSummary { Summary("Draw \(\.$prompt) on my box") }

    func perform() async throws -> some IntentResult & ReturnsValue<IntentFile> & ProvidesDialog {
        let client = try ActiveBox.client()
        let result = try await client.generateImage(ImageRequest(prompt: prompt, negativePrompt: nil,
                                                                 numInferenceSteps: 4, seed: nil,
                                                                 numberOfImages: 1))
        guard let b64 = result.images.first, let data = Data(base64Encoded: b64) else {
            throw IntentProblem.noImage
        }
        let file = IntentFile(data: data, filename: "quietbox-image.jpg", type: .jpeg)
        return .result(value: file, dialog: "Here's your picture, made on your box.")
    }
}

/// Sleep, restart or shut down the box (e.g. at night from a Shortcuts automation).
struct BoxPowerIntent: AppIntent {
    static let title: LocalizedStringResource = "Power My Box"
    static let description = IntentDescription("Put your Tenstorrent box to sleep, restart it, or shut it down.")

    enum Action: String, AppEnum {
        case sleep, restart, shutDown
        static let typeDisplayRepresentation: TypeDisplayRepresentation = "Power action"
        static let caseDisplayRepresentations: [Action: DisplayRepresentation] = [
            .sleep: "Sleep", .restart: "Restart", .shutDown: "Shut down",
        ]
        var wire: String { ["sleep": "suspend", "restart": "reboot", "shutDown": "shutdown"][rawValue] ?? rawValue }
    }

    @Parameter(title: "Action", default: .sleep)
    var action: Action

    static var parameterSummary: some ParameterSummary { Summary("\(\.$action) my box") }

    func perform() async throws -> some IntentResult & ProvidesDialog {
        try await ActiveBox.client().power(action.wire)
        return .result(dialog: "Done. Your box is going down.")
    }
}

/// Send the Wake-on-LAN signal to the box.
struct WakeBoxIntent: AppIntent {
    static let title: LocalizedStringResource = "Wake My Box"
    static let description = IntentDescription("Send a wake signal to your Tenstorrent box (needs a network cable and Wake-on-LAN).")

    func perform() async throws -> some IntentResult & ProvidesDialog {
        let store = BoxStore()
        let activeID = UserDefaults.standard.string(forKey: "tt.mobile.activeBox")
        guard let box = store.boxes.first(where: { $0.id == activeID }) ?? store.boxes.first else {
            throw IntentProblem.notPaired
        }
        _ = try await WakeOnLAN.wake(box)
        return .result(dialog: "Wake signal sent.")
    }
}

struct TTStationShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: AskBoxIntent(),
                    phrases: ["Ask \(.applicationName)", "Ask my box in \(.applicationName)"],
                    shortTitle: "Ask My Box", systemImageName: "sparkles")
        AppShortcut(intent: BoxStatusIntent(),
                    phrases: ["How's my box in \(.applicationName)", "\(.applicationName) status"],
                    shortTitle: "Box Status", systemImageName: "cpu")
        AppShortcut(intent: MakeImageIntent(),
                    phrases: ["Make an image with \(.applicationName)", "Draw with \(.applicationName)"],
                    shortTitle: "Make an Image", systemImageName: "photo.on.rectangle.angled")
        AppShortcut(intent: BoxPowerIntent(),
                    phrases: ["Power my box with \(.applicationName)", "Put my box to sleep with \(.applicationName)"],
                    shortTitle: "Power My Box", systemImageName: "power")
        AppShortcut(intent: WakeBoxIntent(),
                    phrases: ["Wake my box with \(.applicationName)"],
                    shortTitle: "Wake My Box", systemImageName: "sunrise")
    }
}

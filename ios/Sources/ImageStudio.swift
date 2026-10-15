import SwiftUI

/// Text-to-image on the box's served image model (FLUX and friends), via the
/// agent's `POST /generate/image`. Results land in the on-device gallery.
struct ImageStudio: View {
    let session: BoxSession
    @Environment(AppModel.self) private var app
    @State private var prompt = ""
    @State private var negativePrompt = ""
    @State private var style: PromptStyle = .none
    @State private var count = 1
    @State private var steps: Double = 4
    @State private var randomSeed = true
    @State private var seed = ""
    @State private var showSettings = false
    @State private var generatingSince: Date?
    @State private var error: String?
    @FocusState private var promptFocused: Bool

    private var model: String { session.imageModel ?? "image model" }
    private var profile: StepProfile { StepProfile(model: model) }

    var body: some View {
        ScrollView {
            VStack(spacing: 18) {
                header
                composer
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.tt(.footnote))
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card(tint: .red.opacity(0.3), radius: 16)
                }
                GalleryGrid(title: "Images", kind: .image) { reuse($0) }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 32)
        }
        .scrollDismissesKeyboard(.interactively)
        .scrollIndicators(.hidden)
        .onAppear { steps = Double(profile.defaultSteps) }
        .onChange(of: model) { steps = Double(profile.defaultSteps) }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "photo.on.rectangle.angled")
                .foregroundStyle(Brand.accent)
            Text(Brand.shortModel(model)).font(.tt(.headline))
            Spacer()
            StatusPill(state: .serving, text: "Ready")
        }
        .padding(.top, 4)
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 14) {
            TextField("Describe an image…", text: $prompt, axis: .vertical)
                .lineLimit(3...8)
                .focused($promptFocused)
                .padding(14)
                .glassEffect(.regular, in: .rect(cornerRadius: 18))

            ScrollView(.horizontal) {
                HStack(spacing: 8) {
                    ForEach(PromptStyle.allCases, id: \.self) { option in
                        Button(option.label) { style = option }
                            .buttonStyle(.glass)
                            .tint(style == option ? Brand.accent : nil)
                            .controlSize(.small)
                    }
                }
            }
            .scrollIndicators(.hidden)

            DisclosureGroup(isExpanded: $showSettings) {
                VStack(alignment: .leading, spacing: 14) {
                    Stepper("Images: \(count)", value: $count, in: 1...4)
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Steps: \(Int(steps))").font(.tt(.subheadline))
                        Slider(value: $steps, in: Double(profile.minSteps)...Double(profile.maxSteps), step: 1)
                        Text(profile.hint).font(.tt(.caption)).foregroundStyle(.secondary)
                    }
                    Toggle("Random seed", isOn: $randomSeed)
                    if !randomSeed {
                        TextField("Seed", text: $seed).keyboardType(.numberPad)
                    }
                    if !profile.ignoresNegativePrompt {
                        TextField("Avoid (negative prompt)", text: $negativePrompt, axis: .vertical)
                            .lineLimit(1...3)
                    }
                }
                .padding(.top, 10)
            } label: {
                Label("Settings", systemImage: "slider.horizontal.3").font(.tt(.subheadline))
            }

            Button(action: generate) {
                HStack(spacing: 8) {
                    if let since = generatingSince {
                        ProgressView()
                        TimelineView(.periodic(from: .now, by: 1)) { context in
                            Text("Generating · \(Int(context.date.timeIntervalSince(since)))s")
                        }
                    } else {
                        Image(systemName: "wand.and.stars")
                        Text(count == 1 ? "Generate" : "Generate \(count)")
                    }
                }
                .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .controlSize(.large)
            .disabled(prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || generatingSince != nil)
        }
        .padding(16)
        .card()
    }

    private func reuse(_ creation: Creation) {
        prompt = creation.prompt
        style = .none
        if let s = creation.seed {
            randomSeed = false
            seed = String(s)
        }
        steps = Double(min(max(creation.steps, profile.minSteps), profile.maxSteps))
    }

    private func generate() {
        promptFocused = false
        error = nil
        let fullPrompt = style.apply(to: prompt.trimmingCharacters(in: .whitespacesAndNewlines))
        // A fixed seed makes results reproducible (and "Reuse" meaningful); pick one
        // here even when random, so the gallery can record it.
        let usedSeed = randomSeed ? Int.random(in: 0...2_147_483_647) : (Int(seed) ?? 0)
        let request = ImageRequest(
            prompt: fullPrompt,
            negativePrompt: profile.ignoresNegativePrompt || negativePrompt.isEmpty ? nil : negativePrompt,
            numInferenceSteps: Int(steps),
            seed: usedSeed,
            numberOfImages: count
        )
        let modelName = model
        let started = Date()
        generatingSince = started
        Task {
            defer { generatingSince = nil }
            do {
                let result = try await session.generateImage(request)
                if result.images.isEmpty { throw AgentError.http(500, "The model returned no images.") }
                for (index, b64) in result.images.enumerated() {
                    guard let data = Data(base64Encoded: b64) else { continue }
                    let file = try app.creations.write(data, ext: "jpg")
                    app.creations.add(Creation(
                        id: UUID(), kind: .image, prompt: fullPrompt, model: modelName,
                        seed: usedSeed + index, steps: Int(steps), createdAt: Date(), boxID: session.box.id,
                        seconds: result.generationTime ?? Date().timeIntervalSince(started),
                        fileName: file, jobID: nil, status: "done", error: nil
                    ))
                }
            } catch AgentError.http(409, let message) {
                // The served model changed under us (someone switched it).
                error = message
                await session.refreshStatus()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}

/// Step limits and defaults per image model: FLUX.1-schnell is a 4-step distilled
/// model (the server's floor for FLUX is 4); FLUX.1-dev wants ~28; others 20, floor 12.
struct StepProfile {
    let minSteps: Int
    let maxSteps: Int
    let defaultSteps: Int
    let ignoresNegativePrompt: Bool
    let hint: String

    init(model: String) {
        let name = model.lowercased()
        if name.contains("schnell") {
            (minSteps, maxSteps, defaultSteps) = (4, 12, 4)
            ignoresNegativePrompt = true
            hint = "Schnell is tuned for 4 steps: more rarely helps."
        } else if name.contains("flux") {
            (minSteps, maxSteps, defaultSteps) = (4, 50, 28)
            ignoresNegativePrompt = true
            hint = "More steps: more detail, slower."
        } else {
            (minSteps, maxSteps, defaultSteps) = (12, 50, 20)
            ignoresNegativePrompt = false
            hint = "More steps: more detail, slower."
        }
    }
}

/// One-tap prompt styles, appended to the prompt.
enum PromptStyle: CaseIterable {
    case none, photo, cinematic, illustration, render3D, pixel, watercolor

    var label: String {
        switch self {
        case .none: return "No style"
        case .photo: return "Photo"
        case .cinematic: return "Cinematic"
        case .illustration: return "Illustration"
        case .render3D: return "3D render"
        case .pixel: return "Pixel art"
        case .watercolor: return "Watercolor"
        }
    }

    func apply(to prompt: String) -> String {
        switch self {
        case .none: return prompt
        case .photo: return prompt + ", photorealistic, natural light, 35mm photo, sharp focus"
        case .cinematic: return prompt + ", cinematic lighting, dramatic, film still, shallow depth of field"
        case .illustration: return prompt + ", detailed digital illustration, clean lines, vibrant colors"
        case .render3D: return prompt + ", 3D render, octane, soft studio lighting, high detail"
        case .pixel: return prompt + ", pixel art, 16-bit, crisp pixels, limited palette"
        case .watercolor: return prompt + ", watercolor painting, soft washes, paper texture"
        }
    }
}

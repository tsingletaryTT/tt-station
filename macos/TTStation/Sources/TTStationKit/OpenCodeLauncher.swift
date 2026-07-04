import Foundation

/// Builds the files/commands to launch `opencode` pointed at a box endpoint.
///
/// Pure — this type never touches `Process`, the filesystem, or AppKit. The
/// app layer (`LaunchController`) performs the actual file write + Terminal
/// launch, so this logic stays trivially unit-testable.
public enum OpenCodeLauncher {
    /// Contents of a project `opencode.json` registering a custom
    /// OpenAI-compatible provider `ttstation` for `endpoint` and preselecting
    /// its served model.
    ///
    /// `baseURL` is the full `.../v1` the endpoint reports; opencode splits the
    /// selection id (`ttstation/<model>`) on the first `/`, so a vendored model
    /// like `meta-llama/Llama-3.3-70B-Instruct` still resolves under the
    /// `ttstation` provider.
    public static func configJSON(for endpoint: Endpoint) -> String {
        let dict: [String: Any] = [
            "$schema": "https://opencode.ai/config.json",
            "provider": [
                "ttstation": [
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "TT Station",
                    "options": ["baseURL": endpoint.baseURL],
                    "models": [endpoint.model: ["name": "\(endpoint.model) (TT)"]],
                ],
            ],
            "model": "ttstation/\(endpoint.model)",
        ]
        // `try!` is safe: the dictionary is composed only of JSON-serializable
        // types (strings + nested dictionaries), so serialization cannot fail.
        let data = try! JSONSerialization.data(
            withJSONObject: dict, options: [.prettyPrinted, .sortedKeys])
        return String(data: data, encoding: .utf8)!
    }

    /// The shell line Terminal runs: cd into the config dir and start opencode
    /// (Terminal's login shell resolves `opencode` on PATH, sidestepping the
    /// GUI-PATH problem the app process itself has).
    ///
    /// The dir is single-quoted; we assume no single-quotes in our own scratch
    /// path (it lives under Application Support, which we control).
    public static func terminalCommand(configDir: String) -> String {
        "cd '\(configDir)' && opencode"
    }
}

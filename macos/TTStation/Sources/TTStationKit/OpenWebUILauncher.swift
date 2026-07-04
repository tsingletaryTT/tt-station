import Foundation

/// Builds the `uvx open-webui serve` invocation (argv + env) and the URLs to
/// poll/open for a local Open WebUI wired to a box endpoint.
///
/// Pure — the app layer (`LaunchController`) spawns the process and opens the
/// browser. This keeps the exact argv/env/URL shape unit-testable.
public enum OpenWebUILauncher {
    /// The command to run Open WebUI locally via `uvx`, pointed at `endpoint`'s
    /// OpenAI-compatible `/v1`.
    ///
    /// `WEBUI_AUTH=false` skips the account-creation wall for a quick demo;
    /// `OPENAI_API_KEY=sk-none` is a throwaway placeholder (the box endpoint
    /// does not require a key).
    public static func invocation(for endpoint: Endpoint)
        -> (executable: String, args: [String], env: [String: String])
    {
        (
            executable: "uvx",
            args: ["open-webui", "serve", "--port", "8080"],
            env: [
                "OPENAI_API_BASE_URL": endpoint.baseURL,
                "OPENAI_API_KEY": "sk-none",
                "WEBUI_AUTH": "false",
            ]
        )
    }

    /// The browser URL to open once Open WebUI is serving.
    public static let url = URL(string: "http://localhost:8080")!
    /// The health endpoint to poll while waiting for Open WebUI to come up.
    public static let healthURL = URL(string: "http://localhost:8080/health")!
}

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
    /// `--python 3.11` (passed to `uvx` BEFORE the tool name) pins Open WebUI to
    /// its supported Python. This matters a lot: uv otherwise resolves the
    /// newest Python on the machine (e.g. 3.14), for which Open WebUI's heavy
    /// deps (pyarrow/Apache Arrow, chromadb) ship no prebuilt wheels — so uv
    /// compiles them from source, dragging in cmake + Arrow and turning a
    /// "fast Connect" into a long, failure-prone native build. 3.11 has wheels
    /// for all of them, so `uvx` just downloads and runs. uv fetches a managed
    /// 3.11 on demand if the machine doesn't have one.
    ///
    /// `WEBUI_AUTH=false` skips the account-creation wall for a quick demo;
    /// `OPENAI_API_KEY=sk-none` is a throwaway placeholder (the box endpoint
    /// does not require a key).
    ///
    /// `dataDir` pins Open WebUI's own state (`DATA_DIR`) AND its database
    /// (`DATABASE_URL=sqlite:///<dataDir>/webui.db`) into an app-owned
    /// directory. This is not just tidiness: Open WebUI reads `DATABASE_URL`
    /// straight from the environment, so an *ambient* `DATABASE_URL` in the
    /// user's shell (e.g. a Postgres DSN exported for some unrelated tool)
    /// gets picked up and crashes startup with "Could not parse SQLAlchemy
    /// URL". Setting our own sqlite URL here makes the launch immune to
    /// whatever `DATABASE_URL` happens to be exported — `spawnDetached` merges
    /// this env over the inherited one, so our value wins.
    public static func invocation(for endpoint: Endpoint, dataDir: String)
        -> (executable: String, args: [String], env: [String: String])
    {
        (
            executable: "uvx",
            args: ["--python", "3.11", "open-webui", "serve", "--port", "8080"],
            env: [
                "OPENAI_API_BASE_URL": endpoint.baseURL,
                "OPENAI_API_KEY": "sk-none",
                "WEBUI_AUTH": "false",
                "DATA_DIR": dataDir,
                "DATABASE_URL": "sqlite:///\(dataDir)/webui.db",
            ]
        )
    }

    /// The browser URL to open once Open WebUI is serving.
    public static let url = URL(string: "http://localhost:8080")!
    /// The health endpoint to poll while waiting for Open WebUI to come up.
    public static let healthURL = URL(string: "http://localhost:8080/health")!
}

import Foundation

/// Brew-based provisioning of Connect-action dependencies. The Connect actions
/// (opencode, tt-toplike) shell out to CLIs that may not be installed yet; this
/// enum builds the `brew` argv used to install them on demand. Pure/testable —
/// Task 12 wires these args into an actual `Process` launch (owner-verified,
/// not unit-testable the way this pure builder is).
public enum Provisioning {
    /// opencode ships from Tenstorrent's own tap, not homebrew-core.
    public static let opencodeFormula = "sst/tap/opencode"
    /// uv (the Python launcher used to run Open WebUI via `uvx`) is in homebrew-core.
    public static let uvFormula = "uv"

    public static func brewInstallArgs(formula: String) -> [String] {
        ["install", formula]
    }
}

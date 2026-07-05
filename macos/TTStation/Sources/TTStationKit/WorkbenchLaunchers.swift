import Foundation

/// An SSH target: which user on which host. `resolve` canonicalizes the host
/// (mDNS names arrive as FQDNs with a trailing dot) and picks the user
/// (an explicit override, else the current login name).
public struct SSHTarget: Equatable {
    public let user: String
    public let host: String
    public init(user: String, host: String) { self.user = user; self.host = host }

    public static func resolve(host: String, overrideUser: String?, currentUser: String) -> SSHTarget {
        let canonicalHost = host.hasSuffix(".") ? String(host.dropLast()) : host
        let user = (overrideUser.map { $0.isEmpty ? currentUser : $0 }) ?? currentUser
        return SSHTarget(user: user, host: canonicalHost)
    }
}

/// POSIX-safe single-quoting for embedding a value in a `/bin/sh` command:
/// wraps in single quotes and replaces each `'` with `'\''` so the value
/// cannot break out of the quoting (host/user can come from untrusted mDNS).
func shellSingleQuoted(_ s: String) -> String {
    "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
}

/// `ssh` into the box. `accept-new` lets a first connect to an unknown host key
/// through (still prompts for a password if key auth isn't set up — fine, that
/// happens in the Terminal the app opens).
public enum TerminalSSHLauncher {
    public static func command(user: String, host: String) -> String {
        "ssh -o StrictHostKeyChecking=accept-new \(shellSingleQuoted("\(user)@\(host)"))"
    }
}

/// tt-toplike's remote telemetry view against the box's control port.
public enum TTToplikeLauncher {
    public static func command(host: String, ctrlPort: Int) -> String {
        "tt-toplike-tui --remote \(shellSingleQuoted("\(host):\(ctrlPort)"))"
    }
}

/// A VS Code Remote-SSH window on the box (integrated terminal runs on the box).
public enum VSCodeLauncher {
    /// Marketplace ID of Tenstorrent's own extension (also on Open VSX), so
    /// `--install-extension` resolves it directly — no `.vsix` needed.
    public static let toolkitExtensionID = "Tenstorrent.tt-vscode-toolkit"

    /// Builds `code` CLI args to OPEN a Remote-SSH window on the box.
    ///
    /// Deliberately carries NO `--install-extension`: the `code` CLI treats
    /// `--install-extension` as a management command — it installs, prints to
    /// stdout, exits 0, and does NOT open a window, even when a `--remote
    /// <folder>` is also given. Combining the two (an earlier version did) is
    /// exactly why the window "did nothing." The toolkit install is a separate
    /// `code` invocation (`installExtensionArgs`), run before this one.
    public static func remoteArgs(user: String, host: String, path: String) -> [String] {
        ["--remote", "ssh-remote+\(user)@\(host)", path]
    }

    /// Builds `code` CLI args to install the tt-vscode-toolkit extension. Run
    /// as its OWN `code` invocation (it runs headless and exits) — never merged
    /// with `remoteArgs`, or the window won't open (see `remoteArgs`' comment).
    public static func installExtensionArgs() -> [String] {
        ["--install-extension", toolkitExtensionID]
    }

    public static func defaultRemotePath(user: String) -> String { "/home/\(user)" }
}

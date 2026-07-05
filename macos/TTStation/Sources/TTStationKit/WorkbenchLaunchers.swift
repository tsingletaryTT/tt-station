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

    /// Builds `code` CLI args for a Remote-SSH window on the box. When
    /// `installToolkit` is true, prepends `--install-extension <id>` so the
    /// toolkit gets installed into the remote host before the window opens.
    ///
    /// Single method with a defaulted param (rather than a separate 3-arg
    /// overload) — a defaulted param *and* an explicit 3-arg overload would
    /// both match a 3-arg call and be ambiguous. One declaration keeps the
    /// existing `testVSCodeRemoteArgs` (which calls the 3-arg form) resolving
    /// unambiguously to this same method.
    public static func remoteArgs(user: String, host: String, path: String, installToolkit: Bool = false) -> [String] {
        let remoteFlags = ["--remote", "ssh-remote+\(user)@\(host)", path]
        guard installToolkit else { return remoteFlags }
        return ["--install-extension", toolkitExtensionID] + remoteFlags
    }
    public static func defaultRemotePath(user: String) -> String { "/home/\(user)" }
}

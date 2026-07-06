import Foundation

/// An SSH target: which user on which host. `resolve` canonicalizes the host
/// (mDNS names arrive as FQDNs with a trailing dot) and picks the user: an
/// explicit override (`tt.sshUser` in the app), else `defaultUser` (`ttuser`)
/// — the QuietBox 2's default login. The Mac login name is deliberately NOT
/// the default: it's almost never the same account as the box's, which is
/// exactly why VS Code Remote-SSH couldn't authenticate before this changed.
public struct SSHTarget: Equatable {
    /// The QuietBox 2 default login — where the keyless-SSH flow installs
    /// the Mac's public key, so this is the account that actually works.
    public static let defaultUser = "ttuser"

    public let user: String
    public let host: String
    public init(user: String, host: String) { self.user = user; self.host = host }

    /// `currentUser` (the Mac login, typically `NSUserName()`) is kept in the
    /// signature for source-compatibility with existing call sites, but it is
    /// NOT used to pick the default user anymore — only a non-empty
    /// `overrideUser` (the `tt.sshUser` preference) beats `defaultUser`.
    public static func resolve(host: String, overrideUser: String?, currentUser: String) -> SSHTarget {
        let canonicalHost = host.hasSuffix(".") ? String(host.dropLast()) : host
        let user = (overrideUser.flatMap { $0.isEmpty ? nil : $0 }) ?? defaultUser
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

    /// Builds `code` CLI args to install the tt-vscode-toolkit extension from
    /// the marketplace by ID. Run as its OWN `code` invocation (it runs headless
    /// and exits) — never merged with `remoteArgs`, or the window won't open
    /// (see `remoteArgs`' comment). Used only as a FALLBACK when no local
    /// `.vsix` is cached (see `installVsixArgs`).
    public static func installExtensionArgs() -> [String] {
        ["--install-extension", toolkitExtensionID]
    }

    /// Builds `code` CLI args to install the toolkit from a LOCAL `.vsix` file.
    /// Preferred over `installExtensionArgs` because it is gallery-independent:
    /// `--install-extension <id>` only works if the user's VS Code build is
    /// pointed at a marketplace that carries the extension (the default MS
    /// gallery, Open VSX, etc.), and forks/corporate builds often aren't — so
    /// the ID silently no-ops. A `.vsix` path always installs. `--force` lets a
    /// re-launch upgrade/reinstall without prompting. Like the ID variant, this
    /// is its own headless `code` call, never merged with `remoteArgs`.
    public static func installVsixArgs(vsixPath: String) -> [String] {
        ["--install-extension", vsixPath, "--force"]
    }

    public static func defaultRemotePath(user: String) -> String { "/home/\(user)" }
}

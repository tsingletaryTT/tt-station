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
    /// step entirely — scp'd to the box and installed there via
    /// `remoteInstallScript` (a local `--install-extension` wouldn't reach the
    /// remote session at all).
    public static func remoteArgs(user: String, host: String, path: String) -> [String] {
        ["--remote", "ssh-remote+\(user)@\(host)", path]
    }

    /// Where the toolkit `.vsix` is copied (scp) on the box before install.
    public static let remoteVsixPath = "/tmp/tt-vscode-toolkit.vsix"

    /// A shell script (run on the box over SSH) that installs the toolkit
    /// `.vsix` into the REMOTE VS Code server.
    ///
    /// The toolkit is a workspace (remote) extension: it has to live in the
    /// box's `~/.vscode-server/extensions`, not the Mac's local VS Code, or it
    /// never appears in the Remote-SSH session. And you can't push a local
    /// `.vsix` to a remote with `code --install-extension <vsix> --remote` —
    /// that installs the file LOCALLY (verified). So we scp the vsix over and
    /// run the box's own `code-server --install-extension` on it.
    ///
    /// `code-server` only exists after VS Code has bootstrapped its server on
    /// the box (which the Remote-SSH window-open triggers), so this polls for
    /// the binary for up to ~2 min before installing. `ls -t … | head -1`
    /// picks the newest server build if a VS Code update left more than one.
    /// `--force` makes a relaunch idempotently upgrade/reinstall.
    public static func remoteInstallScript(vsixPath: String = remoteVsixPath) -> String {
        """
        CS=""
        for i in $(seq 1 60); do
          CS=$(ls -t ~/.vscode-server/cli/servers/Stable-*/server/bin/code-server 2>/dev/null | head -1)
          [ -n "$CS" ] && break
          sleep 2
        done
        if [ -z "$CS" ]; then echo "vscode-server not ready" >&2; exit 3; fi
        "$CS" --install-extension '\(vsixPath)' --force
        """
    }

    public static func defaultRemotePath(user: String) -> String { "/home/\(user)" }
}

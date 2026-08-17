import Foundation

/// The observed state of the intended `~/.local/bin/tt-station` link path.
public enum CLILinkTarget: Equatable {
    case absent
    case symlink(target: String)
    case regularFile
}

/// What first-run should do about the CLI symlink.
public enum CLILinkAction: Equatable {
    /// Nothing there — create the symlink.
    case create(link: String, target: String)
    /// A symlink we previously installed (points into a `*/TTStation.app/`) —
    /// repoint it at this app's bundled `tt-station`.
    case repoint(link: String, target: String)
    /// Something we did not install is sitting at the link path (a real file,
    /// or a symlink pointing outside any `TTStation.app`).
    ///
    /// Back when the link path was `~/.local/bin/tt`, hitting this branch was
    /// *expected*: `tt` is the name of Tenstorrent's official CLI
    /// (`tenstorrent/tt-cli`), so a foreign file there was most likely that
    /// tool, and the sensible response was to leave it and offer to install
    /// ours under the alternative name `tt-station`. Now that `tt-station` IS
    /// the only name we ever claim, a foreign file here is genuinely
    /// unexpected — there is no other well-known owner of the name, and no
    /// second name left to fall back to. So this branch is now purely
    /// defensive: never overwrite what we find, and tell the operator what is
    /// in the way so they can resolve it themselves.
    case foreign(existing: String)
}

/// Pure decision for the first-run CLI symlink. No filesystem access — the
/// caller probes the path into a `CLILinkTarget` and applies the returned
/// action. A symlink is "ours" iff its target path contains `/TTStation.app/`,
/// which is cheap and avoids executing a foreign binary to classify it.
public enum CLILinkPlanner {
    public static func plan(linkPath: String, bundledTT: String, state: CLILinkTarget) -> CLILinkAction {
        switch state {
        case .absent:
            return .create(link: linkPath, target: bundledTT)
        case let .symlink(target):
            if target.contains("/TTStation.app/") {
                return .repoint(link: linkPath, target: bundledTT)
            }
            return .foreign(existing: target)
        case .regularFile:
            return .foreign(existing: linkPath)
        }
    }
}

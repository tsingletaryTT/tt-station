import Foundation

/// The observed state of the intended `~/.local/bin/tt` link path.
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
    /// repoint it at this app's bundled `tt`.
    case repoint(link: String, target: String)
    /// A foreign `tt` (a real file, or a symlink elsewhere). Never overwrite
    /// it; offer to install ours as `alternative` (a `tt-station` sibling).
    case foreign(existing: String, alternative: String)
}

/// Pure decision for the first-run CLI symlink. No filesystem access — the
/// caller probes the path into a `CLILinkTarget` and applies the returned
/// action. A symlink is "ours" iff its target path contains `/TTStation.app/`,
/// which is cheap and avoids executing a foreign binary to classify it.
public enum CLILinkPlanner {
    public static func plan(linkPath: String, bundledTT: String, state: CLILinkTarget) -> CLILinkAction {
        let alternative = (linkPath as NSString).deletingLastPathComponent + "/tt-station"
        switch state {
        case .absent:
            return .create(link: linkPath, target: bundledTT)
        case let .symlink(target):
            if target.contains("/TTStation.app/") {
                return .repoint(link: linkPath, target: bundledTT)
            }
            return .foreign(existing: target, alternative: alternative)
        case .regularFile:
            return .foreign(existing: linkPath, alternative: alternative)
        }
    }
}

import AppKit
import Foundation
import TTStationKit

/// First-run convenience: symlink the bundled `tt-station` into `~/.local/bin`
/// so the user gets `tt-station` in their own terminal. The app itself never
/// depends on this — `TTBinaryLocator` already falls back to the in-bundle
/// copy — so every branch here is best-effort and non-fatal.
///
/// The link is deliberately named `tt-station`, never `tt`: `tt` belongs to
/// Tenstorrent's official CLI (`tenstorrent/tt-cli`), and an earlier version
/// of this installer shadowed it on every Mac it ran on.
enum CLIInstaller {
    private static let offeredKey = "hasOfferedCLIInstall"

    static func runFirstRunIfNeeded(defaults: UserDefaults = .standard) {
        guard !defaults.bool(forKey: offeredKey) else { return }

        guard let bundled = Bundle.main.resourceURL?.appendingPathComponent("bin/tt-station").path,
              FileManager.default.isExecutableFile(atPath: bundled) else { return }
        // Record the one-time offer only once we actually have something to
        // offer: a dev/source build with no embedded CLI must not consume it,
        // since it shares this UserDefaults domain with a later real install.
        defaults.set(true, forKey: offeredKey)

        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let linkPath = "\(home)/.local/bin/tt-station"
        let action = CLILinkPlanner.plan(linkPath: linkPath, bundledTT: bundled, state: probe(linkPath))

        switch action {
        case let .create(link, target):
            offerInstall(link: link, target: target, replacing: false)
        case let .repoint(link, target):
            // Silent, idempotent update of our own stale link — no prompt.
            try? applyLink(link: link, target: target, replaceExisting: true)
        case let .foreign(existing):
            reportForeign(existing: existing, bundled: bundled)
        }
    }

    /// Classify the link path without following it: symlink vs regular file vs absent.
    private static func probe(_ path: String) -> CLILinkTarget {
        let fm = FileManager.default
        guard let attrs = try? fm.attributesOfItem(atPath: path) else { return .absent }
        if (attrs[.type] as? FileAttributeType) == .typeSymbolicLink {
            let target = (try? fm.destinationOfSymbolicLink(atPath: path)) ?? ""
            // An unreadable link target is treated as a foreign file at the
            // link path, so the alert shows a real path instead of going blank.
            if target.isEmpty { return .regularFile }
            return .symlink(target: target)
        }
        return .regularFile
    }

    private static func applyLink(link: String, target: String, replaceExisting: Bool) throws {
        let fm = FileManager.default
        let dir = (link as NSString).deletingLastPathComponent
        try fm.createDirectory(atPath: dir, withIntermediateDirectories: true)
        if replaceExisting { try? fm.removeItem(atPath: link) }
        try fm.createSymbolicLink(atPath: link, withDestinationPath: target)
    }

    private static func offerInstall(link: String, target: String, replacing: Bool) {
        let alert = NSAlert()
        alert.messageText = "Install the tt-station command-line tool?"
        alert.informativeText = "TTStation can add `tt-station` to \(link) so you can use it in Terminal. The app works either way."
        alert.addButton(withTitle: "Install")
        alert.addButton(withTitle: "Not Now")
        if alert.runModal() == .alertFirstButtonReturn {
            try? applyLink(link: link, target: target, replaceExisting: replacing)
        }
    }

    /// Something we did not install occupies `~/.local/bin/tt-station`.
    ///
    /// There is nothing safe left to do here, so this is a report, not an
    /// offer: we never overwrite a file we did not create, and `tt-station` is
    /// the only name this app claims (the previous `~/.local/bin/tt` link path
    /// had a fallback precisely because a foreign `tt` was probably
    /// Tenstorrent's official CLI; that no longer applies). Point the operator
    /// at the in-bundle copy so they can wire it up however they like.
    private static func reportForeign(existing: String, bundled: String) {
        let alert = NSAlert()
        alert.messageText = "Something else is already at `tt-station`"
        alert.informativeText = "Found \(existing) where TTStation would install its `tt-station` command. TTStation won't replace it. The bundled copy is at \(bundled) if you want to link it yourself."
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }
}

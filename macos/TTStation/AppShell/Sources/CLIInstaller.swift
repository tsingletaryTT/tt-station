import AppKit
import Foundation
import TTStationKit

/// First-run convenience: symlink the bundled `tt` into `~/.local/bin` so the
/// user gets `tt` in their own terminal. The app itself never depends on this
/// — `TTBinaryLocator` already falls back to the in-bundle copy — so every
/// branch here is best-effort and non-fatal.
enum CLIInstaller {
    private static let offeredKey = "hasOfferedCLIInstall"

    static func runFirstRunIfNeeded(defaults: UserDefaults = .standard) {
        guard !defaults.bool(forKey: offeredKey) else { return }

        guard let bundled = Bundle.main.resourceURL?.appendingPathComponent("bin/tt").path,
              FileManager.default.isExecutableFile(atPath: bundled) else { return }
        // Record the one-time offer only once we actually have something to
        // offer: a dev/source build with no embedded tt must not consume it,
        // since it shares this UserDefaults domain with a later real install.
        defaults.set(true, forKey: offeredKey)

        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let linkPath = "\(home)/.local/bin/tt"
        let action = CLILinkPlanner.plan(linkPath: linkPath, bundledTT: bundled, state: probe(linkPath))

        switch action {
        case let .create(link, target):
            offerInstall(link: link, target: target, replacing: false)
        case let .repoint(link, target):
            // Silent, idempotent update of our own stale link — no prompt.
            try? applyLink(link: link, target: target, replaceExisting: true)
        case let .foreign(existing, alternative):
            offerForeign(existing: existing, alternative: alternative, bundled: bundled)
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
        alert.messageText = "Install the tt command-line tool?"
        alert.informativeText = "TTStation can add `tt` to \(link) so you can use it in Terminal. The app works either way."
        alert.addButton(withTitle: "Install")
        alert.addButton(withTitle: "Not Now")
        if alert.runModal() == .alertFirstButtonReturn {
            try? applyLink(link: link, target: target, replaceExisting: replacing)
        }
    }

    private static func offerForeign(existing: String, alternative: String, bundled: String) {
        let alert = NSAlert()
        alert.messageText = "Another `tt` is already installed"
        alert.informativeText = "Found an existing `tt` at \(existing). TTStation won't replace it. Install this version as `tt-station` instead?"
        alert.addButton(withTitle: "Install as tt-station")
        alert.addButton(withTitle: "Not Now")
        if alert.runModal() == .alertFirstButtonReturn {
            try? applyLink(link: alternative, target: bundled, replaceExisting: true)
        }
    }
}

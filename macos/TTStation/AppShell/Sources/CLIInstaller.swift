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

/// First-run offer to install Tenstorrent's OFFICIAL `tt` CLI (tenstorrent/tt-cli, via
/// `uv tool install tenstorrent`). tt-station delegates model right-sizing for a card attached
/// to this Mac to it (`tt-station local` → `tt model list --hw <config>`).
///
/// All the logic lives in the bundled `Resources/scripts/ensure-official-tt.sh`, the same script
/// `macos/install.sh` runs, so the two install paths can't drift. This type only reads the
/// script's stable exit codes and turns them into alerts. Best-effort and non-fatal, like
/// `CLIInstaller`: the app works without the official CLI; `tt-station local` just can't
/// right-size.
enum OfficialCLIInstaller {
    private static let offeredKey = "hasOfferedOfficialTTInstall"

    /// Exit codes of ensure-official-tt.sh (see its header).
    private enum Code: Int32 { case ok = 0, foreign = 3, noUV = 4, stillShadowed = 5, notInstalled = 6 }

    static func runFirstRunIfNeeded(defaults: UserDefaults = .standard) {
        guard !defaults.bool(forKey: offeredKey) else { return }
        // Dev/source builds don't bundle the script; don't consume the one-time offer there.
        guard let script = Bundle.main.resourceURL?.appendingPathComponent("scripts/ensure-official-tt.sh").path,
              FileManager.default.isExecutableFile(atPath: script) else { return }
        defaults.set(true, forKey: offeredKey)

        DispatchQueue.global(qos: .utility).async {
            let (check, checkOut) = run(script, ["--check"])
            DispatchQueue.main.async {
                switch check {
                case Code.ok.rawValue:
                    return  // already the official CLI; nothing to say
                case Code.foreign.rawValue:
                    info("Another `tt` is in the way", checkOut)
                default:
                    offerInstall(script: script, detail: checkOut)
                }
            }
        }
    }

    private static func offerInstall(script: String, detail: String) {
        let alert = NSAlert()
        alert.messageText = "Install Tenstorrent's official `tt` CLI?"
        alert.informativeText = """
            tt-station asks the official CLI which models are right-sized for a Tenstorrent card \
            attached to this Mac. This runs `uv tool install tenstorrent` (installing uv with \
            Homebrew if needed).

            \(detail)
            """
        alert.addButton(withTitle: "Install")
        alert.addButton(withTitle: "Not Now")
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        DispatchQueue.global(qos: .userInitiated).async {
            let (code, out) = run(script, [])
            DispatchQueue.main.async {
                switch code {
                case Code.ok.rawValue: info("Official `tt` CLI installed", out)
                case Code.noUV.rawValue: info("uv is needed first", out)
                case Code.foreign.rawValue, Code.stillShadowed.rawValue: info("Installed, but another `tt` is in the way", out)
                default: info("Couldn't install the official `tt` CLI", out)
                }
            }
        }
    }

    /// Run the script, returning its exit status and the last few lines of combined output
    /// (uv prints a line per package; the alert only needs the verdict).
    private static func run(_ script: String, _ args: [String]) -> (Int32, String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/bash")
        p.arguments = [script] + args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do { try p.run() } catch { return (-1, "\(error.localizedDescription)") }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        let lines = String(decoding: data, as: UTF8.self).split(separator: "\n")
        return (p.terminationStatus, lines.suffix(3).joined(separator: "\n"))
    }

    private static func info(_ title: String, _ body: String) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = body
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }
}

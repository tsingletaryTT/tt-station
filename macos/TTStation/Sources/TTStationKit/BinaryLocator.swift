import Foundation

/// Resolves the `tt` binary. GUI apps do NOT inherit the shell PATH, so we
/// probe explicit locations in order and report every one we tried on failure.
public struct TTBinaryLocator {
    private let override: String?
    private let candidates: [String]
    private let fileExists: (String) -> Bool

    public init(override: String?, candidates: [String], fileExists: @escaping (String) -> Bool) {
        self.override = override
        self.candidates = candidates
        self.fileExists = fileExists
    }

    public func locate() throws -> String {
        var tried: [String] = []
        for path in ([override].compactMap { $0 } + candidates) {
            tried.append(path)
            if fileExists(path) { return path }
        }
        throw TTError.binaryNotFound(triedPaths: tried)
    }

    /// The ordered `tt` search path: the three shell-install locations, then
    /// the in-bundle copy as a last-resort fallback. Pure so it is unit-tested
    /// without touching `Bundle.main` or the filesystem.
    public static func standardCandidates(home: String, bundledPath: String?) -> [String] {
        ["\(home)/.local/bin/tt", "/opt/homebrew/bin/tt", "/usr/local/bin/tt"]
            + [bundledPath].compactMap { $0 }
    }

    /// Real-world locator: user override (UserDefaults key `tt.binaryPath`),
    /// then the standard install locations, then the copy embedded in the app
    /// bundle at `Contents/Resources/bin/tt` (so the app works with an empty
    /// `$PATH` on a fresh machine).
    public static func standard(
        override: String? = UserDefaults.standard.string(forKey: "tt.binaryPath"),
        bundledPath: String? = Bundle.main.resourceURL?.appendingPathComponent("bin/tt").path
    ) -> TTBinaryLocator {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return TTBinaryLocator(
            override: override,
            candidates: standardCandidates(home: home, bundledPath: bundledPath),
            fileExists: { FileManager.default.isExecutableFile(atPath: $0) }
        )
    }
}

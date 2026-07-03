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

    /// Real-world locator: user override (UserDefaults key `tt.binaryPath`)
    /// then the standard install locations.
    public static func standard(override: String? = UserDefaults.standard.string(forKey: "tt.binaryPath")) -> TTBinaryLocator {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return TTBinaryLocator(
            override: override,
            candidates: ["\(home)/.local/bin/tt", "/opt/homebrew/bin/tt", "/usr/local/bin/tt"],
            fileExists: { FileManager.default.isExecutableFile(atPath: $0) }
        )
    }
}

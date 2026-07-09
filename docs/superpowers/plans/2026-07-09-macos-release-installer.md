# macOS Release Installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a prebuilt, drag-to-install macOS DMG on a GitHub Release so people without the repo, Xcode, or Rust can install TTStation, with the `tt` CLI bundled inside the app.

**Architecture:** The `tt` Rust CLI is embedded inside `TTStation.app/Contents/Resources/bin/tt` so the app always has a version-locked CLI. The app's binary locator falls back to that bundled copy, and a first-run prompt symlinks it into `~/.local/bin` with collision handling that never clobbers a foreign `tt`. A local `macos/make-release.sh` builds and packages the arm64 DMG (ad-hoc signed, no notarization); a `v*`-tag GitHub Actions workflow calls the same script.

**Tech Stack:** Swift 5.9 / SwiftUI (`TTStationKit` package + `AppShell` app), XCTest, Rust (cargo, `tt` crate), bash, `hdiutil`, `xcodegen`, `xcodebuild`, GitHub Actions (`macos-14`), `gh` CLI.

## Global Constraints

- Target: **macOS 14.0**, **arm64 only** (`aarch64-apple-darwin`). No universal/Intel build.
- **No Apple Developer license** — ad-hoc signing only (`codesign --sign -`), no notarization. Gatekeeper friction is expected; the documented remedy is `xattr -dr com.apple.quarantine /Applications/TTStation.app`.
- Swift logic under test lives in **`TTStationKit`** (`macos/TTStation/Sources/TTStationKit/`); tests in `macos/TTStation/Tests/TTStationKitTests/`. App-shell wiring (`AppShell/Sources/`) is owner-verified, not unit-tested (matches the existing `LaunchController` convention).
- `tt` is embedded at exactly `Contents/Resources/bin/tt` and referenced by that relative path everywhere.
- Version is the single source of truth in `macos/TTStation/AppShell/project.yml` → `MARKETING_VERSION` (currently `0.8.2`); flows into `Info.plist` via `$(MARKETING_VERSION)`. Bump it as part of this work (Task 7).
- Follow existing test style: `XCTest`, `@testable import TTStationKit`, dependency injection via closures/protocols (see `BinaryLocatorTests.swift`, `InMemoryStore`).
- All Swift-side unit tests run with: `cd macos/TTStation && swift test`.

---

### Task 1: Bundled `tt` in the locator candidate list

Add the in-bundle `tt` as the last-resort candidate so the app resolves a working CLI even with an empty `$PATH`, while any real PATH install (or the `tt.binaryPath` override) still wins.

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/BinaryLocator.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/BinaryLocatorTests.swift`

**Interfaces:**
- Consumes: existing `TTBinaryLocator.init(override:candidates:fileExists:)` and `TTError.binaryNotFound`.
- Produces:
  - `static func standardCandidates(home: String, bundledPath: String?) -> [String]` — the ordered PATH candidates followed by the bundled path when non-nil.
  - Updated `static func standard(override:bundledPath:) -> TTBinaryLocator` (adds a `bundledPath` parameter defaulting to the app-bundle resource path).

- [ ] **Step 1: Write the failing test**

Add to `BinaryLocatorTests.swift`:

```swift
func testStandardCandidatesAppendsBundledPathLast() {
    let c = TTBinaryLocator.standardCandidates(home: "/Users/x", bundledPath: "/App/TTStation.app/Contents/Resources/bin/tt")
    XCTAssertEqual(c, [
        "/Users/x/.local/bin/tt",
        "/opt/homebrew/bin/tt",
        "/usr/local/bin/tt",
        "/App/TTStation.app/Contents/Resources/bin/tt",
    ])
}

func testStandardCandidatesOmitsBundledPathWhenNil() {
    let c = TTBinaryLocator.standardCandidates(home: "/Users/x", bundledPath: nil)
    XCTAssertEqual(c, [
        "/Users/x/.local/bin/tt",
        "/opt/homebrew/bin/tt",
        "/usr/local/bin/tt",
    ])
}

func testBundledPathUsedOnlyWhenPATHCandidatesAbsent() throws {
    let candidates = TTBinaryLocator.standardCandidates(home: "/Users/x", bundledPath: "/App/tt")
    // Only the bundled path exists → it is returned.
    let onlyBundled = TTBinaryLocator(override: nil, candidates: candidates) { $0 == "/App/tt" }
    XCTAssertEqual(try onlyBundled.locate(), "/App/tt")
    // A PATH candidate exists → it wins over the bundled path.
    let pathWins = TTBinaryLocator(override: nil, candidates: candidates) { $0 == "/opt/homebrew/bin/tt" }
    XCTAssertEqual(try pathWins.locate(), "/opt/homebrew/bin/tt")
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter BinaryLocatorTests`
Expected: FAIL — `standardCandidates` is not a member of `TTBinaryLocator`.

- [ ] **Step 3: Write minimal implementation**

In `BinaryLocator.swift`, replace the `standard(...)` function with:

```swift
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd macos/TTStation && swift test --filter BinaryLocatorTests`
Expected: PASS (all cases, including the pre-existing four).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/BinaryLocator.swift macos/TTStation/Tests/TTStationKitTests/BinaryLocatorTests.swift
git commit -m "feat(macos): locator falls back to bundled tt in app resources"
```

---

### Task 2: CLI-symlink collision planner (pure logic)

The decision that first-run makes: given the state of `~/.local/bin/tt`, decide whether to create the symlink, repoint our own stale symlink, or leave a foreign `tt` untouched and offer `tt-station` instead.

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/CLILinkPlanner.swift`
- Test: `macos/TTStation/Tests/TTStationKitTests/CLILinkPlannerTests.swift`

**Interfaces:**
- Produces:
  - `enum CLILinkTarget: Equatable { case absent; case symlink(target: String); case regularFile }`
  - `enum CLILinkAction: Equatable { case create(link: String, target: String); case repoint(link: String, target: String); case foreign(existing: String, alternative: String) }`
  - `enum CLILinkPlanner { static func plan(linkPath: String, bundledTT: String, state: CLILinkTarget) -> CLILinkAction }`
- Consumed by: Task 3 (the executor calls `plan(...)` after probing the filesystem).

- [ ] **Step 1: Write the failing test**

Create `CLILinkPlannerTests.swift`:

```swift
import XCTest
@testable import TTStationKit

final class CLILinkPlannerTests: XCTestCase {
    let link = "/Users/x/.local/bin/tt"
    let bundled = "/App/TTStation.app/Contents/Resources/bin/tt"

    func testAbsentCreatesSymlink() {
        let action = CLILinkPlanner.plan(linkPath: link, bundledTT: bundled, state: .absent)
        XCTAssertEqual(action, .create(link: link, target: bundled))
    }

    func testOurStaleSymlinkGetsRepointed() {
        // A symlink pointing into some (possibly older) TTStation.app is ours.
        let action = CLILinkPlanner.plan(
            linkPath: link, bundledTT: bundled,
            state: .symlink(target: "/Applications/TTStation.app/Contents/Resources/bin/tt"))
        XCTAssertEqual(action, .repoint(link: link, target: bundled))
    }

    func testForeignSymlinkIsLeftAloneWithAlternative() {
        let action = CLILinkPlanner.plan(
            linkPath: link, bundledTT: bundled,
            state: .symlink(target: "/some/other/tool/tt"))
        XCTAssertEqual(action, .foreign(existing: "/some/other/tool/tt", alternative: "/Users/x/.local/bin/tt-station"))
    }

    func testForeignRegularFileIsLeftAloneWithAlternative() {
        let action = CLILinkPlanner.plan(linkPath: link, bundledTT: bundled, state: .regularFile)
        XCTAssertEqual(action, .foreign(existing: link, alternative: "/Users/x/.local/bin/tt-station"))
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter CLILinkPlannerTests`
Expected: FAIL — `CLILinkPlanner` / `CLILinkAction` undefined.

- [ ] **Step 3: Write minimal implementation**

Create `CLILinkPlanner.swift`:

```swift
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd macos/TTStation && swift test --filter CLILinkPlannerTests`
Expected: PASS (4 cases).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/CLILinkPlanner.swift macos/TTStation/Tests/TTStationKitTests/CLILinkPlannerTests.swift
git commit -m "feat(macos): CLI-symlink collision planner (create/repoint/foreign)"
```

---

### Task 3: First-run CLI-install wiring (owner-verified)

Wire the planner into the app shell: on first launch, probe `~/.local/bin/tt`, run the planner, apply the action (creating `~/.local/bin` if needed), and show a one-time prompt. Guarded by a `hasOfferedCLIInstall` UserDefault so it runs once. This is app-shell I/O — no unit test (matches `LaunchController`), verified by build + manual run.

**Files:**
- Create: `macos/TTStation/AppShell/Sources/CLIInstaller.swift`
- Modify: `macos/TTStation/AppShell/Sources/TTStationApp.swift` (call the installer once at startup)

**Interfaces:**
- Consumes: `CLILinkPlanner.plan(linkPath:bundledTT:state:)`, `CLILinkTarget`, `CLILinkAction` (Task 2); `Bundle.main.resourceURL` for the bundled `tt` path (matches Task 1).
- Produces: `enum CLIInstaller { static func runFirstRunIfNeeded(defaults: UserDefaults = .standard) }`.

- [ ] **Step 1: Create the installer**

Create `CLIInstaller.swift`:

```swift
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
        defaults.set(true, forKey: offeredKey)

        guard let bundled = Bundle.main.resourceURL?.appendingPathComponent("bin/tt").path,
              FileManager.default.isExecutableFile(atPath: bundled) else { return }

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
```

- [ ] **Step 2: Call it once at startup**

In `TTStationApp.swift`, extend `init()` so the last line runs the installer after the model is built:

```swift
    init() {
        let registry = HostRegistry(store: UserDefaults.standard)
        let client = TTClient(runner: RealProcessRunner(locator: .standard()))
        let discovery = MDNSDiscoveryService(client: client, registry: registry)
        _model = State(initialValue: AppModel(commands: client, discovery: discovery, registry: registry))
        CLIInstaller.runFirstRunIfNeeded()
    }
```

- [ ] **Step 3: Verify the app builds**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate \
  && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Expected: `BUILD SUCCEEDED`.

- [ ] **Step 4: Manual verification (owner)**

Reset the guard and launch to see the prompt path (do once):
```bash
defaults delete com.tenstorrent.ttstation hasOfferedCLIInstall 2>/dev/null || true
```
Then run the built app; confirm the "Install the tt command-line tool?" prompt appears on first launch and `~/.local/bin/tt` is created on Install. Re-launch → no prompt.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/Sources/CLIInstaller.swift macos/TTStation/AppShell/Sources/TTStationApp.swift
git commit -m "feat(macos): first-run offer to symlink bundled tt into ~/.local/bin"
```

---

### Task 4: `macos/make-release.sh` packaging script

The local source of truth: build arm64 `tt` + the Release app, embed `tt`, ad-hoc sign, and produce `dist/TTStation-<ver>-arm64.dmg`. `--publish` uploads to a GitHub Release.

**Files:**
- Create: `macos/make-release.sh` (executable)
- Create: `macos/dist/.gitignore` (ignore build output)

**Interfaces:**
- Consumes: `AppShell/project.yml` `MARKETING_VERSION`; the existing xcodegen/xcodebuild invocation from `install.sh`; `cargo`, `hdiutil`, `codesign`, optionally `gh`.
- Produces: `macos/dist/TTStation-<ver>-arm64.dmg`.

- [ ] **Step 1: Write the script**

Create `macos/make-release.sh`:

```bash
#!/usr/bin/env bash
# Build a distributable, arm64 TTStation DMG with the `tt` CLI embedded inside
# the app bundle. Ad-hoc signed only (no Apple Developer license / no
# notarization) — downloaders must strip quarantine once; see FIRST-RUN.txt.
#
#   macos/make-release.sh              # build dist/TTStation-<ver>-arm64.dmg
#   macos/make-release.sh --publish    # also create/upload a GitHub Release (needs gh)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/.." && pwd)"
proj="$here/TTStation/AppShell"
dist="$here/dist"
stage="$dist/stage"
publish=""
[[ "${1:-}" == "--publish" ]] && publish="1"

ver="$(awk -F'"' '/MARKETING_VERSION:/{print $2}' "$proj/project.yml")"
[[ -n "$ver" ]] || { echo "error: could not read MARKETING_VERSION"; exit 1; }
dmg="$dist/TTStation-$ver-arm64.dmg"
echo "==> Building TTStation $ver (arm64)"

# 1. Build the arm64 tt CLI.
echo "==> cargo build --release -p tt (aarch64-apple-darwin)"
( cd "$repo_root" && cargo build --release -p tt --target aarch64-apple-darwin )
tt_bin="$repo_root/target/aarch64-apple-darwin/release/tt"
[[ -x "$tt_bin" ]] || { echo "error: tt binary not found at $tt_bin"; exit 1; }

# 2. Build the Release app.
echo "==> xcodegen + xcodebuild (Release)"
( cd "$proj" && xcodegen generate >/dev/null )
xcodebuild -project "$proj/TTStation.xcodeproj" -scheme TTStation \
  -configuration Release -destination 'platform=macOS' \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=NO build >/dev/null
app_src="$(xcodebuild -project "$proj/TTStation.xcodeproj" -scheme TTStation \
  -configuration Release -showBuildSettings 2>/dev/null \
  | awk '/ BUILT_PRODUCTS_DIR /{d=$3} /FULL_PRODUCT_NAME/{p=$3} END{print d"/"p}')"
[[ -d "$app_src" ]] || { echo "error: built app not found ($app_src)"; exit 1; }

# 3. Stage a clean copy and embed tt.
echo "==> Embedding tt into app bundle + ad-hoc signing"
rm -rf "$stage" && mkdir -p "$stage"
cp -R "$app_src" "$stage/TTStation.app"
mkdir -p "$stage/TTStation.app/Contents/Resources/bin"
cp "$tt_bin" "$stage/TTStation.app/Contents/Resources/bin/tt"
chmod +x "$stage/TTStation.app/Contents/Resources/bin/tt"

# 4. Ad-hoc sign the whole bundle AFTER embedding so the nested tt is covered.
codesign --force --deep --sign - "$stage/TTStation.app"
codesign --verify --deep --strict "$stage/TTStation.app" || {
  echo "error: codesign verification failed"; exit 1; }

# 5. Assemble the DMG payload: app + /Applications alias + first-run note.
ln -sf /Applications "$stage/Applications"
cat > "$stage/FIRST-RUN.txt" <<EOF
TTStation $ver — first run
==========================

1. Drag TTStation.app onto the Applications folder in this window.

2. TTStation is signed ad-hoc (no Apple Developer certificate yet), so macOS
   quarantines it and may say "TTStation is damaged." Clear the quarantine
   once, in Terminal:

       xattr -dr com.apple.quarantine /Applications/TTStation.app

3. Launch TTStation from Applications. On first run it offers to add the \`tt\`
   command to ~/.local/bin. The app works whether or not you accept.

TTStation lives in the menu bar (no Dock icon). Look for its icon up top.
EOF

# 6. Build the compressed DMG.
echo "==> hdiutil create $dmg"
rm -f "$dmg"
hdiutil create -volname "TTStation $ver" -srcfolder "$stage" \
  -fs HFS+ -format UDZO "$dmg" >/dev/null
sha="$(shasum -a 256 "$dmg" | awk '{print $1}')"
echo "==> Done: $dmg"
echo "    sha256: $sha"

# 7. Optional publish.
if [[ -n "$publish" ]]; then
  command -v gh >/dev/null || { echo "error: gh not found for --publish"; exit 1; }
  tag="v$ver"
  echo "==> Publishing GitHub Release $tag"
  notes="TTStation $ver (arm64). Ad-hoc signed — after installing run:

    xattr -dr com.apple.quarantine /Applications/TTStation.app

sha256: $sha"
  if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$dmg" --clobber
  else
    gh release create "$tag" "$dmg" --title "TTStation $ver" --notes "$notes"
  fi
fi
```

- [ ] **Step 2: Make it executable + add dist gitignore**

Run:
```bash
chmod +x macos/make-release.sh
mkdir -p macos/dist
printf '*\n!.gitignore\n' > macos/dist/.gitignore
```

- [ ] **Step 3: Lint the script**

Run: `shellcheck macos/make-release.sh` (if `shellcheck` is unavailable, `bash -n macos/make-release.sh` for a syntax check).
Expected: no errors (warnings about `$publish` being intentional are acceptable).

- [ ] **Step 4: Run it end-to-end (owner, on the Mac)**

Run: `macos/make-release.sh`
Expected: `dist/TTStation-<ver>-arm64.dmg` exists; mounting it shows `TTStation.app`, the `Applications` alias, and `FIRST-RUN.txt`. Verify the embedded CLI:
```bash
hdiutil attach dist/TTStation-*-arm64.dmg -mountpoint /tmp/ttmnt -nobrowse
/tmp/ttmnt/TTStation.app/Contents/Resources/bin/tt --help >/dev/null && echo "embedded tt OK"
hdiutil detach /tmp/ttmnt
```
Expected: `embedded tt OK`.

- [ ] **Step 5: Commit**

```bash
git add macos/make-release.sh macos/dist/.gitignore
git commit -m "feat(macos): make-release.sh builds arm64 DMG with embedded tt CLI"
```

---

### Task 5: GitHub Actions release workflow

On a `v*` tag push, build the DMG on a macOS runner by calling `make-release.sh --publish`.

**Files:**
- Create: `.github/workflows/macos-release.yml`

**Interfaces:**
- Consumes: `macos/make-release.sh --publish`; the workflow `GITHUB_TOKEN` (for `gh`).
- Produces: a GitHub Release with the DMG asset, on tag push.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/macos-release.yml`:

```yaml
name: macOS release

on:
  push:
    tags: ["v*"]
  workflow_dispatch:

permissions:
  contents: write

jobs:
  build:
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust (arm64)
        run: |
          rustup toolchain install stable --profile minimal
          rustup target add aarch64-apple-darwin

      - name: Install xcodegen
        run: brew install xcodegen

      - name: Build + publish DMG
        env:
          GH_TOKEN: ${{ github.token }}
        run: macos/make-release.sh --publish

      - name: Upload DMG artifact
        uses: actions/upload-artifact@v4
        with:
          name: TTStation-dmg
          path: macos/dist/*.dmg
```

- [ ] **Step 2: Lint the workflow**

Run: `actionlint .github/workflows/macos-release.yml` (if installed). Otherwise validate YAML: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/macos-release.yml'))" && echo OK`.
Expected: no errors / `OK`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/macos-release.yml
git commit -m "ci(macos): build + publish release DMG on v* tag"
```

---

### Task 6: Docs — user install section + Gatekeeper remedy

Give end users a written path: download → drag → quarantine one-liner → first-run CLI prompt.

**Files:**
- Modify: `macos/README.md` (add an "Install (for users)" section near the top; update the status line's version)

**Interfaces:**
- Consumes: the DMG naming from Task 4 (`TTStation-<ver>-arm64.dmg`) and the Gatekeeper remedy from the Global Constraints.

- [ ] **Step 1: Add the install section**

Insert after the opening description in `macos/README.md`:

```markdown
## Install (for users)

TTStation ships as a prebuilt **Apple Silicon** DMG on the repo's
[Releases](https://github.com/tsingletaryTT/tt-station/releases) page.

1. Download `TTStation-<version>-arm64.dmg` and open it.
2. Drag **TTStation.app** onto **Applications**.
3. The app is ad-hoc signed (no Apple Developer certificate yet), so macOS
   quarantines it and may say *"TTStation is damaged."* Clear the quarantine
   once:

   ```sh
   xattr -dr com.apple.quarantine /Applications/TTStation.app
   ```

4. Launch TTStation from Applications — it lives in the **menu bar**, not the
   Dock. On first run it offers to add the `tt` CLI to `~/.local/bin`
   (skippable; the app bundles its own copy and works either way). If you
   already have a different `tt` on your PATH, TTStation leaves it alone and
   offers to install as `tt-station` instead.

> Building from source instead? See `macos/install.sh` (needs Xcode + Rust).
> Notarizing to remove the quarantine step is a future upgrade once an Apple
> Developer certificate is available.
```

- [ ] **Step 2: Verify the doc renders / links resolve**

Run: `grep -n "Install (for users)" macos/README.md`
Expected: the new heading is present.

- [ ] **Step 3: Commit**

```bash
git add macos/README.md
git commit -m "docs(macos): user install section + Gatekeeper quarantine remedy"
```

---

### Task 7: Version bump + project CLAUDE.md note

Bump the app version (per the project's version-per-change rule) and record what shipped.

**Files:**
- Modify: `macos/TTStation/AppShell/project.yml` (`MARKETING_VERSION`)
- Modify: `macos/README.md` (status line version)
- Modify: `CLAUDE.md` (short note)

- [ ] **Step 1: Bump MARKETING_VERSION**

In `macos/TTStation/AppShell/project.yml`, change `MARKETING_VERSION: "0.8.2"` to `MARKETING_VERSION: "0.9.0"` (minor bump — new distribution capability).

- [ ] **Step 2: Update the README status line**

In `macos/README.md`, update the `**Status:** v0.6.2 built ...` line to `v0.9.0` (keep the rest of the sentence).

- [ ] **Step 3: Note it in CLAUDE.md**

Add to the macOS app bullet in `CLAUDE.md` (under "Current state"): a sentence that the app now bundles the `tt` CLI in `Contents/Resources/bin/tt`, ships as an arm64 DMG via `macos/make-release.sh` (local, also called by `.github/workflows/macos-release.yml` on `v*` tags), and that the first-run prompt installs the `~/.local/bin/tt` symlink with foreign-`tt` collision handling. Point to the spec (`docs/superpowers/specs/2026-07-09-macos-release-installer-design.md`).

- [ ] **Step 4: Verify the version is consistent**

Run: `grep -n "MARKETING_VERSION" macos/TTStation/AppShell/project.yml`
Expected: `0.9.0`.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/project.yml macos/README.md CLAUDE.md
git commit -m "chore(macos): bump to 0.9.0; document release-installer flow"
```

---

## Self-Review

**Spec coverage:**
- §1 self-contained DMG → Task 4 (hdiutil, embedded tt, `/Applications` alias, FIRST-RUN.txt).
- §2a bundled-tt resolution → Task 1.
- §2b first-run symlink + collision → Task 2 (planner) + Task 3 (wiring).
- §2c `~/.local/bin` creation → Task 3 (`applyLink` `createDirectory`).
- §3 make-release.sh → Task 4.
- §4 GitHub Actions → Task 5.
- §5 Gatekeeper docs (three places: DMG, Release notes, README) → Task 4 (FIRST-RUN.txt + `--publish` notes) + Task 6 (README).
- §6 housekeeping (version bump, README section, CLAUDE.md) → Tasks 6 & 7.
- Deferred items (vsix bundling, universal build, notarization) → intentionally out of scope; notarization referenced in README (Task 6) as the upgrade path.

**Placeholder scan:** No TBD/TODO; every code and shell step is complete and copy-pasteable.

**Type consistency:** `standardCandidates(home:bundledPath:)` and `standard(override:bundledPath:)` used consistently (Tasks 1, 3). `CLILinkTarget` / `CLILinkAction` / `CLILinkPlanner.plan(linkPath:bundledTT:state:)` identical across Tasks 2 and 3. DMG name `TTStation-<ver>-arm64.dmg` consistent across Tasks 4, 6. Bundled path `Contents/Resources/bin/tt` consistent across Tasks 1, 3, 4.

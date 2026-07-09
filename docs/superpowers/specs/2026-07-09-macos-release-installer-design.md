# macOS release installer — design

**Date:** 2026-07-09
**Status:** approved (brainstorm), pending implementation plan
**Topic:** A prebuilt, drag-to-install macOS distribution so people without the
source tree, Xcode, or Rust can install TTStation — built without an Apple
Developer license.

---

## Problem

"The macOS side" is **two binaries that must travel together**:

1. `TTStation.app` — the SwiftUI control-room app.
2. `tt` — the Rust CLI the app shells out to for *all* control
   (`TTBinaryLocator` probes `~/.local/bin/tt`, `/opt/homebrew/bin/tt`,
   `/usr/local/bin/tt`; the app only does read-only telemetry I/O itself).

Today `macos/install.sh` builds **both from source** — it needs the repo, the
Swift/Xcode toolchain, and the Rust toolchain. That is fine for the developer,
but not for "more people," who have none of those.

We want people to **download one file from a GitHub Release and drag it in.**
The hard constraint: **no Apple Developer license**, so we cannot notarize or
Developer-ID-sign. Gatekeeper friction is therefore unavoidable — the design
minimizes it and documents the one reliable remedy, and calls out notarization
as the clean future upgrade.

## Decisions (from brainstorming)

| Question | Decision |
|---|---|
| Distribution channel | **GitHub Release download** (drag-to-install DMG) |
| Build automation | **Both** — a local `make-release.sh` is the source of truth; CI calls the same script |
| Architectures | **Apple Silicon only (arm64)**; Intel/universal deferred |
| CLI delivery | **Bundle `tt` inside the app**, symlink into `~/.local/bin` on first run, **detect and work around a collision** with a foreign `tt` |

**Out of v1 (deferred, non-blocking):**
- Bundling the ~80 MB `tt-vscode-toolkit.vsix` into the app. The Workbench's
  VS Code launcher already falls back to the marketplace extension ID, so it
  degrades gracefully without the seeded cache.
- Intel / universal (`x86_64`) builds.
- Notarization (requires the Apple Developer cert — the clean Gatekeeper fix).

---

## Architecture

Six components. Each is independently understandable and testable.

### 1. Self-contained DMG (arm64)

- `tt` is embedded **inside** the app bundle at
  `TTStation.app/Contents/Resources/bin/tt`. Consequences:
  - The app always resolves a working `tt` even with an empty `$PATH`.
  - The CLI is **version-locked** to the app it shipped in — no drift.
- DMG contents:
  - `TTStation.app`
  - `/Applications` alias (the drag target)
  - `FIRST-RUN.txt` — the Gatekeeper step (see §5)
  - *(optional)* `Fix Gatekeeper.command` — double-click helper that runs the
    quarantine-strip on the installed app
- Assembled with **`hdiutil`** (no extra build-time dependency).
  - *Alternative considered:* `create-dmg` (Homebrew) for a windowed layout
    with background art. Deferred to keep the builder dependency-free; trivial
    to add later.

### 2. App-side Swift changes (small, TDD-able)

**2a. Bundled-`tt` resolution.** `TTBinaryLocator.standard()` gains the app
bundle's resource path as a candidate:

```
Bundle.main.resourceURL?.appendingPathComponent("bin/tt").path
```

Placement in the candidate list: the **bundled path is the reliable fallback**,
tried *after* the user override (`tt.binaryPath` UserDefault) and the three
PATH locations. Rationale: if the user has intentionally installed a `tt`
(including our own symlink) on PATH, honor it; only fall back to the in-bundle
copy when nothing else is found. This keeps the app working out-of-the-box on a
fresh machine while respecting an explicit user install.

The unit test extends the existing `TTBinaryLocator` tests: with all PATH
candidates absent and a bundled path present, `locate()` returns the bundled
path; with a PATH candidate present, it wins over the bundled path.

**2b. First-run CLI symlink with collision handling.** Guarded by a
`hasOfferedCLIInstall` (Bool) UserDefault so it runs once. On first launch, if
the app decides `tt` is not yet conveniently on the user's shell PATH, it offers
to symlink the bundled `tt` into `~/.local/bin/tt`. The **collision decision**
is pure logic and lives in `TTStationKit` so it is unit-tested without touching
the filesystem in tests (inject a small filesystem probe):

Given the intended link path `~/.local/bin/tt`:

| Existing state at `~/.local/bin/tt` | Action |
|---|---|
| Nothing there | Create the symlink → bundled `tt`. |
| Symlink whose target contains `/TTStation.app/` (i.e. **ours**) | Repoint it to *this* app's bundled `tt` (idempotent update). |
| A real file, or a symlink pointing elsewhere (**foreign `tt`**) | **Do not overwrite.** Leave it intact. Inform the user; offer to install ours as **`tt-station`** in the same dir instead, and surface the in-bundle path. |

"Ours" is detected by the **symlink target path containing `/TTStation.app/`** —
cheap, no process launch, no ambiguity. (We deliberately do *not* shell out to
`tt --version` to classify, which would be slower and could execute a foreign
binary.)

Because the app already works via the in-bundle copy (§2a), **every branch above
leaves the app fully functional** — the symlink is a convenience for the user's
own terminal, never a correctness dependency.

The first-run UI is a lightweight prompt (alert or a small sheet). Only the
**decision function** (state → action) needs unit tests; the alert wiring and
the actual `FileManager` symlink call are owner-verified like the rest of
`LaunchController`.

**2c. `~/.local/bin` creation.** If `~/.local/bin` does not exist, create it
before symlinking (mirrors how the CLI install already expects that dir).

### 3. `macos/make-release.sh` — local, source of truth

Ordered steps (all idempotent, `set -euo pipefail`):

1. Resolve version from `AppShell/project.yml`'s `MARKETING_VERSION`.
2. `cargo build --release -p tt` for `aarch64-apple-darwin`; capture the binary.
3. `xcodegen generate` + `xcodebuild ... -configuration Release build`
   (reuse the existing invocation from `install.sh`).
4. Copy the built `TTStation.app` into a clean staging dir; embed `tt` at
   `Contents/Resources/bin/tt` and `chmod +x`.
5. `codesign --force --deep --sign - "TTStation.app"` — ad-hoc sign the **whole
   bundle after embedding** so the embedded `tt` is covered by the signature
   (an unsigned nested executable would break the seal).
6. Build the DMG staging folder (app + `/Applications` alias + `FIRST-RUN.txt`
   + optional `.command`); `hdiutil create -volname "TTStation <ver>"
   -srcfolder <stage> -format UDZO -fs HFS+ dist/TTStation-<ver>-arm64.dmg`.
7. Print the output path and its SHA-256.
8. `--publish` flag → `gh release create v<ver>` (or upload asset if the release
   exists) with the DMG and generated notes.

The script must run standalone on a clean Mac checkout (only Xcode CLT + Rust +
xcodegen assumed). It supersedes nothing — `install.sh` remains the
from-source developer install.

### 4. GitHub Actions — `.github/workflows/macos-release.yml`

- Trigger: push of a `v*` tag (plus `workflow_dispatch` for manual runs).
- Runner: `macos-14` (Apple Silicon, native arm64).
- Steps: checkout → ensure Rust toolchain → `brew install xcodegen` →
  `macos/make-release.sh --publish` (uses the workflow's `GITHUB_TOKEN` for
  `gh`).
- **Caveats (documented, accepted):** the repo is private, so this consumes
  Actions minutes (may warrant a TT org/self-hosted runner later); CI ad-hoc
  signs identically to local — neither is notarized.

### 5. Gatekeeper story (honest)

Not notarized + ad-hoc signed + downloaded from the internet → macOS applies the
`com.apple.quarantine` xattr, and on **Apple Silicon** this commonly surfaces as
**"TTStation is damaged and can't be opened"** — which **right-click → Open does
not fix.** The single reliable remedy after dragging the app in:

```sh
xattr -dr com.apple.quarantine /Applications/TTStation.app
```

This is documented in three places, in the user's likely order of discovery:
1. `FIRST-RUN.txt` inside the DMG.
2. The GitHub Release notes (auto-included by `make-release.sh`).
3. `macos/README.md` → "Install (for users)".

Optional polish: a `Fix Gatekeeper.command` in the DMG that runs the command on
the installed app with a friendly echo. (It, too, is quarantined and prompts
once on first double-click — so the text one-liner remains the primary,
copy-pasteable path.)

**Clean upgrade path (noted, not built):** an Apple Developer ID + notarization
removes this friction entirely. See the existing memory note on obtaining TT
Apple Developer credentials.

### 6. Housekeeping

- Bump `MARKETING_VERSION` in `project.yml` (per the project's version-per-change
  convention) and the README status line.
- `macos/README.md`: add an "Install (for users)" section (download → drag →
  quarantine one-liner → first-run CLI prompt explanation).
- Project `CLAUDE.md`: short "what happened" note + point to this spec and the
  `make-release.sh` / workflow.

---

## Data flow (install, end to end)

```
release build (make-release.sh / CI)
  cargo build tt (arm64) ─┐
  xcodebuild app  ────────┤→ embed tt in .app → ad-hoc codesign → hdiutil → DMG → GitHub Release
                                                                                   │
user
  download DMG → open → drag TTStation.app to /Applications
  xattr -dr com.apple.quarantine /Applications/TTStation.app   (FIRST-RUN.txt)
  launch app
     ├─ TTBinaryLocator finds tt: override? PATH? → else Contents/Resources/bin/tt  (always works)
     └─ first run: offer ~/.local/bin/tt symlink
            ├─ empty  → create symlink
            ├─ ours   → repoint
            └─ foreign→ keep theirs; offer `tt-station`; app still works via bundle
```

## Testing

- **Unit (TTStationKit, no hardware, no filesystem writes):**
  - `TTBinaryLocator`: bundled path used as last-resort fallback; PATH/override
    still win when present.
  - CLI-symlink **decision function**: the four collision states → correct
    action, given an injected filesystem probe.
- **Owner-verified (LaunchController-style, not unit-tested):** the actual
  symlink creation, the first-run alert, and the `~/.local/bin` mkdir.
- **Manual release smoke:** run `make-release.sh` on this Mac → mount the DMG →
  drag-install on a *second* account/machine (or after clearing UserDefaults)
  → confirm the Gatekeeper one-liner works, the app launches, `tt` resolves
  from the bundle, and the first-run prompt behaves in all three collision
  states.

## Risks / open items

- **"Damaged" vs "unidentified developer":** the exact Gatekeeper string depends
  on macOS version and whether ad-hoc signing is present; the `xattr` one-liner
  covers both, which is why it's the documented primary path.
- **CI signing identity:** ad-hoc signatures differ per build machine but are
  functionally equivalent (both untrusted); acceptable until notarization.
- **`gh` auth in CI:** relies on the default `GITHUB_TOKEN` having
  `contents: write` — set `permissions:` in the workflow.

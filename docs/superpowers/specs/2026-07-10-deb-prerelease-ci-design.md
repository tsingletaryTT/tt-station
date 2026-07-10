# .deb pre-release CI (manual on-demand build) — design

**Date:** 2026-07-10
**Status:** approved (brainstorming) + implemented
**Builds on:** `.github/workflows/release.yml` (per-suite `.deb` build → GitHub Release on `v*` tags).

## Goal

Let anyone produce the two `.deb`s **on demand, without cutting a tag or creating a
Release** — a pre-release flow for grabbing a test build. The manual build uploads the
packages as downloadable GitHub Actions artifacts.

## Decisions (from brainstorming)

- **Trigger:** manual `workflow_dispatch` only (no push/PR builds, no rc/beta-tag
  pre-releases, no rolling `edge` release).
- **Output:** GitHub Actions artifacts only — no GitHub Release is created or touched by a
  manual run.
- **Location:** extend the existing `release.yml` rather than add a second workflow, so the
  build matrix + steps (apt prereqs, rustup, changelog-suite patch, `build-deb.sh`,
  suite-suffix rename) are defined once. Only the terminal publish step differs by event.

## Implementation

In `.github/workflows/release.yml`:

- `on:` gains `workflow_dispatch:` (keeps `push: tags: ['v*']`).
- The existing "Ensure release exists, then upload packages" step is gated
  `if: github.event_name == 'push'` — a manual run never touches a Release and never
  mis-resolves `$GITHUB_REF_NAME` to a branch name.
- Two new steps gated `if: github.event_name == 'workflow_dispatch'`:
  1. **Stage** — copy that suite's two `.deb`s from the parent dir (where
     `dpkg-buildpackage` writes them) into `dist/` inside the workspace. Staging avoids
     `actions/upload-artifact@v4`'s mishandling of paths above the workspace root (it
     derives artifact layout from the paths' common ancestor).
  2. **Upload** — `actions/upload-artifact@v4` with `name: tt-station-debs-<suite>` (v4
     requires unique artifact names per job, so the name is keyed on the matrix suite),
     `path: dist/*.deb`, `if-no-files-found: error`, `retention-days: 90`.

`/dist/` added to `.gitignore` (CI-only staging dir).

Net behavior:
- **Tag `v*`** → per-suite Release upload (unchanged).
- **Actions → Run workflow (manual)** → per-suite downloadable artifacts
  (`tt-station-debs-noble`, `tt-station-debs-jammy`), no Release.

## Testing

- YAML validity + guard check (done locally): exactly one terminal publish path runs per
  event (`push` → release step; `workflow_dispatch` → stage+upload steps), and the artifact
  name interpolates the matrix suite so the two jobs don't collide.
- End-to-end only exercises on GitHub (like the rest of `release.yml`): trigger a manual run
  and confirm two artifacts appear on the run page, each holding both `.deb`s.

## Deferred (not now)

- Pre-release GitHub Releases on rc/beta tags; CI artifacts on every push/PR; a rolling
  `edge`/nightly pre-release. All were considered and declined for this pass.

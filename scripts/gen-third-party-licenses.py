#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""Regenerate THIRD_PARTY_LICENSES.md.

Sources:
  * Rust: `cargo tree --workspace -e normal,build --target all` (dependencies the
    workspace crates actually compile, every platform; dev-dependencies are left out
    because they never ship).
  * npm (desktop/): production packages from desktop/package-lock.json (entries not
    marked "dev"). The lockfile often omits `license`, so each package's own
    package.json is read from a node_modules tree:
        cd desktop && npm ci --omit=dev --ignore-scripts
        scripts/gen-third-party-licenses.py
    or point at another install with --node-modules DIR.
  * Bundled assets (fonts, the iOS SwiftPM package) are described by hand below,
    and the SIL Open Font License text is copied from ios/Resources/Fonts/.

Usage: scripts/gen-third-party-licenses.py [--node-modules DIR] [--check]
  --check   exit 1 if the file on disk differs from what would be generated.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD_PARTY_LICENSES.md"
FONTS = ROOT / "ios/Resources/Fonts"

LEGAL_REVIEW = """\
## Flagged for legal review

These are not blockers to building, but need a decision before TT Station ships company-wide.

| Item | Where | License | Why it is flagged |
|---|---|---|---|
| **n8n** (`docker.n8n.io/n8nio/n8n`) | App catalogue, `crates/tt-station-agentd/src/apps.rs` (one-click install in Docker) | Sustainable Use License (fair-code, **not** OSI open source) | We do not redistribute the image (the user's box pulls it), but offering it from a Tenstorrent product may count as "making it available" commercially; the license restricts commercial redistribution and hosting for others. |
| **`terminfo`** (Rust crate) | In `Cargo.lock` through `ratatui`'s optional termwiz backend (`tt-station console`). **Not compiled today** (the feature is off), so it is absent from the table below, but enabling that backend would ship it | WTFPL | Not on most corporate allow-lists; permissive in effect but often needs explicit sign-off. |
| **`webpki-roots`** (Rust crate) | Transitive dependency of the TLS stack (`reqwest`/`rustls`) | CDLA-Permissive-2.0 | A data license (Mozilla's root CA list), not a software license; requires keeping the license text with redistributions. |
"""

BUNDLED = """\
## Bundled assets

| Asset | Shipped in | License | Copyright |
|---|---|---|---|
| IBM Plex Mono (`IBMPlexMono-*.ttf`) | iOS app bundle (`ios/Resources/Fonts`); desktop app via `@fontsource/ibm-plex-mono` (WOFF/WOFF2 in the Vite build) | OFL-1.1 | © 2017 IBM Corp., Reserved Font Name "Plex" |
| Inter Tight (`InterTight-*.ttf`) | iOS app bundle (`ios/Resources/Fonts`); desktop app via `@fontsource/inter-tight` | OFL-1.1 | © 2022 The Inter Project Authors |
| SwiftTerm | iOS app (Swift Package, `ios/project.yml`) | MIT | © Miguel de Icaza and contributors |

The OFL requires that the license text travel with the fonts. The iOS bundle ships
`OFL-IBMPlexMono.txt` and `OFL-InterTight.txt` next to the font files, the
`@fontsource/*` npm packages carry their own `LICENSE`, and the full text is
reproduced at the end of this file.
"""


def rust_deps() -> list[tuple[str, str, str, str]]:
    # `cargo tree` (not `cargo metadata`): it is feature-aware, so optional deps that
    # sit in Cargo.lock but are never compiled (e.g. ratatui's unused termwiz
    # backend) are left out, matching what actually ships.
    out = subprocess.check_output(
        ["cargo", "tree", "--workspace", "--locked", "-e", "normal,build", "--target", "all",
         "--prefix", "none", "--format", "{p}|{l}|{r}"],
        cwd=ROOT, text=True,
    )
    rows = set()
    for line in out.splitlines():
        line = line.removesuffix(" (*)")
        if not line.strip() or "|" not in line:
            continue
        pkg, lic, repo = (line.split("|") + ["", ""])[:3]
        words = pkg.split()
        name, ver = words[0], words[1].lstrip("v")
        if any(w.startswith("(/") for w in words[2:]):
            continue  # a workspace crate (path dependency): ours, Apache-2.0
        rows.add((name, ver, lic or "UNKNOWN", repo))
    return sorted(rows, key=lambda r: (r[0].lower(), r[1]))


def npm_deps(node_modules: Path | None) -> list[tuple[str, str, str, str]]:
    lock = json.loads((ROOT / "desktop/package-lock.json").read_text())
    base = node_modules.parent if node_modules else ROOT / "desktop"
    rows = []
    for path, entry in lock["packages"].items():
        if not path or entry.get("dev") or entry.get("devOptional"):
            continue
        name = path.split("node_modules/")[-1]
        lic = entry.get("license")
        repo = ""
        pj = base / path / "package.json"
        if pj.is_file():
            data = json.loads(pj.read_text())
            lic = lic or data.get("license")
            if isinstance(lic, dict):
                lic = lic.get("type")
            r = data.get("repository")
            repo = r.get("url", "") if isinstance(r, dict) else (r or "")
        rows.append((name, entry.get("version", ""), lic or "UNKNOWN (run npm ci first)", repo))
    return sorted(set(rows), key=lambda r: (r[0].lower(), r[1]))


def table(rows) -> str:
    out = ["| Package | Version | License | Repository |", "|---|---|---|---|"]
    for name, ver, lic, repo in rows:
        repo = repo.replace("git+", "").removesuffix(".git")
        out.append(f"| {name} | {ver} | {lic} | {repo} |")
    return "\n".join(out)


def summary(rows) -> str:
    c = Counter(r[2] for r in rows)
    return "\n".join(f"- {lic}: {n}" for lic, n in sorted(c.items(), key=lambda x: (-x[1], x[0])))


def render(node_modules: Path | None) -> str:
    rust = rust_deps()
    npm = npm_deps(node_modules)
    ofl = []
    for f in sorted(FONTS.glob("OFL-*.txt")):
        ofl.append(f"### {f.stem.removeprefix('OFL-')} (`{f.relative_to(ROOT)}`)\n\n```text\n{f.read_text().rstrip()}\n```")
    return f"""<!-- Generated by scripts/gen-third-party-licenses.py. Do not edit by hand. -->
# Third-party licenses

TT Station itself is licensed under Apache-2.0 (see `LICENSE` and `NOTICE`). This file
lists the third-party software and assets that TT Station's binaries and apps include,
and the licenses they come under. It is generated; regenerate it after changing
`Cargo.lock`, `desktop/package-lock.json` or the bundled assets.

{LEGAL_REVIEW}
{BUNDLED}
## Rust crates (shipped in `tt-station`, `tt-station-agentd`, `mock-box`)

Normal and build dependencies of the workspace crates (dev-dependencies excluded),
all target platforms, as `cargo tree` resolves features. {len(rust)} crates.

{summary(rust)}

{table(rust)}

## npm packages (desktop app, production dependencies)

From `desktop/package-lock.json`, excluding devDependencies (Electron itself, the
build toolchain and test tools). {len(npm)} packages.

{summary(npm)}

{table(npm)}

## SIL Open Font License 1.1

{chr(10).join(ofl)}
"""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--node-modules", type=Path, help="node_modules dir of a desktop install")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    nm = args.node_modules.resolve() if args.node_modules else None
    text = render(nm)
    if args.check:
        if OUT.read_text() != text:
            print(f"{OUT.name} is out of date; run scripts/gen-third-party-licenses.py", file=sys.stderr)
            return 1
        return 0
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

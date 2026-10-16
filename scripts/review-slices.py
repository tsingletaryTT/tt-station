#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

"""Cut a branch into a stack of cumulative review branches, one area each.

Every file the branch changes against the base is assigned to exactly one slice
(the first whose patterns match). Slice N's branch is slice N-1's plus that
slice's files taken verbatim from the source branch, so each review shows one
area, and the last branch's tree is the source branch's. See docs/landing-plan.md.

    scripts/review-slices.py --list                # sizes and unassigned files, changes nothing
    scripts/review-slices.py --build WORKTREE      # (re)create review/* branches in WORKTREE
"""

import argparse
import re
import subprocess
import sys

BASE = "origin/main"
SOURCE = "jberkowitz/mobile-voice"
S = "crates/tt-station-agentd/src/"
T = "crates/tt-station-agentd/tests/"
D = "docs/reference/"


def src(*names):
    return [S + n for n in names]


def tests(*names):
    return [T + n for n in names]


def docs(*names):
    return [D + n + ".md" for n in names]


# (branch suffix, title, patterns). Order is review order.
SLICES = [
    ("00-hygiene", "Build, CI, packaging and licenses", [
        ".github/*", "scripts/*", "deny.toml", "NOTICE", "THIRD_PARTY_LICENSES.md",
        ".gitattributes", ".gitignore", "debian/*", "build-deb.sh", "deploy/*",
        "README.md", "Cargo.toml", "Cargo.lock", "CLAUDE.md", "docs/landing-plan.md",
        *docs("installing", "releases", "pinned-dependencies", "box-hardening"),
    ]),
    ("01-cli", "CLI and client library", [
        "crates/tt-station/*", "crates/libttstation/*",
        *docs("cli-https-pinning"),
    ]),
    ("02-agent-core", "Agent core: wiring, sign-in, devices, roles, audit", [
        "crates/tt-station-agentd/Cargo.toml", "crates/mock-box/*",
        *src("lib.rs", "main.rs", "state.rs", "state/*", "router.rs", "routes.rs",
             "handlers/mod.rs", "auth.rs", "error.rs", "localpeer.rs", "sessions.rs",
             "pairing.rs", "pairqr.rs", "tls.rs", "web.rs", "audit.rs", "authkeys.rs",
             "config.rs", "net.rs", "log.rs", "fsio.rs", "scope.rs", "health.rs",
             "urltrust.rs", "model_control.rs", "people.rs",
             "handlers/status.rs", "handlers/pairing.rs", "handlers/devices.rs",
             "handlers/security.rs", "handlers/ssh.rs"),
        *tests("access_control.rs", "devices.rs", "error_bodies.rs", "pairing.rs",
               "pair_offer.rs", "token_persistence.rs", "ws_auth.rs", "status.rs",
               "config.rs", "model_control.rs", "people.rs", "support/*",
               "fixtures/proc_net_*"),
        "docs/security-faq.md", "docs/design/enterprise-identity.md",
        *docs("agentd-config"),
    ]),
    ("03-serving-chips", "Serving models and sharing the chips", [
        *src("serving/*", "served.rs", "localhttp.rs", "device.rs", "ttcli.rs",
             "tttools.rs", "download.rs", "native_vllm.rs", "discover.rs", "history.rs",
             "inference.rs", "telemetry.rs", "procscan.rs", "gozer.rs", "prefs.rs",
             "favorites.rs", "coserve.rs", "modes.rs", "liveness.rs", "logs.rs",
             "handlers/serving.rs", "handlers/ttcli.rs", "handlers/telemetry.rs",
             "handlers/logs.rs"),
        *tests("serve.rs", "runpy.rs", "ttcli.rs", "metrics.rs", "serving.rs", "gozer.rs",
               "gozer_client.rs", "leases.rs", "lease_ownership.rs", "chip_leases.rs",
               "logs.rs", "logs_stream.rs", "reset.rs", "models.rs", "telemetry.rs",
               "coserve.rs", "modes.rs", "native_vllm.rs", "liveness.rs"),
        *docs("tt-cli", "tt-inference-server-docker", "chip-leasing", "logs", "tt-console"),
    ]),
    ("04-box-care", "Power, sleep and wake, updates, backups, doctor", [
        *src("power.rs", "idle.rs", "doctor.rs", "updates.rs", "release_keys.rs",
             "boot.rs", "export.rs", "handlers/power.rs"),
        *tests("updates.rs", "export.rs", "fixtures/release-sig/*"),
        *docs("box-care", "power-controls", "sleep-and-wake", "alerts-and-power", "your-data"),
    ]),
    ("05-assistant-gateway", "OpenAI gateway and the assistant", [
        *src("gateway.rs", "anthropic.rs", "bench.rs", "creations.rs", "handlers/media.rs",
             "assistant.rs", "memory.rs", "docs.rs", "embeddings.rs", "approvals.rs",
             "skills.rs", "library.rs", "handlers/assistant.rs", "handlers/memories.rs",
             "handlers/docs.rs"),
        *tests("gateway.rs", "anthropic.rs", "assistant.rs", "library.rs"),
        *docs("gateway", "assistant"),
    ]),
    ("06-household", "Notifications, voice, meetings and jobs", [
        *src("notify.rs", "apns.rs", "schedules.rs", "jobs.rs", "recorder.rs", "voice.rs",
             "chime.rs", "tori.rs"),
        *tests("notify.rs", "jobs.rs", "voice.rs", "schedules.rs"),
        *docs("jobs", "voice-and-desktop", "household-schedules-remote"),
    ]),
    ("07-research-routines", "Research, routines and model check-ups", [
        *src("research.rs", "routines.rs", "evals.rs", "reliability.rs"),
        "crates/tt-station-agentd/examples/*",
        *tests("research_jobs.rs", "routines.rs", "routines_api.rs", "evals_*"),
        "docs/notes/agent-reliability-*", "docs/tasks-framework.md",
    ]),
    ("08-apps", "Store apps, desktop launches and community models", [
        *src("apps.rs", "app_ui.rs", "desktop.rs", "hermes.rs", "community.rs",
             "toplike_term.rs"),
        *tests("apps.rs", "community_*"),
    ]),
    ("09-bringup-lab", "Orchard, Lab and model bring-up agents", [
        *src("orchard*", "lab.rs", "lease_run.py", "bringup.rs", "*_bringup.py",
             "permission_mcp.py", "sandbox.rs", "sandbox_exec.py", "test_*.py"),
        *tests("orchard.rs", "bringup_*", "bringup_support/*", "fixtures/fake-orchard/*"),
        *docs("orchard", "lab", "bringup-containment"),
    ]),
    ("10-code", "Code workspace (the agent builds from here)", [
        *src("workspace.rs", "code_session.py", "code_holder.py", "safegit.rs",
             "coding.rs", "codespeed.rs", "codemodels.rs"),
        *tests("workspace_*", "code_*"),
        *docs("code-models"), "docs/design/code-serving.md", "docs/notes/tool-call-*",
    ]),
    ("11-desktop-shell", "Desktop app shell and shared pieces", [
        "desktop/electron/*", "desktop/test/*", "desktop/mac/*", "desktop/linux/*", "desktop/gnome-extension/*",
        "desktop/assets/*", "desktop/public/*", "desktop/*.json", "desktop/*.ts",
        "desktop/*.html", "desktop/*.sh", "desktop/*.md", "desktop/.gitignore",
        "desktop/src/*.ts", "desktop/src/*.tsx", "desktop/src/app.css",
        "desktop/src/assets/*",
    ]),
    ("12-desktop-pages", "Desktop pages", ["desktop/src/pages/*"]),
    ("13-ios-core", "iPhone app: connection, pairing, box control", [
        "ios/project.yml", "ios/Info.plist", "ios/*.entitlements", "ios/README.md",
        "ios/Assets.xcassets/*", "ios/Resources/*", "ios/Shared/*", "ios/Tests/*",
        *["ios/Sources/" + n + ".swift" for n in (
            "AgentAPI", "AppModel", "BoxSession", "BoxStore", "BoxesView", "Discovery",
            "PairSheet", "PairLink", "SSHKey", "Brand", "ChipStyle", "KeyboardDone",
            "Markdown", "TTStationMobileApp", "BoxControlAPI", "BoxControlLogic",
            "BoxControlModels", "Power", "SleepViews", "DevicesViews", "SettingsView",
            "SettingsPages", "Telemetry", "DashboardView", "DashboardCharts",
            "GlanceViews", "BoxCare")],
    ]),
    ("14-ios-features", "iPhone app: features and extensions", [
        "ios/Sources/*", "ios/ActivityExtension/*", "ios/NotificationService/*",
        "docs/reference/mobile-parity.md",
    ]),
    ("15-docs", "Presentations, notes and the benchmark summary", [
        "docs/presentations/*", "docs/notes/flux-*", "docs/notes/tt-media-*",
        "docs/notes/code-serving-benchmark-2026-10-15/*",
    ]),
]


def matches(path, pat):
    """A trailing "/*" takes everything under a folder; any other "*" stays within one."""
    if pat.endswith("/*"):
        return path.startswith(pat[:-1])
    return re.fullmatch(re.escape(pat).replace(r"\*", "[^/]*"), path) is not None


def git(*args, cwd=None, check=True):
    return subprocess.run(["git", *args], cwd=cwd, check=check, text=True,
                          capture_output=True).stdout


def assign():
    changed = {}
    for line in git("diff", "--numstat", f"{BASE}...{SOURCE}").splitlines():
        a, d, path = line.split("\t")
        changed[path] = (0 if a == "-" else int(a)) + (0 if d == "-" else int(d))
    deleted = {l.split("\t")[1] for l in git("diff", "--name-status", f"{BASE}...{SOURCE}").splitlines()
               if l.startswith("D")}
    plan = {name: [] for name, _, _ in SLICES}
    left = []
    for path in sorted(changed):
        for name, _, pats in SLICES:
            if any(matches(path, p) for p in pats):
                plan[name].append(path)
                break
        else:
            left.append(path)
    return changed, deleted, plan, left


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--build", metavar="WORKTREE")
    args = ap.parse_args()
    changed, deleted, plan, left = assign()
    for name, title, _ in SLICES:
        files = plan[name]
        print(f"{name:22} {sum(changed[f] for f in files):>7} lines {len(files):>4} files  {title}")
    if left:
        print("\nUNASSIGNED:", *left, sep="\n  ")
        sys.exit(1)
    if not args.build:
        return
    wt = args.build
    git("switch", "--detach", BASE, cwd=wt)
    for i, (name, title, _) in enumerate(SLICES):
        files = plan[name]
        git("switch", "-C", f"review/{name}", cwd=wt)
        keep = [f for f in files if f not in deleted]
        gone = [f for f in files if f in deleted]
        for k in range(0, len(keep), 200):
            git("checkout", SOURCE, "--", *keep[k:k + 200], cwd=wt)
        if gone:
            git("rm", "-q", "--", *gone, cwd=wt)
        git("-c", "user.name=jberkowitz", "-c", "user.email=jberkowitz@tenstorrent.com",
            "commit", "-q", "-m",
            f"review {i:02d}/{len(SLICES) - 1:02d}: {title}\n\n"
            f"Slice {name} of {SOURCE} (docs/landing-plan.md). Files are taken verbatim\n"
            f"from that branch; the last slice's tree equals it.\n\n"
            "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>", cwd=wt)
    rest = git("diff", "--stat", SOURCE, "HEAD", cwd=wt)
    print("\nlast slice vs", SOURCE + ":", "identical" if not rest else rest)


if __name__ == "__main__":
    main()

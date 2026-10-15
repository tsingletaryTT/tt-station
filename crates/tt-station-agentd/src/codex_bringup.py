# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""TT Station's driver for the fast-models-fast Codex runner.

Runs tt-metal's own `.agents/scripts/multigoal` unchanged (its stages,
gates, manifest and logs), with one difference: Codex's approval requests go
to TT Station instead of being accepted automatically. The agent shows them in
the app (and on the phone) and answers here.

    python codex_bringup.py --policy smart -- <multigoal arguments>

Environment: TT_STATION_URL (the agent, e.g. http://127.0.0.1:8765),
TT_STATION_RUN (this run's id) and TT_STATION_RUN_SECRET (proves the driver
is the run's own when it reports approvals).

Policies:
  ask    every approval request waits for a person
  smart  routine work is approved here (tests, builds, edits inside the
         repo); anything risky waits for a person (chip resets, sudo, deleting
         outside the repo, git push, network installs, writes outside the repo)
  auto   everything is approved (Codex runs with approval policy `never`)
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

# A convenience, not containment: it keeps routine work from waiting on a
# person and catches the obvious slips. A command can always be spelled so
# that no pattern sees it (a script, a variable, another language); what
# actually contains a bring-up is the account it runs as and the chips it
# leases, and `ask` for anything that matters.
RISKY = [
    (re.compile(r"\btt-smi\b.*\s-r\b|\btt-smi\s+-r\b|\breset\b.*\bboard"), "resets chips"),
    (re.compile(r"\bsudo\b"), "uses sudo"),
    (re.compile(r"\brm\s+-[a-z]*r[a-z]*f?\s+(/|~|\$HOME)(\s|$)"), "deletes outside the repo"),
    (re.compile(r"""\brm\s+(-\S+\s+)*["']?(/\*|~|\$HOME\b|\$\{HOME\})"""), "deletes in your home folder"),
    (re.compile(r"\bpython3?\b.*\s-c\b.*\b(shutil\.rmtree|os\.(remove|unlink|rmdir|removedirs))\b"), "deletes files from Python"),
    (re.compile(r"""(>|\btee\s+(-a\s+)?)\s*["']?(~|\$HOME\b|\$\{HOME\})/\."""), "changes a settings file in your home folder"),
    (re.compile(r"\bgit\s+push\b"), "pushes to a remote"),
    (re.compile(r"\b(pip|uv|npm|apt|apt-get)\s+(install|add)\b"), "installs packages"),
    (re.compile(r"\b(docker|systemctl)\b"), "changes services or containers"),
    (re.compile(r"\b(shutdown|reboot|mkfs|dd\s+if=)\b"), "could damage the system"),
    (re.compile(r"\bcurl\b.*\|\s*(ba)?sh\b"), "runs a downloaded script"),
]


def inside(path: str, root: str) -> bool:
    """Whether `path` is `root` or under it, symlinks resolved (not a sibling
    that merely starts the same way, like tt-metal-old next to tt-metal)."""
    p, r = os.path.realpath(path), os.path.realpath(root)
    try:
        return os.path.commonpath([p, r]) == r
    except ValueError:
        return False


def risk(command: str | None, cwd: str | None, repo: str) -> str | None:
    """Why this command needs a person, or None if it's routine."""
    if not command:
        return "an unnamed command"
    for pattern, why in RISKY:
        if pattern.search(command):
            return why
    if cwd and not inside(cwd, repo):
        return f"runs outside the repo ({cwd})"
    return None


def post(path: str, body: dict) -> dict:
    url = os.environ["TT_STATION_URL"].rstrip("/") + path
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "X-Run-Secret": os.environ.get("TT_STATION_RUN_SECRET", "")},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=40) as res:
        return json.loads(res.read() or b"{}")


def ask_person(kind: str, params: dict, why: str) -> str:
    """Report the request to TT Station and wait (as long as it takes) for a decision."""
    run = os.environ["TT_STATION_RUN"]
    summary = {
        "kind": kind,
        "why": why,
        "command": params.get("command"),
        "cwd": params.get("cwd"),
        "reason": params.get("reason"),
        "grant_root": params.get("grantRoot"),
        "thread_id": params.get("threadId"),
        "item_id": params.get("itemId"),
    }
    while True:
        try:
            r = post(f"/bringup/codex/runs/{run}/approvals", summary)
            approval_id = r["id"]
            break
        except (urllib.error.URLError, OSError, KeyError):
            time.sleep(5)  # the agent restarting: keep trying
    print(f"[tt-station] waiting for a person: {kind} ({why}): {params.get('command') or ''}", file=sys.stderr, flush=True)
    while True:
        try:
            r = post(f"/bringup/codex/runs/{run}/approvals/{approval_id}/wait", {})
            if r.get("decision") in ("accept", "acceptForSession", "decline", "cancel"):
                print(f"[tt-station] decided: {r['decision']}", file=sys.stderr, flush=True)
                return r["decision"]
        except (urllib.error.URLError, OSError):
            time.sleep(5)


# TT Station serves a port natively once its vLLM stage passes; this is how it knows how.
SERVE_RECIPE = (
    "Also, once the vLLM server works: write <model dir>/doc/vllm_integration/serve.json as "
    '{"command": [the exact argv that launched the vLLM OpenAI server, starting with python], '
    '"env": {the environment variables it needed, such as MESH_DEVICE}}, so TT Station can serve '
    "this port with native vLLM."
)


def take_note(path: str | None) -> str | None:
    """A person's note from TT Station, once: it's marked delivered when read."""
    if not path or not os.path.isfile(path):
        return None
    with open(path) as f:
        text = f.read().strip()
    os.rename(path, os.path.join(os.path.dirname(path), f"note.delivered-{int(time.time())}.txt"))
    return text or None


def main() -> int:
    argv = sys.argv[1:]
    policy = "smart"
    if argv[:1] == ["--policy"]:
        policy, argv = argv[1], argv[2:]
    if argv[:1] == ["--"]:
        argv = argv[1:]
    note_file = None
    if "--note-file" in argv:
        i = argv.index("--note-file")
        note_file, argv = argv[i + 1], argv[:i] + argv[i + 2:]
    repo = os.getcwd()
    runner = os.path.join(repo, ".agents", "scripts", "multigoal")
    loader = importlib.machinery.SourceFileLoader("multigoal", runner)
    spec = importlib.util.spec_from_loader("multigoal", loader)
    multigoal = importlib.util.module_from_spec(spec)
    sys.modules["multigoal"] = multigoal  # its dataclasses look themselves up here
    loader.exec_module(multigoal)

    def handle(self, message, log):  # replaces AppServerClient._handle_server_request
        method = message.get("method")
        params = message.get("params") if isinstance(message.get("params"), dict) else {}
        result: dict = {}
        if method in ("item/commandExecution/requestApproval", "item/fileChange/requestApproval"):
            kind = "command" if "commandExecution" in method else "file change"
            if method.endswith("fileChange/requestApproval"):
                root = params.get("grantRoot")
                why = None if not root or inside(root, repo) else f"writes outside the repo ({root})"
            else:
                why = risk(params.get("command"), params.get("cwd"), repo)
            if policy == "ask":
                why = why or "you asked to approve everything"
            decision = "accept" if (policy == "auto" or why is None) else ask_person(kind, params, why)
            result = {"decision": decision}
        elif method == "item/permissions/requestApproval":
            decision = "accept" if policy == "auto" else ask_person("permissions", params, "asks for wider permissions")
            result = {"permissions": params.get("permissions", {}), "scope": "session"} if decision.startswith("accept") else {"permissions": {}, "scope": "turn"}
        self._send({"id": message["id"], "result": result}, log)

    multigoal.AppServerClient._handle_server_request = handle

    # A person's note from TT Station goes ahead of the next message Codex gets
    # (the resumed stage's "continue", or the next stage's goal), once.
    def with_note(build):
        def wrapped(*a, **kw):
            items, missing = build(*a, **kw)
            # Only where the items are sent (run_stage also calls this, to check skills).
            if sys._getframe(1).f_code.co_name not in ("execute_goal", "execute_resumed_goal"):
                return items, missing
            objective = a[-1] if a else kw.get("objective", "")
            if isinstance(objective, str) and "vllm-integration" in objective:
                items = [*items, {"type": "text", "text": SERVE_RECIPE}]
            note = take_note(note_file)
            if note:
                items = [{"type": "text", "text": f"The person overseeing this bring-up in TT Station says:\n\n{note}\n\nTake that into account."}, *items]
            return items, missing
        return wrapped

    multigoal.input_items_for_resume = with_note(multigoal.input_items_for_resume)
    multigoal.input_items_for_objective = with_note(multigoal.input_items_for_objective)
    approval = "never" if policy == "auto" else "untrusted"
    sys.argv = ["multigoal", "--approval-policy", approval, "--sandbox", "danger-full-access", *argv]
    return multigoal.main()


if __name__ == "__main__":
    raise SystemExit(main())

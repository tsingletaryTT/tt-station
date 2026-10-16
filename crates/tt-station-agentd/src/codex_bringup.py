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
is the run's own when it reports approvals; removed from the environment the
stage inherits).

Policies:
  ask    every approval request waits for a person
  smart  routine work is approved here (tests, builds, edits inside the
         repo); anything risky waits for a person (chip resets, sudo, deleting
         outside the repo, installs, writes outside the repo, and anything that
         talks to another machine: curl, ssh, git push, inline network code...)
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

# The run's secret proves this driver (and its permission server) is the run's
# own when it reports approvals. Nothing the stage runs needs it, so it leaves
# the environment here, before Codex (and every command it runs) inherits it.
RUN_SECRET = os.environ.pop("TT_STATION_RUN_SECRET", "")

# `git`, then its global options, then the subcommand.
GIT = r"\bgit(\s+(-[Cc]\s+\S+|--?[\w-]+(=\S+)?))*\s+"
# An interpreter, and code in the command line that reaches the network: inline
# code (`python -c`, `node -e`, a heredoc) is where a script would hide it.
INTERPRETER = re.compile(r"(?<![\w.-])(python[\d.]*|node|nodejs|deno|bun|perl|ruby|php|lua|Rscript)(?![\w./=-])")
NET_CODE = re.compile(
    r"(\b(import|from)\s+(socket|ssl|urllib\w*|requests|httpx|http|aiohttp|ftplib|smtplib|poplib|imaplib|telnetlib"
    r"|paramiko|websockets?|websocket_client|xmlrpc|huggingface_hub|boto3|pycurl|asyncssh|grpc)\b"
    r"|__import__|\bimportlib\b|\burlopen\b|-m\s+http\.server\b"
    r"""|require\(\s*['"](node:)?(https?|http2|net|dgram|tls|child_process)['"]|\bfetch\(|from\s+['"](node:)?(https?|net|dgram|tls)['"]"""
    r"|IO::Socket|\bLWP\b|HTTP::Tiny|\bNet::|net/http|open-uri|\bfsockopen\b|\bcurl_init\b)"
)

# A guard, not containment: it keeps routine work from waiting on a person and
# catches the obvious slips, including the obvious ways to send what the agent
# has read off the box. A command can always be spelled so that no pattern sees
# it (a script it wrote, a variable, another language): what actually contains
# a bring-up is the account it runs as, the chips it leases, and `ask` for
# anything that matters. The stage also runs in a sandbox (sandbox_exec.py:
# TT Station's folder and keys unreadable, few paths writable); real network
# containment needs a separate account (deploy/contain-bringup.sh).
RISKY = [
    (re.compile(r"\btt-smi\b.*\s-r\b|\btt-smi\s+-r\b|\breset\b.*\bboard"), "resets chips"),
    (re.compile(r"\bsudo\b"), "uses sudo"),
    (re.compile(r"\brm\s+-[a-z]*r[a-z]*f?\s+(/|~|\$HOME)(\s|$)"), "deletes outside the repo"),
    (re.compile(r"""\brm\s+(-\S+\s+)*["']?(/\*|~|\$HOME\b|\$\{HOME\})"""), "deletes in your home folder"),
    (re.compile(r"\bpython3?\b.*\s-c\b.*\b(shutil\.rmtree|os\.(remove|unlink|rmdir|removedirs))\b"), "deletes files from Python"),
    (re.compile(r"""(>|\btee\s+(-a\s+)?)\s*["']?(~|\$HOME\b|\$\{HOME\})/\."""), "changes a settings file in your home folder"),
    (re.compile(r"\b(pip3?|uv|npm|pnpm|yarn|bun|apt|apt-get|conda|mamba|gem|cargo)\s+(install|add)\b|\bpip3?\s+download\b|\bgo\s+(get|install)\b"), "installs packages"),
    (re.compile(r"\b(docker|systemctl)\b"), "changes services or containers"),
    # The ways out of the sandbox as the same account: a new unit outside it, the user's bus.
    (re.compile(r"(?<![\w.-])(systemd-run|busctl|dbus-send|gdbus|loginctl|machinectl)(?![\w./=-])|/run/user/\d+/(bus|systemd)"),
     "starts something outside the bring-up's sandbox"),
    (re.compile(r"\b(shutdown|reboot|mkfs|dd\s+if=)\b"), "could damage the system"),
    (re.compile(r"\bcurl\b.*\|\s*(ba)?sh\b"), "runs a downloaded script"),
    # Talking to another machine. Not a word inside a path or a flag (~/.ssh, -nc).
    (re.compile(r"(?<![\w.-])(curl|wget|aria2c|nc|ncat|netcat|socat|telnet|ftp|tftp|sftp|lftp|ssh|scp|mosh|gh)(?![\w./=-])"), "uses the network"),
    (re.compile(r"(?<![\w.-])(npx|pnpx|bunx|uvx|pipx)(?![\w./=-])"), "downloads and runs a package"),
    (re.compile(r"/dev/(tcp|udp)/"), "opens a network connection"),
    (re.compile(r"\brsync\b[^|;&]*(\s([\w.-]+@)?[\w.-]+:|rsync://)"), "copies to or from another machine"),
    (re.compile(r"\b(hf|huggingface-cli)\s+(upload|repo|api)\b"), "sends files to Hugging Face"),
    # git that sends, or that fetches from an address it names (a URL can carry data out).
    (re.compile(GIT + r"(push|send-email|request-pull)\b"), "pushes to a remote"),
    (re.compile(GIT + r"(remote\s+(add|set-url)\b|config\b[^|;&]*(\burl\b|\.url\b|insteadof|pushurl))", re.I), "changes where git sends things"),
    (re.compile(GIT + r"(clone|fetch|pull|ls-remote|submodule|archive)\b[^|;&]*(://|\s[\w.-]+@[\w.-]+:)"), "fetches from an address it names"),
    # Code that writes the command it runs, where no pattern can read it.
    (re.compile(r"(^|[;&|(]\s*)eval\s|\|\s*(ba|z|da|k)?sh\b|\bbase64\s+(-d|--decode)\b|\bxxd\s+-r\b"), "runs code it generates"),
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
    # Inline code (python -c, node -e, a heredoc) that brings in networking.
    if INTERPRETER.search(command) and NET_CODE.search(command):
        return "runs inline code that uses the network"
    if cwd and not inside(cwd, repo):
        return f"runs outside the repo ({cwd})"
    return None


def post(path: str, body: dict) -> dict:
    url = os.environ["TT_STATION_URL"].rstrip("/") + path
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "X-Run-Secret": RUN_SECRET},
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

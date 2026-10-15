# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""TT Station's Claude Code runner for tt-metal's fast-models-fast stages.

The same stage prompts (`.agents/prompts/model_bringup_multigoal/NN-*.txt`),
skills (from ~/.claude/skills) and gate checks (`NN-*.check.sh`) as the Codex
runner, driven by `claude -p`. It writes the same `manifest.txt`, `STATUS.md`
and per-stage logs, and exits with the same codes, so TT Station shows both
engines' runs the same way:

  0 all stages done   3 a goal didn't complete   5 a turn failed
  6 a gate failed after a fix-up attempt          7 a gate is broken

    python claude_bringup.py --policy smart --claude-bin claude --log-dir DIR \
        --replace HF_MODEL=org/model --replace MODEL_DIR=models/autoports/x \
        [--start-index N | --resume-stage N] PROMPT...

Claude's permission questions go to TT Station through permission_mcp.py.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import pathlib
import re
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
# Only the permission server needs the run's secret (it reports approvals);
# Claude and every command it runs inherit this environment, so it leaves here.
RUN_SECRET = os.environ.pop("TT_STATION_RUN_SECRET", "")
SKILL_RE = re.compile(r"\$([A-Za-z0-9][A-Za-z0-9_-]*)")
DONE_RE = re.compile(r"^\s*GOAL:\s*(complete|blocked)\b\s*[—:-]?\s*(.*)$", re.I | re.M)


def ts() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def read_manifest(path: pathlib.Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if path.exists():
        for line in path.read_text().splitlines():
            if "=" in line and not line.startswith("- "):
                k, v = line.split("=", 1)
                out[k] = v
    return out


def append(path: pathlib.Path, lines: list[str]) -> None:
    with path.open("a") as f:
        for line in lines:
            f.write(line + "\n")


def status(log_dir: pathlib.Path, name: str, goal: str, check: str) -> None:
    p = log_dir / "STATUS.md"
    if not p.exists():
        p.write_text("| stage | goal | check |\n|---|---|---|\n")
    append(p, [f"| {name} | {goal} | {check} |"])


def objective(path: pathlib.Path, replacements: list[tuple[str, str]]) -> str:
    text = path.read_text()
    for old, new in replacements:
        text = text.replace(old, new)
    text = text.lstrip()
    if text.startswith("/goal"):
        text = text[len("/goal"):]
    # Codex's `$skill` references → Claude's skills, by name.
    skills = sorted(set(SKILL_RE.findall(text)))
    text = re.sub(r"`?\$([A-Za-z0-9][A-Za-z0-9_-]*)`?", lambda m: f"the `{m.group(1)}` skill", text).strip()
    if skills:
        text += "\n\nUse these skills (they're installed): " + ", ".join(skills) + "."
    if "vllm-integration" in skills:
        text += SERVE_RECIPE
    return text


# TT Station serves a port natively once its vLLM stage passes; this is how it knows how.
SERVE_RECIPE = (
    "\n\nAlso, once the vLLM server works: write <model dir>/doc/vllm_integration/serve.json as "
    '{"command": [the exact argv that launched the vLLM OpenAI server, starting with python], '
    '"env": {the environment variables it needed, such as MESH_DEVICE}}, so TT Station can serve '
    "this port with native vLLM."
)

FINISH = (
    "\n\nThis is one stage of an unattended bring-up run by TT Station. Work until the goal is "
    "fully met, verifying with real tests on the hardware. When you're done, end your final "
    "message with a line `GOAL: complete`. If you cannot finish (missing access, a hardware "
    "problem, a decision only a person can make), end with `GOAL: blocked — <why>`."
)


def take_note(path: pathlib.Path | None) -> str | None:
    """A person's note from TT Station, once: it's marked delivered when read."""
    if path is None or not path.is_file():
        return None
    text = path.read_text().strip()
    path.rename(path.with_name(f"note.delivered-{ts()}.txt"))
    return text or None


class Stage:
    def __init__(self, args, log_dir: pathlib.Path, n: int, prompt: pathlib.Path):
        self.args, self.log_dir, self.n, self.prompt = args, log_dir, n, prompt
        self.name = f"{n:02d}-{prompt.stem}"
        self.log = log_dir / f"{self.name}.jsonl"
        self.manifest = log_dir / "manifest.txt"

    def mcp_config(self) -> pathlib.Path:
        """The permission server's config, in a file only this account can read
        rather than on Claude's command line (which every account can list)."""
        a = self.args
        mcp = {"mcpServers": {"ttstation": {
            "command": sys.executable, "args": [str(HERE / "permission_mcp.py")],
            "env": {k: os.environ.get(k, "") for k in ("TT_STATION_URL", "TT_STATION_RUN")}
            | {"TT_STATION_RUN_SECRET": RUN_SECRET, "TT_STATION_POLICY": a.policy, "TT_STATION_REPO": a.repo},
        }}}
        mcp_file = self.log_dir / "mcp.json"
        fd = os.open(mcp_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(mcp, f)
        return mcp_file

    def run_claude(self, text: str, session: str | None) -> tuple[str | None, str, bool]:
        """One `claude -p` turn. Returns (session id, final text, failed)."""
        a = self.args
        argv = [a.claude_bin, "-p", text, "--output-format", "stream-json", "--verbose", "--add-dir", a.repo]
        if a.policy == "auto":
            argv += ["--dangerously-skip-permissions"]
        else:
            argv += ["--mcp-config", str(self.mcp_config()), "--permission-prompt-tool", "mcp__ttstation__approve"]
        if a.model:
            argv += ["--model", a.model]
        if session:
            argv += ["--resume", session]
        sid, final, failed = session, "", False
        with self.log.open("a") as log:
            log.write(json.dumps({"ts": ts(), "direction": "send", "claude": {"type": "prompt", "text": text[:4000]}}) + "\n")
            # A permission question can wait hours for a person.
            env = os.environ | {"MCP_TOOL_TIMEOUT": "86400000", "MCP_TIMEOUT": "60000"}
            proc = subprocess.Popen(argv, cwd=a.repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            for line in proc.stdout:  # type: ignore[union-attr]
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    continue
                log.write(json.dumps({"ts": ts(), "direction": "recv", "claude": obj}) + "\n")
                log.flush()
                if obj.get("session_id"):
                    if sid is None:
                        sid = obj["session_id"]
                        append(self.manifest, [f"stage_{self.n}_thread_id={sid}"])
                    sid = obj["session_id"]
                if obj.get("type") == "result":
                    final = obj.get("result") or ""
                    failed = bool(obj.get("is_error"))
            err = proc.stderr.read() if proc.stderr else ""
            code = proc.wait()
            if code != 0 and not final:
                failed = True
                log.write(json.dumps({"ts": ts(), "direction": "recv", "claude": {"type": "error", "error": err[-2000:]}}) + "\n")
        return sid, final, failed

    def goal(self, replacements, resume: bool) -> str:
        """Run the stage's goal; returns complete | blocked | turnFailed."""
        manifest = read_manifest(self.manifest)
        session = manifest.get(f"stage_{self.n}_thread_id") if resume else None
        text = objective(self.prompt, replacements) + FINISH
        if resume and session:
            text = "Continue this stage from where you stopped." + FINISH
            # Running again: the old outcome no longer stands.
            append(self.manifest, [f"stage_{self.n}_terminal_status=", f"stage_{self.n}_check="])
        note = take_note(self.args.note_file)
        if note:
            said = f"The person overseeing this bring-up in TT Station says:\n\n{note}\n\n"
            if resume and session:
                text = said + "Take that into account and continue this stage from where you stopped." + FINISH
            else:
                text = objective(self.prompt, replacements) + "\n\n" + said.strip() + FINISH
        for attempt in range(3):
            session, final, failed = self.run_claude(text, session)
            if failed:
                return "turnFailed"
            m = DONE_RE.findall(final or "")
            if m:
                verdict, why = m[-1]
                if verdict.lower() == "blocked":
                    append(self.manifest, [f"stage_{self.n}_blocked_reason={why.strip()[:300]}"])
                    return "blocked"
                return "complete"
            # No verdict: ask it to finish, or say why it can't.
            text = "You stopped without saying whether the goal is met. Keep going until it is." + FINISH
        return "blocked"

    def check(self, replacements) -> tuple[int, str]:
        script = self.prompt.with_name(self.prompt.stem + ".check.sh")
        if not script.exists() or self.args.no_checks:
            return 0, "none"
        env = os.environ | {k: v for k, v in replacements}
        out = subprocess.run(["bash", str(script)], cwd=self.args.repo, env=env, capture_output=True, text=True)
        (self.log_dir / f"{self.name}.check.log").write_text(out.stdout + out.stderr)
        return out.returncode, (out.stdout + out.stderr)[-3000:]


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("prompts", nargs="+", type=pathlib.Path)
    p.add_argument("--policy", default="smart")
    p.add_argument("--claude-bin", default="claude")
    p.add_argument("--log-dir", type=pathlib.Path, required=True)
    p.add_argument("--replace", action="append", default=[])
    p.add_argument("--start-index", type=int, default=1)
    p.add_argument("--resume-stage", type=int)
    p.add_argument("--model")
    p.add_argument("--no-checks", action="store_true")
    p.add_argument("--note-file", type=pathlib.Path)
    a = p.parse_args()
    a.repo = os.getcwd()
    a.log_dir.mkdir(parents=True, exist_ok=True)
    replacements = [tuple(r.split("=", 1)) for r in a.replace if "=" in r]
    manifest = a.log_dir / "manifest.txt"
    if a.resume_stage is None:
        append(manifest, [f"started_at_utc={ts()}", f"repo={a.repo}", "runner=claude", f"approval_policy={a.policy}",
                          *[f"{k.lower()}={v}" for k, v in replacements if k in ("HF_MODEL", "MODEL_DIR")]])
    for i, prompt in enumerate(a.prompts, start=a.start_index):
        if a.resume_stage is not None and i < a.resume_stage:
            continue
        stage = Stage(a, a.log_dir, i, prompt)
        print(f"\n== Stage {i}: {prompt.name} ==", file=sys.stderr, flush=True)
        goal = stage.goal(replacements, resume=a.resume_stage == i)
        append(manifest, [f"stage_{i}_terminal_status={goal}"])
        if goal != "complete":
            status(a.log_dir, stage.name, goal, "skipped")
            return 5 if goal == "turnFailed" else 3
        code, output = stage.check(replacements)
        if code in (1, 2):
            # One fix-up attempt, in the same session, with the gate's output.
            sid = read_manifest(manifest).get(f"stage_{i}_thread_id")
            stage.run_claude(f"The stage's check failed (exit {code}):\n\n{output}\n\nFix what it found." + FINISH, sid)
            code, output = stage.check(replacements)
        elif code not in (0,):
            code, output = stage.check(replacements)  # a transient? one retry
        label = {0: "pass", 1: "advisory-fail", 2: "critical-fail"}.get(code, f"check-error(exit{code})")
        if code == 0 and not stage.prompt.with_name(prompt.stem + ".check.sh").exists():
            label = "none"
        append(manifest, [f"stage_{i}_check={label}"])
        status(a.log_dir, stage.name, goal, label)
        if code == 2:
            return 6
        if code not in (0, 1):
            return 7
    print(f"\nAll {len(a.prompts)} goals completed.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

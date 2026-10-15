# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

"""Does vLLM hold gpt-oss's tool-call arguments to the schema? See the .md beside this.

Sends Claude Code-like requests to an OpenAI-compatible server under four
tool_choice modes and reports, per mode, how often the arguments fit the tool's
schema and how long a request took. Standard library only.

    python3 docs/notes/tool-call-constrained-decoding.py [--base http://localhost:8003/v1] [--reps 3]
"""

import argparse
import concurrent.futures
import json
import random
import statistics
import time
import urllib.request


def obj(props, required):
    return {"type": "object", "properties": props, "required": required, "additionalProperties": False}


# Claude Code's file and shell tools, trimmed to the parts that matter here.
TOOLS = {
    "Bash": obj({"command": {"type": "string"}, "description": {"type": "string"}, "timeout": {"type": "number"}}, ["command"]),
    "Read": obj({"file_path": {"type": "string"}, "offset": {"type": "number"}, "limit": {"type": "number"}}, ["file_path"]),
    "Edit": obj({"file_path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"},
                 "replace_all": {"type": "boolean"}}, ["file_path", "old_string", "new_string"]),
    "Write": obj({"file_path": {"type": "string"}, "content": {"type": "string"}}, ["file_path", "content"]),
    "Grep": obj({"pattern": {"type": "string"}, "path": {"type": "string"}, "glob": {"type": "string"},
                 "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                 "-i": {"type": "boolean"}, "head_limit": {"type": "number"}}, ["pattern"]),
    "Glob": obj({"pattern": {"type": "string"}, "path": {"type": "string"}}, ["pattern"]),
}
DESCRIPTIONS = {
    "Bash": "Run a shell command.",
    "Read": "Read a file. offset is the line to start at, limit how many lines to read.",
    "Edit": "Replace old_string with new_string in a file.",
    "Write": "Write a file, replacing it if it exists.",
    "Grep": "Search file contents with a regex (ripgrep).",
    "Glob": "Find files by glob pattern.",
}
SYSTEM = ("You are Claude Code, a coding agent working in the repository at /home/dev/tt-station. "
          "Use the tools to act; answer briefly.")

# (tool the task needs, prompt). The second group asks for arguments the schema
# doesn't allow, the way a model drifts: a schema-bound decoder can't comply.
ORDINARY = [
    ("Read", "Show me lines 470 to 520 of desktop/src/pages/Code.tsx."),
    ("Grep", "Find every place parse_args is used under crates/, case-insensitively, with matching lines."),
    ("Glob", "List all the Rust test files under crates/tt-station-agentd/tests."),
    ("Bash", "Run the agentd unit tests: cargo test -p tt-station-agentd --lib, with a 10 minute timeout."),
    ("Edit", "In crates/tt-station-agentd/src/lib.rs, replace every 'pub mod tori;' with 'pub mod tori; // voice'."),
    ("Write", "Create notes/todo.md containing a three-item markdown checklist for the release."),
]
TEMPTING = [
    ("Read", "Call Read on src/main.rs with line_start=100 and line_end=150 (use exactly those parameter names)."),
    ("Read", "Call Read on src/main.rs with offset given as the string \"40\" and limit as the string \"20\"."),
    ("Grep", "Call Grep for 'TODO' with output_mode set to \"lines\" and case_sensitive=false."),
]

TYPES = {"string": str, "boolean": bool, "object": dict, "array": list}


def problems(name, args_text):
    """What's wrong with a call, by the translator's rules (anthropic.rs ToolCheck), without its repairs."""
    if name not in TOOLS:
        return [f"unknown tool {name}"]
    try:
        args = json.loads(args_text)
    except (TypeError, ValueError):
        return ["arguments aren't JSON"]
    if not isinstance(args, dict):
        return ["arguments aren't an object"]
    schema, out = TOOLS[name], []
    out += [f"missing {k}" for k in schema["required"] if k not in args]
    for k, v in args.items():
        prop = schema["properties"].get(k)
        if prop is None:
            out.append(f"unexpected {k}")
            continue
        t = prop["type"]
        ok = (isinstance(v, (int, float)) and not isinstance(v, bool)) if t == "number" else isinstance(v, TYPES[t])
        if not ok:
            out.append(f"{k} not {t}")
        elif "enum" in prop and v not in prop["enum"]:
            out.append(f"{k} not in enum")
    return out


def tools_payload(strict):
    tools = []
    for name, params in TOOLS.items():
        fn = {"name": name, "description": DESCRIPTIONS[name], "parameters": params}
        if strict:
            fn["strict"] = True
        tools.append({"type": "function", "function": fn})
    return tools


def one(base, model, mode, tool, prompt):
    body = {
        "model": model,
        "messages": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": prompt}],
        "tools": tools_payload(mode == "auto+strict"),
        "max_tokens": 2048,
        "temperature": 1.0,
        # What the translator sends when Claude Code hasn't asked for thinking.
        "chat_template_kwargs": {"enable_thinking": False},
        "tool_choice": {"auto": "auto", "auto+strict": "auto", "required": "required",
                        "named": {"type": "function", "function": {"name": tool}}}[mode],
    }
    req = urllib.request.Request(f"{base}/chat/completions", json.dumps(body).encode(), {"content-type": "application/json"})
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            resp = json.load(r)
    except Exception as e:  # noqa: BLE001 -- an HTTP error is a result here
        return {"mode": mode, "tool": tool, "prompt": prompt, "error": str(e)[:200], "seconds": time.monotonic() - t0}
    secs = time.monotonic() - t0
    msg = resp["choices"][0]["message"]
    calls = msg.get("tool_calls") or []
    return {
        "mode": mode, "tool": tool, "prompt": prompt, "seconds": secs,
        "finish": resp["choices"][0]["finish_reason"],
        "completion_tokens": resp.get("usage", {}).get("completion_tokens"),
        "text": (msg.get("content") or "")[:300],
        "calls": [{"name": c["function"]["name"], "arguments": c["function"]["arguments"],
                   "problems": problems(c["function"]["name"], c["function"]["arguments"])} for c in calls],
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://localhost:8003/v1")
    ap.add_argument("--model", default="openai/gpt-oss-20b")
    ap.add_argument("--reps", type=int, default=2)
    ap.add_argument("--concurrency", type=int, default=4)
    ap.add_argument("--out", default="tool-call-results.json")
    ap.add_argument("--seed", type=int, default=7)
    a = ap.parse_args()
    modes = ["auto", "auto+strict", "required", "named"]
    jobs = [(m, t, p) for m in modes for t, p in ORDINARY * a.reps + TEMPTING * a.reps]
    random.Random(a.seed).shuffle(jobs)  # interleave modes so load affects them alike
    with concurrent.futures.ThreadPoolExecutor(min(a.concurrency, 4)) as pool:
        results = list(pool.map(lambda j: one(a.base, a.model, *j), jobs))
    with open(a.out, "w") as f:
        json.dump(results, f, indent=1)

    tempting = {p for _, p in TEMPTING}
    print(f"{'mode':12} {'set':9} {'n':>3} {'call':>5} {'fits':>5} {'right tool':>10} {'p50 s':>6} {'p90 s':>6} {'errors':>6}")
    for m in modes:
        for label, pick in (("ordinary", lambda r: r["prompt"] not in tempting), ("tempting", lambda r: r["prompt"] in tempting)):
            rs = [r for r in results if r["mode"] == m and pick(r)]
            ok = [r for r in rs if "error" not in r]
            called = [r for r in ok if r["calls"]]
            fits = [r for r in called if all(not c["problems"] for c in r["calls"])]
            right = [r for r in called if r["calls"][0]["name"] == r["tool"]]
            secs = sorted(r["seconds"] for r in rs)
            p90 = secs[min(len(secs) - 1, int(len(secs) * 0.9))] if secs else 0
            print(f"{m:12} {label:9} {len(rs):3} {len(called):5} {len(fits):5} {len(right):10} "
                  f"{statistics.median(secs) if secs else 0:6.1f} {p90:6.1f} {len(rs) - len(ok):6}")


if __name__ == "__main__":
    main()

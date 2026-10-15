# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""tt-station's bridge into a tt-orchard checkout. Prints one JSON object.

    python3 -c <this file> CHECKOUT info
    python3 -c <this file> CHECKOUT preflight MODEL [--refuse-credentials] [--base BASE]
    python3 -c <this file> CHECKOUT lab-check
    python3 -c <this file> CHECKOUT story RUN_DIR

`story` tells a run the way tt-orchard does: its status facts (`orchard.status.collect`, the same as
`tt-orchard status`), the narration `tt-orchard watch` prints (`orchard.narrate`: one line per thing
that starts, ends or is tried, by its orchard role), what the run is doing right now, the outcome
class stage 0 chose and the model it grafts onto, and for a blocked run what was tried and how to
unblock it. It reads the run directory and writes nothing.

`info` reports the setup checks (`orchard.setup_machine.plan`, which only checks) and the bringup config.
`preflight` runs tt-orchard's own preflight (`orchard.preflight.run_preflight`), the same checks
`tt-orchard bringup` runs before it starts. `lab-check` runs `tt-orchard lab setup --check`'s plan
(`orchard.lab_setup.plan`) for the `[lab]` box. None of them changes either machine.
"""
import json
import os
import sys
from pathlib import Path


def load_config(checkout):
    from orchard import bringup_config, cli

    path = cli.find_config(None, os.environ, checkout, Path.home())
    return bringup_config.load(path), path


def info(checkout):
    from orchard import __version__

    out = {"version": __version__}
    try:
        from orchard import setup_machine as sm

        steps = sm.plan(sm.default_env(checkout), sm.Options())
        out["setup"] = [{"name": s.name, "status": s.status, "detail": s.detail} for s in steps]
    except Exception as exc:  # a broken check is a result, not a crash
        out["setup_error"] = f"{type(exc).__name__}: {exc}"
    try:
        cfg, path = load_config(checkout)
        out["config"] = {
            "path": str(path),
            "runs_root": str(cfg.runs_root),
            "coder": {"target": cfg.coder.target, "port": cfg.coder.port, "chips": cfg.coder.chips},
            "required_chips": cfg.required_chips,
            "package_format": cfg.package_format,
            "tt_model_root": str(cfg.tt_model_root) if getattr(cfg, "tt_model_root", None) else None,
            "lab": _lab(cfg),
        }
        try:
            from orchard import preflight as pf

            out["config"]["hf_home"] = str(pf.hf_home_for(cfg))
        except Exception:
            pass
    except Exception as exc:
        out["config_error"] = str(exc)
    return out


def _lab(cfg):
    lab = getattr(cfg, "lab", None)
    if lab is None:
        return None
    return {"host": lab.host, "root": str(lab.root), "gozer": lab.gozer, "path": list(lab.path),
            "python": lab.python, "test_python": lab.test_python}


def lab_check(checkout):
    from orchard import lab_setup

    cfg, _ = load_config(checkout)
    if getattr(cfg, "lab", None) is None:
        return {"error": "bringup.toml has no [lab] table"}
    ref = cfg.reference_python or (Path(cfg.lab.root) / "venvs" / "reference" / "bin" / "python")
    sources = [Path(os.environ.get("HF_HOME") or Path.home() / ".cache" / "huggingface")]
    run = lab_setup._run
    steps = lab_setup.plan(cfg.lab, run=run, reference_python=ref, local_fw=lab_setup.local_firmware(),
                           local_sfpi=lab_setup.local_sfpi(run), models=[], model_sources=sources,
                           tt_model_root=cfg.tt_model_root, hf_home=cfg.hf_home)
    out = [{"name": st.name, "status": st.status, "detail": st.detail, "fixable": bool(st.actions)} for st in steps]
    return {"host": cfg.lab.host, "steps": out,
            "ready": not any(st["status"] in ("fail", "todo") for st in out)}


def preflight(checkout, model, accept, base=None):
    from orchard import bringup_config
    from orchard import preflight as pf

    cfg, _ = load_config(checkout)
    run_dir = bringup_config.run_dir(cfg, model)
    resuming = (run_dir / "ledger.jsonl").exists()
    signals = pf.default_signals(cfg)
    try:
        checks = pf.run_preflight(cfg, model, accept_credentials=accept, signals=signals, resuming=resuming,
                                  base_override=base)
    except TypeError:  # a tt-orchard without --base
        checks = pf.run_preflight(cfg, model, accept_credentials=accept, signals=signals, resuming=resuming)
    hub = checks[0].data if checks else None
    try:
        local = signals.local_snapshot(model) is not None
    except Exception:
        local = False
    return {
        "model": model,
        "run_name": run_dir.name,
        "run_dir": str(run_dir),
        "resuming": resuming,
        "local": local,
        "download_gb": 0.0 if local or hub is None else round(hub.total_bytes / 1e9, 1),
        "license": getattr(hub, "license", None),
        "checks": [{"name": c.name, "status": c.status, "detail": c.detail, "reason": c.reason,
                    # The base check lists the bundles a run could build on.
                    "candidates": (c.data or {}).get("candidates") if isinstance(getattr(c, "data", None), dict) else None}
                   for c in checks],
        "lab": _lab(cfg),
        "blocked": bool(pf.blocked(checks)),
    }


def _activity(nar, now):
    act = getattr(nar, "_activity", None)
    if not act:
        return None
    text, since, stage, hardware = act
    return {"text": text, "since": since, "stage": stage, "hardware": bool(hardware)}


def story(checkout, run_dir, limit=80):
    import time

    from orchard import lexicon, narrate, status
    from orchard.ledger import read_entries
    from orchard.ui import Style

    run = Path(run_dir)
    now = time.time()
    out = {"facts": None, "narration": [], "agent": [], "activity": None, "graft": None,
           "tried": None, "unblock": None}
    try:
        out["facts"] = status.collect(run, now=now)
    except Exception as exc:
        out["facts_error"] = f"{type(exc).__name__}: {exc}"
    try:
        entries = read_entries(run / "ledger.jsonl")
    except Exception:
        entries = []
    # The narrator's own bookkeeping: who works each stage (grafter, head grower, seasonal hand) and
    # what the run is waiting on right now.
    nar = narrate.Narrator(run, Style(), out=_Null(), clock=lambda: now)
    lines = []
    from datetime import datetime, timezone

    def at(ts):
        try:
            return datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc).timestamp()
        except (TypeError, ValueError):
            return now

    for e in entries:
        try:
            nar.clock = (lambda t: (lambda: t))(at(e.get("ts")))  # an activity starts when its entry was written
            nar._track(e)
            for ln in narrate.lines_for(e):
                lines.append({"ts": e.get("ts"), "actor": ln.actor, "text": ln.text, "bad": ln.bad,
                              "stage": e.get("stage"), "event": e["event"]})
            if e["event"] == "evidence" and e["data"].get("what") == "hardware test" \
                    and e["data"].get("returncode") != 0 and e["stage"] is not None:
                for t in narrate._output_tail(run, e["stage"], 3):
                    lines.append({"ts": e.get("ts"), "actor": "orchardist", "text": "last output: " + narrate._clip(t, 150),
                                  "bad": True, "stage": e.get("stage"), "event": "output"})
        except Exception:
            continue
    out["narration"] = lines[-limit:]
    out["activity"] = _activity(nar, now)
    act = out["activity"]
    if act and act["hardware"] and act["stage"] is not None:
        try:
            act["last_output"] = narrate._last_output_line(run, act["stage"])
        except Exception:
            pass
    # The agent's latest turns, in the stage it works on now (its newest log).
    try:
        logs = sorted((run / "stages").glob("*/log/*.jsonl"), key=lambda p: p.stat().st_mtime)
        if logs:
            f = logs[-1]
            stage = narrate._stage_of(f)
            actor = nar._stage_actor.get(stage, "grafter")
            tn = narrate.TurnNarrator()
            got = []
            for raw in f.read_text(encoding="utf-8", errors="replace").splitlines()[-40:]:
                try:
                    rec = json.loads(raw)
                except ValueError:
                    continue
                if isinstance(rec, dict):
                    got += [{"actor": ln.actor, "text": ln.text, "bad": ln.bad, "stage": stage}
                            for ln in tn.lines_for(rec, actor=actor)]
            out["agent"] = got[-14:]
    except Exception:
        pass
    # The graft: stage 0's delta.json names the class, the path and the rootstock (the nearest model).
    for d in sorted(run.glob("stages/0*/delta.json"), key=lambda p: p.stat().st_mtime, reverse=True)[:1]:
        try:
            delta = json.loads(d.read_text(encoding="utf-8"))
            out["graft"] = {
                "class": delta.get("class"), "path": delta.get("path"), "scion": delta.get("model"),
                "rootstock": delta.get("nearest_model"), "reasons": delta.get("path_reasons") or [],
                "differences": [{"area": x.get("area"), "finding": x.get("finding")} for x in delta.get("differences") or []][:12],
                "hazards": [{"area": x.get("area"), "finding": x.get("finding")} for x in delta.get("hazards") or []][:8],
                "sidecars": len(delta.get("sidecars") or []),
            }
        except Exception:
            pass
    f = out["facts"] or {}
    if f.get("state") == "blocked" or f.get("blocked"):
        b = f.get("blocked") or {}
        st = (f.get("stage") or {}).get("current")
        if st is None:
            st = max((r["stage"] for r in f.get("stages") or []), default=None)
        try:
            out["tried"] = narrate.what_was_tried(run, stage=st) if st is not None else None
        except Exception:
            pass
        try:
            out["unblock"] = narrate.how_to_unblock(b.get("code") or "unclassified", f.get("model"))
        except Exception:
            pass
    out["lexicon"] = {
        "states": {k: {"emoji": v.emoji, "phrase": v.phrase} for k, v in lexicon.STATES.items()},
        "stages": {str(k): {"emoji": v.emoji, "name": v.name} for k, v in lexicon.STAGES.items()},
        "actors": lexicon.ACTORS,
        "icons": getattr(narrate, "ACTOR_ICON", {}),
    }
    return out


class _Null:
    def write(self, *_):
        pass

    def flush(self):
        pass


def main():
    checkout = Path(sys.argv[1])
    sys.path.insert(0, str(checkout))
    cmd = sys.argv[2]
    try:
        if cmd == "info":
            out = info(checkout)
        elif cmd == "preflight":
            rest = sys.argv[4:]
            base = rest[rest.index("--base") + 1] if "--base" in rest else None
            # Credentials visible to agent shells are accepted by default now (tt-orchard's own default).
            out = preflight(checkout, sys.argv[3], "--refuse-credentials" not in rest, base)
        elif cmd == "lab-check":
            out = lab_check(checkout)
        elif cmd == "story":
            out = story(checkout, sys.argv[3])
        else:
            out = {"error": f"unknown command {cmd}"}
    except Exception as exc:
        out = {"error": f"{type(exc).__name__}: {exc}"}
    print(json.dumps(out))


main()

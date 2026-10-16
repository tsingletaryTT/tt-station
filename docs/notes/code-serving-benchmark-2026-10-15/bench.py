#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: © 2026 Tenstorrent AI ULC
"""Code-serving benchmark (S1, S2, S3, S7 of docs/design/code-serving.md §5).

Standard library only. Talks to an OpenAI-compatible vLLM server.

  bench.py wait-ready --t0 <epoch> --out load.json     # S7: first 1-token completion
  bench.py suite --config A --out results-A.json       # warm-up, then S1, S2, S3 x3
"""
import argparse
import http.client
import json
import random
import statistics
import sys
import threading
import time
import uuid

HOST, PORT = "localhost", 8003
MODEL = "openai/gpt-oss-20b"
REQS = {"n": 0}  # our own request count, to compare with the server's access log
REQS_LOCK = threading.Lock()

WORDS = (
    "system kernel buffer latency thread compile module vector tensor matrix cache "
    "socket packet router queue memory pointer struct trait async await future mutex "
    "server client request response header payload schema index table column query "
    "branch commit merge rebase patch diff review test assert fixture mock stub "
    "device chip board mesh fabric link driver firmware reset power clock voltage "
    "river mountain forest ocean garden window door table chair lamp paper pencil "
    "quickly slowly carefully gently loudly quietly brightly darkly warmly coldly"
).split()


def count(n=1):
    with REQS_LOCK:
        REQS["n"] += n


def post_stream(path, body, timeout=900, on_chunk=None, stop=None):
    """POST a streaming request. Returns dict with t0, t_first, t_last, chunk times, usage."""
    body = dict(body, stream=True, stream_options={"include_usage": True})
    conn = http.client.HTTPConnection(HOST, PORT, timeout=timeout)
    t0 = time.monotonic()
    count()
    conn.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
    resp = conn.getresponse()
    if resp.status != 200:
        raise RuntimeError(f"HTTP {resp.status}: {resp.read()[:300]!r}")
    times, usage, text = [], None, []
    buf = b""
    while True:
        if stop is not None and stop.is_set():
            break
        line = resp.fp.readline()
        if not line:
            break
        line = line.strip()
        if not line.startswith(b"data:"):
            continue
        data = line[5:].strip()
        if data == b"[DONE]":
            break
        ev = json.loads(data)
        if ev.get("usage"):
            usage = ev["usage"]
        for ch in ev.get("choices") or []:
            piece = ch.get("text")
            if piece is None:
                d = ch.get("delta") or {}
                piece = (d.get("content") or "") + (d.get("reasoning_content") or d.get("reasoning") or "")
            if piece is not None and piece != "" or ch.get("finish_reason"):
                now = time.monotonic()
                times.append(now)
                text.append(piece or "")
                if on_chunk:
                    on_chunk(now)
    conn.close()
    return {
        "t0": t0,
        "t_first": times[0] if times else None,
        "t_last": times[-1] if times else None,
        "times": times,
        "usage": usage,
        "ttft": (times[0] - t0) if times else None,
        "text": "".join(text),
    }


def post_json(path, body, timeout=900):
    conn = http.client.HTTPConnection(HOST, PORT, timeout=timeout)
    t0 = time.monotonic()
    count()
    conn.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
    resp = conn.getresponse()
    raw = resp.read()
    conn.close()
    if resp.status != 200:
        raise RuntimeError(f"HTTP {resp.status}: {raw[:300]!r}")
    return time.monotonic() - t0, json.loads(raw)


def completion(prompt, max_tokens, ignore_eos=True, **kw):
    return {
        "model": MODEL,
        "prompt": prompt,
        "max_tokens": max_tokens,
        "temperature": 0,
        "ignore_eos": ignore_eos,
        **kw,
    }


def filler(n_words, seed=0):
    rng = random.Random(seed)
    out = []
    for i in range(n_words):
        out.append(rng.choice(WORDS))
        if i % 17 == 16:
            out.append(".\n")
    return " ".join(out)


def stats(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return None
    xs = sorted(xs)
    med = statistics.median(xs)
    if len(xs) >= 2:
        q = statistics.quantiles(xs, n=4, method="inclusive")
        iqr = q[2] - q[0]
    else:
        iqr = 0.0
    return {"median": round(med, 4), "iqr": round(iqr, 4), "n": len(xs), "values": [round(x, 4) for x in xs]}


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


# ---------------------------------------------------------------- S7
def wait_ready(t0_epoch, timeout_s, out):
    start = time.time()
    last_err = None
    while time.time() - start < timeout_s:
        try:
            dt, r = post_json("/v1/completions", completion("Hello", 1), timeout=30)
            if r.get("choices"):
                t = time.time()
                res = {"t0_epoch": t0_epoch, "ready_epoch": t, "load_s": round(t - t0_epoch, 1)}
                json.dump(res, open(out, "w"), indent=1)
                print(json.dumps(res))
                return 0
        except Exception as e:  # noqa: BLE001 - polling until it answers
            last_err = str(e)[:200]
        # The container died during boot: stop polling (tt-model leaves it Exited).
        if time.time() - start > 60:
            import subprocess
            up = subprocess.run(["docker", "ps", "-q", "--filter", "label=org.tenstorrent.tt-model=gpt-oss-20b-p150"],
                                capture_output=True, text=True).stdout.strip()
            if not up:
                json.dump({"t0_epoch": t0_epoch, "error": "container exited during boot", "last_err": last_err},
                          open(out, "w"))
                print("container exited during boot")
                return 1
        time.sleep(2)
    json.dump({"t0_epoch": t0_epoch, "error": "timeout", "last_err": last_err}, open(out, "w"))
    print("timeout", last_err)
    return 1


# ---------------------------------------------------------------- S1
def s1_single(rep):
    prompt = f"[run {uuid.uuid4().hex[:8]}] Write a long, detailed story about a lighthouse keeper."
    r = post_stream("/v1/completions", completion(prompt, 256))
    n = (r["usage"] or {}).get("completion_tokens") or len(r["times"])
    dec = (n - 1) / (r["t_last"] - r["t_first"]) if r["t_last"] > r["t_first"] else None
    return {"ttft_s": r["ttft"], "decode_tok_s": dec, "completion_tokens": n,
            "prompt_tokens": (r["usage"] or {}).get("prompt_tokens")}


def s1_batch(rep, n=8, max_tokens=128):
    res = [None] * n

    def one(i):
        prompt = f"[run {uuid.uuid4().hex[:8]} user {i}] Explain, step by step, how a compiler works."
        dt, r = post_json("/v1/completions", completion(prompt, max_tokens))
        res[i] = r["usage"]["completion_tokens"]

    th = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    t0 = time.monotonic()
    for t in th:
        t.start()
    for t in th:
        t.join()
    wall = time.monotonic() - t0
    return {"aggregate_tok_s": sum(res) / wall, "wall_s": wall, "tokens": sum(res)}


# ---------------------------------------------------------------- S2
class Sizer:
    """Words-per-token ratio for the filler text, calibrated against the server."""

    def __init__(self):
        words = 4000
        _, r = post_json("/v1/completions", completion("x " + filler(words, 7), 1))
        self.tok_per_word = r["usage"]["prompt_tokens"] / words

    def prompt(self, tokens, nonce, seed=1):
        words = int(tokens / self.tok_per_word)
        return f"[session {nonce}]\n" + filler(words, seed) + "\nSummarize the text above in one word:"


def ttft_1tok(prompt):
    r = post_stream("/v1/completions", completion(prompt, 1))
    return r["ttft"], (r["usage"] or {}).get("prompt_tokens")


def s2(sizer, sizes, reps):
    out = {}
    for size in sizes:
        rows = []
        for rep in range(reps):
            p = sizer.prompt(size, uuid.uuid4().hex, seed=size + rep)
            cold, pt = ttft_1tok(p)
            hit1, _ = ttft_1tok(p)
            hit2, _ = ttft_1tok(p)
            rows.append({"prompt_tokens": pt, "cold_s": cold, "hit1_s": hit1, "hit2_s": hit2,
                         "cold_tok_s": pt / cold if cold else None})
            print(f"  S2 {size} rep{rep}: tokens={pt} cold={cold:.2f}s hit1={hit1:.2f}s hit2={hit2:.2f}s", flush=True)
        out[str(size)] = {
            "rows": rows,
            "prompt_tokens": stats([r["prompt_tokens"] for r in rows]),
            "cold_s": stats([r["cold_s"] for r in rows]),
            "cold_tok_s": stats([r["cold_tok_s"] for r in rows]),
            "hit1_s": stats([r["hit1_s"] for r in rows]),
            "hit2_s": stats([r["hit2_s"] for r in rows]),
        }
    return out


# ---------------------------------------------------------------- S3
def s3(sizer, reps, size=65536):
    out = []
    for rep in range(reps):
        stop = threading.Event()
        chat = {}
        cold = {}
        shorts = []

        def chat_stream():
            try:
                chat["r"] = post_stream(
                    "/v1/completions",
                    completion(f"[chat {uuid.uuid4().hex[:8]}] Tell me a very long story about the sea.", 6000),
                    stop=stop,
                )
            except Exception as e:  # noqa: BLE001
                chat["err"] = str(e)

        def cold_read():
            p = sizer.prompt(size, uuid.uuid4().hex, seed=99 + rep)
            cold["t0"] = time.monotonic()
            t, pt = ttft_1tok(p)
            cold["t1"] = time.monotonic()
            cold["ttft"], cold["tokens"] = t, pt

        tc = threading.Thread(target=chat_stream)
        tc.start()
        time.sleep(4)  # chat stream in steady decode
        tb = threading.Thread(target=cold_read)
        tb.start()
        time.sleep(0.2)
        while tb.is_alive():
            r = post_stream("/v1/completions",
                            completion(f"[q {uuid.uuid4().hex[:8]}] What is two plus two? Answer:", 1))
            shorts.append(r["ttft"])
            time.sleep(0.25)
        tb.join()
        time.sleep(2)
        stop.set()
        tc.join(timeout=60)
        times = chat.get("r", {}).get("times") or []
        win = [t for t in times if cold["t0"] - 0.5 <= t <= cold["t1"] + 2]
        # gap spanning into the window: include the last token before it
        before = [t for t in times if t < cold["t0"] - 0.5]
        if before:
            win = [before[-1]] + win
        gaps = [b - a for a, b in zip(win, win[1:])]
        pre = [t for t in times if t < cold["t0"]]
        pre_gaps = [b - a for a, b in zip(pre, pre[1:])]
        row = {
            "cold_tokens": cold.get("tokens"),
            "cold_ttft_s": cold.get("ttft"),
            "chat_longest_gap_s": max(gaps) if gaps else None,
            "chat_tokens_during_cold": len(win),
            "chat_gap_p50_before_s": statistics.median(pre_gaps) if pre_gaps else None,
            "short_ttfts_s": shorts,
            "short_ttft_p95_s": pct(shorts, 0.95),
            "chat_error": chat.get("err"),
        }
        print(f"  S3 rep{rep}: cold={row['cold_ttft_s']:.2f}s longest gap={row['chat_longest_gap_s']}s "
              f"short p95={row['short_ttft_p95_s']} n={len(shorts)}", flush=True)
        out.append(row)
    pooled = [x for r in out for x in r["short_ttfts_s"]]
    return {
        "rows": out,
        "longest_gap_s": stats([r["chat_longest_gap_s"] for r in out]),
        "short_ttft_p95_s": stats([r["short_ttft_p95_s"] for r in out]),
        "short_ttft_p95_pooled_s": pct(pooled, 0.95),
        "cold_ttft_s": stats([r["cold_ttft_s"] for r in out]),
    }


def suite(config, out, reps, sizes):
    res = {"config": config, "started": time.time()}
    print("warm-up", flush=True)
    s1_single(-1)
    s1_batch(-1)
    sizer = Sizer()
    res["tok_per_word"] = sizer.tok_per_word
    ttft_1tok(sizer.prompt(8000, uuid.uuid4().hex))  # warm the prefill path once
    print("S1", flush=True)
    singles = [s1_single(i) for i in range(reps)]
    batches = [s1_batch(i) for i in range(reps)]
    short_ttft = [ttft_1tok(f"[t {uuid.uuid4().hex[:8]}] Hi")[0] for _ in range(reps)]
    res["S1"] = {
        "singles": singles,
        "batches": batches,
        "ttft_s": stats([s["ttft_s"] for s in singles]),
        "ttft_short_1tok_s": stats(short_ttft),
        "decode_tok_s": stats([s["decode_tok_s"] for s in singles]),
        "batch8x128_tok_s": stats([b["aggregate_tok_s"] for b in batches]),
    }
    print(json.dumps({k: v for k, v in res["S1"].items() if k not in ("singles", "batches")}), flush=True)
    json.dump(res, open(out, "w"), indent=1)
    print("S2", flush=True)
    res["S2"] = s2(sizer, sizes, reps)
    json.dump(res, open(out, "w"), indent=1)
    print("S3", flush=True)
    res["S3"] = s3(sizer, reps)
    res["finished"] = time.time()
    res["our_requests"] = REQS["n"]
    json.dump(res, open(out, "w"), indent=1)
    print("done", out, flush=True)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    w = sub.add_parser("wait-ready")
    w.add_argument("--t0", type=float, required=True)
    w.add_argument("--timeout", type=float, default=5400)
    w.add_argument("--out", required=True)
    s = sub.add_parser("suite")
    s.add_argument("--config", required=True)
    s.add_argument("--out", required=True)
    s.add_argument("--reps", type=int, default=3)
    s.add_argument("--sizes", default="5000,16000,32000,64000,96000")
    a = ap.parse_args()
    if a.cmd == "wait-ready":
        sys.exit(wait_ready(a.t0, a.timeout, a.out))
    suite(a.config, a.out, a.reps, [int(x) for x in a.sizes.split(",")])


if __name__ == "__main__":
    main()

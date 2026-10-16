#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: © 2026 Tenstorrent AI ULC
"""Tables (markdown) from results-*.json, load-*.json and s5-*.json in this folder."""
import json
import os
import sys

D = os.path.dirname(os.path.abspath(__file__))
CONFIGS = [("A", "p150x2-throughput"), ("A′", "p150x2"), ("B", "p150x4"), ("B′", "p150x4-throughput")]
FILE = {"A": "A", "A′": "A2", "B": "B", "B′": "B2"}


def load(name):
    p = os.path.join(D, name)
    return json.load(open(p)) if os.path.exists(p) else None


def m(s, f="{:.2f}"):
    if not s:
        return "–"
    return (f + " ({})").format(s["median"], (f.format(s["iqr"])))


def s5_valid(s5):
    """Within-one-retry validity per size from the agent_reliability report."""
    if not s5:
        return {}
    out = {}
    sizes = s5.get("sizes") or s5.get("results") or []
    for r in sizes if isinstance(sizes, list) else []:
        n = r.get("tokens") or r.get("length") or r.get("size")
        v = r.get("valid_within_one_retry") or r.get("within_one_retry")
        t = r.get("trials") or r.get("n")
        out[n] = (v, t)
    return out


def main():
    rows = []
    for name, prof in CONFIGS:
        f = FILE[name]
        r, ld = load(f"results-{f}.json"), load(f"load-{f}.json")
        rows.append((name, prof, r, ld))
    print("| Config | Load (s) | TTFT short (s) | Decode 1×256 (tok/s) | Batch 8×128 (tok/s) |")
    print("|---|---|---|---|---|")
    for name, prof, r, ld in rows:
        load_s = (ld or {}).get("load_s", "fail" if ld else "–")
        if not r:
            print(f"| {name} `{prof}` | {load_s} | – | – | – |")
            continue
        s1 = r["S1"]
        print(f"| {name} `{prof}` | {load_s} | {m(s1['ttft_short_1tok_s'], '{:.3f}')} | "
              f"{m(s1['decode_tok_s'], '{:.1f}')} | {m(s1['batch8x128_tok_s'], '{:.0f}')} |")
    print()
    print("S2: TTFT seconds, median (IQR) of 3; cold tok/s; hit overhead = hit2 − short TTFT")
    print()
    print("| Config | Size | Tokens | Cold (s) | Cold tok/s | Hit 1 (s) | Hit 2 (s) | Hit overhead (s) |")
    print("|---|---|---|---|---|---|---|---|")
    for name, prof, r, ld in rows:
        if not r or "S2" not in r:
            continue
        short = r["S1"]["ttft_short_1tok_s"]["median"]
        for size, s in r["S2"].items():
            print(f"| {name} | {int(size)//1000}K | {int(s['prompt_tokens']['median'])} | {m(s['cold_s'])} | "
                  f"{m(s['cold_tok_s'], '{:.0f}')} | {m(s['hit1_s'])} | {m(s['hit2_s'])} | "
                  f"{s['hit2_s']['median'] - short:.2f} |")
    print()
    print("| Config | S3 chat longest gap (s) | S3 short-request p95 TTFT (s) | pooled p95 | cold 64K TTFT under load (s) |")
    print("|---|---|---|---|---|")
    for name, prof, r, ld in rows:
        if not r or "S3" not in r:
            continue
        s = r["S3"]
        print(f"| {name} | {m(s['longest_gap_s'])} | {m(s['short_ttft_p95_s'])} | "
              f"{s['short_ttft_p95_pooled_s']:.2f} | {m(s['cold_ttft_s'])} |")


if __name__ == "__main__":
    sys.exit(main())

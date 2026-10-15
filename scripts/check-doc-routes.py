#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""Check that every agent endpoint named in docs/reference/*.md exists in the router.

The route table is parsed out of crates/tt-station-agentd/src/router.rs (every
`.route("<path>", <method>(...)...)` call, multi-line ones included). Docs are
scanned for inline code spans that look like endpoints:

  `GET /people`, `POST /orchard/runs/{name}/{pause|resume}`  -> path AND method must match
  `/status`, `/v1`, `/tt/*`                                   -> path must be a route or a
                                                                 prefix of one

Filesystem paths (`/dev/...`, `/etc/...`), and endpoints of *other* services
the docs legitimately describe (vLLM's `/health`, the media server's
`/v1/videos/generations`, ...) are listed in ALLOW below with a reason.

Usage: scripts/check-doc-routes.py [--list-routes]   (exit 1 on any mismatch)
"""

from __future__ import annotations

import itertools
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUTER_RS = ROOT / "crates/tt-station-agentd/src/router.rs"
DOCS = sorted((ROOT / "docs/reference").glob("*.md"))

METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE")

# First path segments that are filesystem / D-Bus paths, never HTTP routes.
NON_HTTP_PREFIXES = ("dev", "etc", "proc", "sys", "usr", "var", "tmp", "home", "opt", "org")

# Endpoints of other services (or non-endpoints) the reference docs name on purpose.
ALLOW = {
    "/health": "tt-inference-server / vLLM readiness, not the agent",
    "/chat/completions": "upstream OpenAI-compatible path (relative to the served /v1)",
    "/v1/audio/transcriptions": "upstream speech server the jobs runner calls",
    "/v1/videos/generations": "tt-media-server path the agent proxies to",
    "/api/models/{repo}": "Hugging Face Hub API",
    "/explain": "a tt-toplike key binding, not a route",
    "/1": "fragment of /dev/tenstorrent/0 and /1",
    "/": "bare root, used in prose",
}


def parse_routes(src: str) -> dict[str, set[str]]:
    """`{path: {METHOD, ...}}` from every `.route("...", ...)` call."""
    routes: dict[str, set[str]] = {}
    for m in re.finditer(r'\.route\(\s*"([^"]+)"', src):
        # Walk to the matching close paren of this .route( call.
        i, depth = m.start() + len(".route("), 1
        while depth and i < len(src):
            depth += {"(": 1, ")": -1}.get(src[i], 0)
            i += 1
        body = src[m.end() : i]
        methods = {x.upper() for x in re.findall(r"\b(get|post|put|patch|delete)\(", body)}
        routes.setdefault(m.group(1), set()).update(methods)
    return routes


def segs(path: str) -> list[str]:
    return [s for s in path.strip("/").split("/") if s != ""]


def is_param(seg: str) -> bool:
    return (seg.startswith("{") and seg.endswith("}")) or (seg.startswith("<") and seg.endswith(">")) or seg.startswith(":")


def seg_match(doc: str, route: str) -> bool:
    if route.startswith("{"):
        return True  # a route parameter accepts anything the doc writes there
    return doc == route


def path_matches(doc: list[str], route: list[str], prefix_ok: bool) -> bool:
    if route and route[-1].startswith("{*"):
        head = route[:-1]
        return len(doc) >= len(head) and all(seg_match(d, r) for d, r in zip(doc, head))
    if len(doc) > len(route):
        return False
    if len(doc) < len(route) and not prefix_ok:
        return False
    return all(seg_match(d, r) for d, r in zip(doc, route))


def expand(path: str) -> list[str]:
    """`/a/{x|y}` and `/a/start|stop` -> one path per alternative."""
    parts = []
    for s in segs(path):
        inner = s[1:-1] if s.startswith("{") and s.endswith("}") else s
        parts.append(inner.split("|") if "|" in inner else [s])
    return ["/" + "/".join(p) for p in itertools.product(*parts)] or ["/"]


SPAN = re.compile(r"`([^`]+)`")
ENDPOINT = re.compile(rf"^(?:({'|'.join(METHODS)})\s+)?(/[^\s?\[]*)")


def doc_endpoints(text: str):
    for lineno, line in enumerate(text.splitlines(), 1):
        for span in SPAN.findall(line.replace("\\|", "|")):
            m = ENDPOINT.match(span.strip())
            if not m:
                continue
            method, path = m.group(1), m.group(2).rstrip(".,;:")
            yield lineno, span, method, path


def main() -> int:
    routes = parse_routes(ROUTER_RS.read_text())
    if "/status" not in routes:
        # The router moved or changed shape: say so rather than pass on an empty table.
        print(f"{ROUTER_RS.relative_to(ROOT)}: no route table found (no `.route(\"/status\", ...)`)")
        return 1
    if "--list-routes" in sys.argv:
        for p in sorted(routes):
            print(f"{','.join(sorted(routes[p])):<20} {p}")
        return 0
    table = [(segs(p), p, ms) for p, ms in routes.items()]
    problems = 0
    checked = 0
    for doc in DOCS:
        for lineno, span, method, path in doc_endpoints(doc.read_text()):
            first = segs(path)[:1]
            if first and first[0] in NON_HTTP_PREFIXES:
                continue
            for alt in expand(path):
                if alt in ALLOW or alt.rstrip("/") in ALLOW:
                    continue
                wildcard = alt.endswith("/*")
                ds = segs(alt[:-2] if wildcard else alt)
                checked += 1
                hits = [
                    (p, ms)
                    for rs, p, ms in table
                    if path_matches(ds, rs, prefix_ok=wildcard or method is None)
                ]
                where = f"{doc.relative_to(ROOT)}:{lineno}"
                if not hits:
                    print(f"{where}: `{span}` -> no route {alt}")
                    problems += 1
                elif method and not any(method in ms for _, ms in hits):
                    have = sorted({m for _, ms in hits for m in ms})
                    print(f"{where}: `{span}` -> {alt} has no {method} (has {', '.join(have)})")
                    problems += 1
    print(f"checked {checked} endpoint mentions in {len(DOCS)} docs against {len(routes)} routes: "
          f"{problems} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())

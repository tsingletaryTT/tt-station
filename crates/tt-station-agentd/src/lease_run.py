# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""Run a bring-up under a whole-box gozer lease, waiting for the chips if needed.

`gozer run` doesn't wait: when the chips are busy (a model still letting go
of them, someone else's lease) it queues a ticket and exits 10. This keeps
asking until the lease is granted, writing `<status-file>` meanwhile so
TT Station can say "waiting for the chips" (and why), then runs the command
under `gozer run`, which always releases the lease afterwards.

    python lease_run.py STATUS_FILE WHO REASON -- COMMAND...
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import threading
import time

EXIT_QUEUED = 10
TICKET_RE = re.compile(r"queued: ticket (\S+)")


def holders() -> str:
    """Who holds the chips now, in a few words."""
    try:
        out = subprocess.run(["gozer", "status"], capture_output=True, text=True, timeout=20).stdout
    except Exception:
        return ""
    held = {m.group(1).strip() for m in re.finditer(r"HELD\S*\s+(.+)", out)}
    return ", ".join(sorted(held))[:200]


def main() -> int:
    status_file, who, reason = sys.argv[1], sys.argv[2], sys.argv[3]
    cmd = sys.argv[sys.argv.index("--") + 1:]
    started = time.time()
    while True:
        proc = subprocess.Popen(
            # Not --fresh: gozer only counts chips it reset itself as fresh, so chips a
            # model just let go of would wait forever. TT Station resets them before
            # a run starts (and "Make the chips available" does too).
            ["gozer", "run", "--chips", "4", "--who", who, "--reason", reason, "--", *cmd],
            stderr=subprocess.PIPE, text=True,
        )
        ticket: list[str] = []

        def copy() -> None:
            for line in proc.stderr:  # type: ignore[union-attr]
                m = TICKET_RE.search(line)
                if m:
                    ticket.append(m.group(1))
                    continue
                sys.stderr.write(line)
                sys.stderr.flush()

        t = threading.Thread(target=copy, daemon=True)
        t.start()
        code = proc.wait()
        t.join(timeout=2)
        if code != EXIT_QUEUED:
            try:
                import os
                os.remove(status_file)
            except OSError:
                pass
            return code
        # Queued: drop the ticket (we'll ask again) and say what we're waiting on.
        for tk in ticket:
            subprocess.run(["gozer", "cancel", tk], capture_output=True)
        with open(status_file, "w") as f:
            json.dump({"waiting_for_chips": True, "since": started, "held_by": holders()}, f)
        print(f"[tt-station] the chips are busy ({holders() or 'releasing'}); asking again in 20 s", file=sys.stderr, flush=True)
        time.sleep(20)


if __name__ == "__main__":
    raise SystemExit(main())

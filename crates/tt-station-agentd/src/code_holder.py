# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""Holds one Code session's Claude Code, so it outlives the agent.

The agent starts this under its own systemd user unit. It starts Claude Code
(on the box or in its sandbox container) with pipes, writes every line Claude
Code prints to a journal, and serves a unix socket: whoever connects says
`ATTACH <n>` and gets every journal line after the first n, then the rest as
they come; what they send goes to Claude Code. When the agent restarts it
connects again and carries on from the line it had reached, so a turn in
progress -- or a question waiting for the person -- survives.

When Claude Code exits, `#EXIT <code>` goes to the journal and the client, and
this exits too.

argv[1]: a JSON spec, read once and then deleted (it can hold the session's
key): {argv, env, cwd, socket, journal, stderr}.
"""

import json
import os
import socket
import subprocess
import sys
import threading

spec_path = sys.argv[1]
with open(spec_path) as f:
    spec = json.load(f)
try:
    os.unlink(spec_path)
except OSError:
    pass

env = os.environ.copy()
env.update(spec.get("env") or {})
with open(spec["stderr"], "ab") as err:
    child = subprocess.Popen(
        spec["argv"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err,
        cwd=spec.get("cwd") or None, env=env,
    )

journal = open(spec["journal"], "wb")
lock = threading.Lock()
client = None


def send_line(conn, line):
    try:
        conn.sendall(line)
        return True
    except OSError:
        return False


def pump_child():
    """Claude Code's output: to the journal, and to the client if one is there."""
    global client
    for line in child.stdout:
        with lock:
            journal.write(line)
            journal.flush()
            if client is not None and not send_line(client, line):
                client = None
    code = child.wait()
    with lock:
        end = f"#EXIT {code}\n".encode()
        journal.write(end)
        journal.flush()
        if client is not None:
            send_line(client, end)
            try:
                client.close()
            except OSError:
                pass
    try:
        os.unlink(spec["socket"])
    except OSError:
        pass
    os._exit(0)


def serve_client(conn):
    """One client: replay from its line, then live; its lines go to Claude Code."""
    global client
    reader = conn.makefile("rb")
    first = reader.readline().decode(errors="replace").split()
    skip = int(first[1]) if len(first) == 2 and first[0] == "ATTACH" and first[1].isdigit() else 0
    with lock:
        if client is not None:
            try:
                # shutdown, not just close: its reader thread's makefile keeps
                # the socket open, and it would go on sending input.
                client.shutdown(socket.SHUT_RDWR)
                client.close()
            except OSError:
                pass
        with open(spec["journal"], "rb") as j:
            for i, line in enumerate(j):
                if i >= skip and not send_line(conn, line):
                    return
        client = conn
    for line in reader:
        try:
            child.stdin.write(line)
            child.stdin.flush()
        except (BrokenPipeError, ValueError):
            break
    with lock:
        if client is conn:
            client = None


try:
    os.unlink(spec["socket"])
except OSError:
    pass
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(spec["socket"])
os.chmod(spec["socket"], 0o600)
server.listen(2)
threading.Thread(target=pump_child, daemon=True).start()
while True:
    conn, _ = server.accept()
    threading.Thread(target=serve_client, args=(conn,), daemon=True).start()

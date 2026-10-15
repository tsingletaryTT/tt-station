# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""Holds one coding session's terminal for TT Station (like a tiny tmux).

    python3 -c <this file> SOCKET LOG COLS ROWS -- COMMAND...

Runs COMMAND in a pseudo-terminal and serves it on a Unix socket, so TT
Station's desktop and phone apps can watch and type into the same session,
leave, and come back: a new viewer first gets the recent screen output. It
runs under its own systemd unit, so the session outlives the app and the
agent. It exits when the command does, writing its exit code beside the log.

Viewers send lines: `i <base64>` (keys) and `r <cols> <rows>` (resize).
They get the terminal's raw output bytes.
"""
import base64
import fcntl
import os
import pty
import select
import signal
import socket
import struct
import sys
import termios

KEEP = 400_000  # bytes of recent output a new viewer gets


def main():
    sock_path, log_path, cols, rows = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    cmd = sys.argv[sys.argv.index("--") + 1:]
    pid, master = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLORTERM"] = "truecolor"
        try:
            os.execvp(cmd[0], cmd)
        except Exception as exc:  # the command doesn't exist: say so in the terminal
            print(f"couldn't start {cmd[0]}: {exc}")
            os._exit(127)

    def resize(c, r):
        try:
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", r, c, 0, 0))
            os.kill(pid, signal.SIGWINCH)
        except OSError:
            pass

    resize(cols, rows)
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(sock_path)
    os.chmod(sock_path, 0o600)
    server.listen(8)
    log = open(log_path, "ab", buffering=0)
    recent = bytearray()
    viewers = {}  # socket -> partial input line

    def stop(*_):
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGHUP, stop)
    status = None
    while status is None:
        fds = [master, server] + list(viewers)
        try:
            ready, _, _ = select.select(fds, [], [], 1.0)
        except InterruptedError:
            continue
        for fd in ready:
            if fd is master:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    data = b""
                if not data:
                    _, code = os.waitpid(pid, 0)
                    status = os.waitstatus_to_exitcode(code)
                    break
                log.write(data)
                recent.extend(data)
                if len(recent) > KEEP:
                    del recent[: len(recent) - KEEP]
                for v in list(viewers):
                    try:
                        v.sendall(data)
                    except OSError:
                        viewers.pop(v, None)
                        v.close()
            elif fd is server:
                conn, _ = server.accept()
                try:
                    conn.sendall(bytes(recent))
                    viewers[conn] = b""
                except OSError:
                    conn.close()
            else:
                try:
                    data = fd.recv(65536)
                except OSError:
                    data = b""
                if not data:
                    viewers.pop(fd, None)
                    fd.close()
                    continue
                buf = viewers[fd] + data
                *lines, rest = buf.split(b"\n")
                viewers[fd] = rest
                for line in lines:
                    if line.startswith(b"i "):
                        try:
                            os.write(master, base64.b64decode(line[2:]))
                        except Exception:
                            pass
                    elif line.startswith(b"r "):
                        parts = line.split()
                        if len(parts) == 3 and parts[1].isdigit() and parts[2].isdigit():
                            resize(int(parts[1]), int(parts[2]))
    with open(log_path + ".exit", "w") as f:
        f.write(str(status))
    end = f"\r\n\x1b[2m[session ended, exit {status}]\x1b[0m\r\n".encode()
    for v in list(viewers):
        try:
            v.sendall(end)
            v.close()
        except OSError:
            pass
    try:
        os.unlink(sock_path)
    except OSError:
        pass


if __name__ == "__main__":
    main()

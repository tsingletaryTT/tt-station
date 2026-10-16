# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
"""Run a command inside a Landlock sandbox: what a bring-up stage may touch.

TT Station starts bring-up runs as systemd user units, but this box's user
manager can't give them a mount namespace (Ubuntu's AppArmor refuses
CAP_SYS_ADMIN in unprivileged user namespaces, and systemd then silently
drops ProtectHome=/InaccessiblePaths=/ReadWritePaths=). Landlock needs no
privileges, so this is the layer that holds:

* every file is readable and runnable except under each `--deny` path
  (TT Station's own folder, SSH and GnuPG keys, the keyring);
* only the `--rw` paths are writable (the run's folder, the repo, caches);
* `--ro-file` files under a denied folder stay readable (the drivers);
* signals and abstract Unix sockets don't reach outside the sandbox, and
  neither does ptrace or /proc/<pid>/environ of a process outside it (the
  agent's), which Landlock refuses by itself.

    python sandbox_exec.py [--deny P]... [--rw P]... [--ro-file F]... -- COMMAND...

Folder names along the way to a denied path stay listable (names only, no
contents): a program that lists `~` still works.

Without Landlock (an old kernel) it says so on stderr and runs the command
anyway, unless `--require`.
"""

from __future__ import annotations

import ctypes
import os
import platform
import stat
import sys

# Syscall numbers are the same on x86_64 and aarch64 (asm-generic).
SYS_CREATE, SYS_ADD_RULE, SYS_RESTRICT = 444, 445, 446
CREATE_RULESET_VERSION = 1
RULE_PATH_BENEATH = 1
PR_SET_NO_NEW_PRIVS = 38

EXECUTE, WRITE_FILE, READ_FILE, READ_DIR = 1 << 0, 1 << 1, 1 << 2, 1 << 3
REMOVE_DIR, REMOVE_FILE, MAKE_CHAR, MAKE_DIR = 1 << 4, 1 << 5, 1 << 6, 1 << 7
MAKE_REG, MAKE_SOCK, MAKE_FIFO, MAKE_BLOCK = 1 << 8, 1 << 9, 1 << 10, 1 << 11
MAKE_SYM, REFER, TRUNCATE, IOCTL_DEV = 1 << 12, 1 << 13, 1 << 14, 1 << 15
SCOPE_ABSTRACT_UNIX, SCOPE_SIGNAL = 1 << 0, 1 << 1

# What a file (not a folder) can be granted.
FILE_RIGHTS = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV
READ = EXECUTE | READ_FILE | READ_DIR | IOCTL_DEV


def handled_for(abi: int) -> int:
    """Every filesystem right this kernel's Landlock knows (all of them are refused unless granted)."""
    rights = (1 << 13) - 1
    if abi >= 2:
        rights |= REFER
    if abi >= 3:
        rights |= TRUNCATE
    if abi >= 5:
        rights |= IOCTL_DEV
    return rights


def norm(p: str) -> str:
    return os.path.realpath(os.path.expanduser(p))


def under(path: str, root: str) -> bool:
    return path == root or path.startswith(root.rstrip("/") + "/")


def read_rules(deny: list[str], root: str = "/") -> list[tuple[str, str]]:
    """The read rules that leave out each denied path: `(path, "read")` for
    whole subtrees, `(folder, "list")` for the folders on the way to a denied
    path (their names stay listable). Symlinks get no rule: what they point at
    is readable only if it is readable by its own path."""
    deny = sorted({norm(d) for d in deny})
    root = norm(root)
    rules: list[tuple[str, str]] = []

    def visit(folder: str) -> None:
        rules.append((folder, "list"))
        try:
            names = sorted(os.listdir(folder))
        except OSError:
            return
        for name in names:
            child = os.path.join(folder, name)
            if child in deny:
                continue
            try:
                st = os.lstat(child)
            except OSError:
                continue
            if stat.S_ISLNK(st.st_mode):
                continue
            if any(under(d, child) for d in deny) and stat.S_ISDIR(st.st_mode):
                visit(child)
            else:
                rules.append((child, "read"))

    if not any(under(d, root) for d in deny):
        return [(root, "read")]
    if root in deny:
        return []
    visit(root)
    return rules


def plan(deny: list[str], rw: list[str], ro_files: list[str]) -> list[tuple[str, str]]:
    """All the rules: reads everywhere but the denied paths, writes to the
    `rw` paths that exist, and reads of the `ro_files`."""
    rules = read_rules(deny)
    rules += [(norm(p), "rw") for p in rw if os.path.exists(norm(p))]
    rules += [(norm(f), "read") for f in ro_files if os.path.isfile(norm(f))]
    return rules


class RulesetAttr(ctypes.Structure):
    _fields_ = [("fs", ctypes.c_uint64), ("net", ctypes.c_uint64), ("scoped", ctypes.c_uint64)]


class PathBeneath(ctypes.Structure):
    _pack_ = 1
    _fields_ = [("allowed", ctypes.c_uint64), ("parent_fd", ctypes.c_int32)]


def libc():
    return ctypes.CDLL(None, use_errno=True)


def abi_version() -> int:
    if platform.system() != "Linux":
        return 0
    v = libc().syscall(SYS_CREATE, None, ctypes.c_size_t(0), ctypes.c_uint32(CREATE_RULESET_VERSION))
    return max(v, 0)


def enforce(rules: list[tuple[str, str]]) -> int:
    """Restrict this process (and everything it starts) to `rules`. Returns the ABI."""
    abi = abi_version()
    if abi < 1:
        raise OSError("Landlock isn't available on this kernel")
    c = libc()
    handled = handled_for(abi)
    attr = RulesetAttr(handled, 0, (SCOPE_ABSTRACT_UNIX | SCOPE_SIGNAL) if abi >= 6 else 0)
    size = ctypes.sizeof(RulesetAttr) if abi >= 6 else 16 if abi >= 4 else 8
    ruleset = c.syscall(SYS_CREATE, ctypes.byref(attr), ctypes.c_size_t(size), ctypes.c_uint32(0))
    if ruleset < 0:
        raise OSError(ctypes.get_errno(), "landlock_create_ruleset")
    try:
        for path, kind in rules:
            try:
                fd = os.open(path, os.O_PATH | os.O_CLOEXEC)
            except OSError:
                continue
            try:
                is_dir = stat.S_ISDIR(os.fstat(fd).st_mode)
                want = {"list": READ_DIR, "read": READ, "rw": handled}[kind]
                if not is_dir:
                    want &= FILE_RIGHTS
                want &= handled
                if not want:
                    continue
                rule = PathBeneath(want, fd)
                if c.syscall(SYS_ADD_RULE, ruleset, ctypes.c_uint32(RULE_PATH_BENEATH), ctypes.byref(rule), ctypes.c_uint32(0)) < 0:
                    raise OSError(ctypes.get_errno(), f"landlock_add_rule {path}")
            finally:
                os.close(fd)
        if c.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0:
            raise OSError(ctypes.get_errno(), "prctl(NO_NEW_PRIVS)")
        if c.syscall(SYS_RESTRICT, ruleset, ctypes.c_uint32(0)) < 0:
            raise OSError(ctypes.get_errno(), "landlock_restrict_self")
    finally:
        os.close(ruleset)
    return abi


def parse(argv: list[str]) -> tuple[dict[str, list[str]], bool, list[str]]:
    if "--" not in argv:
        raise SystemExit("usage: sandbox_exec.py [--deny P] [--rw P] [--ro-file F] [--require] -- COMMAND...")
    i = argv.index("--")
    opts, cmd = argv[:i], argv[i + 1:]
    lists: dict[str, list[str]] = {"--deny": [], "--rw": [], "--ro-file": []}
    require = False
    j = 0
    while j < len(opts):
        if opts[j] == "--require":
            require = True
            j += 1
        elif opts[j] in lists and j + 1 < len(opts):
            lists[opts[j]].append(opts[j + 1])
            j += 2
        else:
            raise SystemExit(f"sandbox_exec.py: unknown option {opts[j]}")
    if not cmd:
        raise SystemExit("sandbox_exec.py: no command")
    return lists, require, cmd


def main(argv: list[str]) -> int:
    lists, require, cmd = parse(argv)
    rules = plan(lists["--deny"], lists["--rw"], lists["--ro-file"])
    try:
        abi = enforce(rules)
        print(f"[tt-station] sandbox: Landlock ABI {abi}; hidden: {', '.join(lists['--deny']) or 'nothing'}; "
              f"writable: {', '.join(lists['--rw']) or 'nothing'}", file=sys.stderr, flush=True)
    except OSError as e:
        if require:
            print(f"[tt-station] sandbox: couldn't confine the run ({e}); not starting it", file=sys.stderr, flush=True)
            return 126
        print(f"[tt-station] sandbox: couldn't confine the run ({e}); running without file containment",
              file=sys.stderr, flush=True)
    os.execvp(cmd[0], cmd)
    return 127


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

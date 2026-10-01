"""Find a RISC-V-capable toolchain for blackhole-py's firmware on macOS, and check prerequisites.

blackhole-py compiles its RISC-V firmware with `CC` (default: the first `clang` on PATH), links with
`TT_RISCV_LD` and extracts with `TT_RISCV_OBJCOPY` (firmware/__init__.py `_tool`). On a Mac the
first `clang` on PATH is Apple's, which has NO RISC-V backend, so the build fails with a confusing
error. Homebrew's keg-only LLVM does have one (verified 2026-09-30: `clang -print-targets` lists
riscv32/riscv64), and Homebrew's riscv-gnu-toolchain names its binutils `riscv64-unknown-elf-*`,
not the Linux `riscv64-linux-gnu-*` blackhole-py looks for. With Homebrew clang 22.1.8,
riscv64-unknown-elf-ld and llvm-objcopy, blackhole-py d8eae8f built its 9,576-byte firmware on this
Mac. No reference build exists to byte-compare against, so silicon is the real test.

apply() fills in only the variables the user hasn't set; an explicit choice always wins.
"""
import os
import shutil
import subprocess

BREW_PREFIXES = ("/opt/homebrew", "/usr/local")


def _brew(keg, tool):
    return [os.path.join(p, "opt", keg, "bin", tool) for p in BREW_PREFIXES]


def _exe(path):
    found = shutil.which(path) if os.sep not in path else (path if os.access(path, os.X_OK) else None)
    return found


def clang_targets_riscv(path):
    """True if this clang can emit riscv32 (Apple's clang can't)."""
    try:
        out = subprocess.run([path, "-print-targets"], capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.SubprocessError):
        return False
    return "riscv32" in out


def _first(candidates, accept=lambda p: True):
    for c in candidates:
        p = _exe(c)
        if p and accept(p):
            return p
    return None


def find(env=os.environ):
    """{'CC': path|None, 'TT_RISCV_LD': …, 'TT_RISCV_OBJCOPY': …}. Honors explicit env settings."""
    cc = env.get("CC") or _first(_brew("llvm", "clang") + ["clang"], clang_targets_riscv)
    ld = env.get("TT_RISCV_LD") or _first(
        ["riscv32-unknown-elf-ld", "riscv64-unknown-elf-ld", "riscv64-linux-gnu-ld"]
        + _brew("riscv-gnu-toolchain", "riscv64-unknown-elf-ld")
        + ["ld.lld"] + _brew("lld", "ld.lld") + _brew("llvm", "ld.lld"))
    objcopy = env.get("TT_RISCV_OBJCOPY") or _first(
        _brew("llvm", "llvm-objcopy") + ["llvm-objcopy", "riscv32-unknown-elf-objcopy", "riscv64-unknown-elf-objcopy"]
        + _brew("riscv-gnu-toolchain", "riscv64-unknown-elf-objcopy") + ["riscv64-linux-gnu-objcopy"])
    return {"CC": cc, "TT_RISCV_LD": ld, "TT_RISCV_OBJCOPY": objcopy}


def apply(env=os.environ):
    """Set any unset toolchain variables to what find() located. Returns what's in effect."""
    found = find(env)
    for k, v in found.items():
        if v and not env.get(k):
            env[k] = v
    return found


def doctor(bhpy_dir=None, env=os.environ):
    """Print a checklist; return True when everything needed to run blackhole-py is present."""
    import importlib.util
    from . import client

    rows = []
    tc = find(env)
    rows.append(("RISC-V clang (CC)", tc["CC"], "brew install llvm  (Apple clang has no RISC-V target)"))
    rows.append(("RISC-V linker (TT_RISCV_LD)", tc["TT_RISCV_LD"], "brew install riscv-gnu-toolchain  (or lld)"))
    rows.append(("objcopy (TT_RISCV_OBJCOPY)", tc["TT_RISCV_OBJCOPY"], "brew install llvm"))
    for mod in ("numpy", "transformers", "huggingface_hub"):
        ok = importlib.util.find_spec(mod) is not None     # find_spec: no import side effects
        rows.append((f"python: {mod}", mod if ok else None, "pip install -r <blackhole-py>/requirements.txt (in a venv)"))
    if bhpy_dir:
        p = os.path.join(os.path.expanduser(bhpy_dir), "pcie.py")
        rows.append(("blackhole-py checkout", p if os.path.exists(p) else None, "git clone https://github.com/boopdotpng/blackhole-py"))
    sock = env.get("TTBH_BROKER_SOCKET", client.DEFAULT_SOCKET)
    rows.append(("ttbh broker socket", sock if os.path.exists(sock) else None,
                 "TTStationDriver serve  (needs the dext loaded; see macos/TTStationDriver/README.md)"))

    print("╔══ ttstation_bhpy doctor")
    for name, value, fix in rows:
        print(f"║  {'✓' if value else '✗'} {name:<30} {value or '— ' + fix}")
    ok = all(v for _, v, _ in rows)
    print(f"╚══ {'ready' if ok else 'not ready yet'}")
    return ok

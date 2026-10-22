"""ttstation_bhpy.toolchain: pick a RISC-V-capable toolchain on macOS (hardware-free)."""
import os
import stat
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from ttstation_bhpy import toolchain  # noqa: E402

BHPY_DIR = os.environ.get("BHPY_DIR")


def _script(path, body):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write("#!/bin/sh\n" + body + "\n")
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)


class Detection(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.bin = os.path.join(self.tmp, "bin")                      # plays PATH
        self.brew = os.path.join(self.tmp, "brew")                    # plays /opt/homebrew
        # An Apple-like clang first on PATH: no RISC-V target (exactly what bit us on this Mac).
        _script(os.path.join(self.bin, "clang"), 'echo "  aarch64 - AArch64"; echo "  x86-64 - 64-bit X86"')
        # Homebrew's keg-only LLVM: has riscv32.
        _script(os.path.join(self.brew, "opt/llvm/bin/clang"), 'echo "  riscv32 - 32-bit RISC-V"')
        _script(os.path.join(self.brew, "opt/llvm/bin/llvm-objcopy"), "true")
        _script(os.path.join(self.bin, "riscv64-unknown-elf-ld"), "true")
        self.saved = (toolchain.BREW_PREFIXES, os.environ.get("PATH"))
        toolchain.BREW_PREFIXES = (self.brew,)
        os.environ["PATH"] = self.bin

    def tearDown(self):
        toolchain.BREW_PREFIXES, path = self.saved
        os.environ["PATH"] = path

    def test_skips_a_clang_without_riscv_and_finds_homebrew_llvm(self):
        found = toolchain.find(env={})
        self.assertEqual(found["CC"], os.path.join(self.brew, "opt/llvm/bin/clang"))
        self.assertEqual(found["TT_RISCV_LD"], os.path.join(self.bin, "riscv64-unknown-elf-ld"))
        self.assertEqual(found["TT_RISCV_OBJCOPY"], os.path.join(self.brew, "opt/llvm/bin/llvm-objcopy"))

    def test_an_explicit_choice_always_wins(self):
        env = {"CC": "/my/own/clang"}
        self.assertEqual(toolchain.find(env)["CC"], "/my/own/clang")
        toolchain.apply(env)
        self.assertEqual(env["CC"], "/my/own/clang")                  # not overwritten
        self.assertTrue(env["TT_RISCV_LD"].endswith("riscv64-unknown-elf-ld"))

    def test_no_riscv_clang_anywhere_is_reported_not_guessed(self):
        os.remove(os.path.join(self.brew, "opt/llvm/bin/clang"))
        self.assertIsNone(toolchain.find(env={})["CC"])              # never falls back to Apple clang


@unittest.skipUnless(BHPY_DIR, "BHPY_DIR not set")
class RealFirmwareBuild(unittest.TestCase):
    """blackhole-py's firmware, built with the toolchain apply() picks, on this machine."""

    def test_firmware_builds(self):
        env = dict(os.environ)
        found = toolchain.apply(env)
        if not all(found.values()):
            self.skipTest(f"no RISC-V toolchain here: {found}")
        out = os.path.join(tempfile.mkdtemp(), "fw.bin")
        r = subprocess.run([sys.executable, "-m", "firmware", out], cwd=BHPY_DIR, env=env,
                           capture_output=True, text=True, timeout=600)
        self.assertEqual(r.returncode, 0, r.stderr[-2000:])
        self.assertGreater(os.path.getsize(out), 1024)


if __name__ == "__main__":
    unittest.main()

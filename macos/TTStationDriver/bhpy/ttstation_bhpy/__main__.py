"""python3 -m ttstation_bhpy BLACKHOLE_PY_DIR SCRIPT [ARGS...]   run a blackhole-py script on macOS
   python3 -m ttstation_bhpy doctor [BLACKHOLE_PY_DIR]          check prerequisites"""
import os
import runpy
import sys

from . import install, toolchain

if len(sys.argv) >= 2 and sys.argv[1] == "doctor":
    sys.exit(0 if toolchain.doctor(sys.argv[2] if len(sys.argv) > 2 else None) else 1)
if len(sys.argv) < 3:
    sys.exit(__doc__)
bhpy, script = sys.argv[1], sys.argv[2]
toolchain.apply()          # RISC-V-capable clang/ld/objcopy unless the user chose their own
install(bhpy)
sys.argv = [script] + sys.argv[3:]
os.chdir(os.path.expanduser(bhpy))
runpy.run_path(script, run_name="__main__")

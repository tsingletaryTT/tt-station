"""python3 -m ttstation_bhpy BLACKHOLE_PY_DIR SCRIPT [ARGS...] — run a blackhole-py script on macOS."""
import os
import runpy
import sys

from . import install

if len(sys.argv) < 3:
    sys.exit(__doc__)
bhpy, script = sys.argv[1], sys.argv[2]
install(bhpy)
sys.argv = [script] + sys.argv[3:]
os.chdir(os.path.expanduser(bhpy))
runpy.run_path(script, run_name="__main__")

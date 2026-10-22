"""Run tt-smi on macOS against tt-umd's tt-kmd emulation.

tt-smi checks for the kernel driver by reading /sys/module/tenstorrent/version before it touches
UMD. macOS has no /sys, so this answers that check with the emulated tt-kmd version and then
hands over to tt-smi's own main(). Nothing in the installed tt-smi is modified.
"""
import sys

import tt_smi.utils
import tt_tools_common.utils_common.system_utils as system_utils
import tt_smi.tt_smi
import tt_smi.latest_releases  # noqa: F401  (reads the same file; patched below if it exposes a helper)

EMULATED_KMD = "2.11.0"  # matches tt_darwin_kmd_version()

def get_driver_version():
    return EMULATED_KMD

tt_smi.utils.get_driver_version = get_driver_version
system_utils.get_driver_version = get_driver_version  # tt-tools-common keeps its own copy
tt_smi.tt_smi.get_driver_version = get_driver_version

if __name__ == "__main__":
    sys.argv[0] = "tt-smi"
    sys.exit(tt_smi.tt_smi.main())

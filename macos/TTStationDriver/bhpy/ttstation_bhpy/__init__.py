"""Run blackhole-py on a Mac through tt-station's DriverKit driver.

    import ttstation_bhpy
    ttstation_bhpy.install("~/code/blackhole-py")   # before anything imports blackhole-py's device
    from device import Device                        # blackhole-py, now on TTStationDriver

or `python3 -m ttstation_bhpy ~/code/blackhole-py examples/matmul_peak.py`.

install() puts the blackhole-py checkout on sys.path, loads ITS pcie.py under a private name (for
Allocator, board_config and the layout constants, which we must not copy), and registers a `pcie`
module whose PCIDevice/TLBWindow/Sysmem go through the ttbh broker (pcie_darwin.py). Everything
else in blackhole-py is untouched.
"""
import importlib.util
import os
import sys
import types

from . import pcie_darwin, toolchain  # noqa: F401  (toolchain.apply/doctor for library users)

__all__ = ["install", "toolchain"]


def install(bhpy_dir):
    bhpy_dir = os.path.abspath(os.path.expanduser(bhpy_dir))
    original_path = os.path.join(bhpy_dir, "pcie.py")
    if not os.path.exists(original_path):
        raise FileNotFoundError(f"no pcie.py in {bhpy_dir}: point install() at a blackhole-py checkout")
    spec = importlib.util.spec_from_file_location("_blackhole_py_pcie", original_path)
    original = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(original)

    shim = types.ModuleType("pcie", pcie_darwin.__doc__)
    for name in dir(original):          # constants, Allocator, board_config, BoardConfig, …
        if not name.startswith("__"):
            setattr(shim, name, getattr(original, name))
    pcie_darwin.Allocator = original.Allocator
    pcie_darwin.board_config = original.board_config
    for name in ("PCIDevice", "TLBWindow", "Sysmem"):
        setattr(shim, name, getattr(pcie_darwin, name))

    if bhpy_dir not in sys.path:
        sys.path.insert(0, bhpy_dir)
    sys.modules["pcie"] = shim
    return shim

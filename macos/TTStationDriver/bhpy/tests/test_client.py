"""ttstation_bhpy against the REAL C broker over libttbh's simulated Blackhole (ttbh-broker-sim).

Run: make -C macos/TTStationDriver/bhpy test   (BHPY_DIR=<blackhole-py checkout> adds parity tests)

What this exercises end to end, without hardware: the wire protocol, SCM_RIGHTS fd passing, TLB
window bookkeeping and packing (verified by the sim's independent bitfield decoder: a wrong packing
shows the wrong memory), telemetry through the server's window, ARC messages through the fake
firmware, and the sysmem iATU plan over deliberately fragmented fake IOVAs.
"""
import inspect
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from ttstation_bhpy import client as wire, pcie_darwin  # noqa: E402

BROKER = os.environ.get("TTBH_BROKER_SIM")
BHPY_DIR = os.environ.get("BHPY_DIR")


class BrokerCase(unittest.TestCase):
    def setUp(self):
        if not BROKER:
            self.skipTest("TTBH_BROKER_SIM not set (run via make test)")
        self.tmp = tempfile.mkdtemp()
        self.sock = os.path.join(self.tmp, "ttbh.sock")
        self.proc = subprocess.Popen([BROKER, self.sock], stderr=subprocess.PIPE)
        os.environ["TTBH_BROKER_SOCKET"] = self.sock
        # Retry the connection itself rather than waiting for the socket file: bind() creates the
        # file a moment before listen(), and on macOS the FIRST launch of a freshly built binary is
        # held for a security assessment that can take seconds (the flake this replaced: 1 failure
        # right after a rebuild, 0 in 20 steady-state runs).
        deadline = time.monotonic() + 10
        while True:
            try:
                self.c = wire.BrokerClient(self.sock)
                break
            except OSError:
                if time.monotonic() > deadline or self.proc.poll() is not None:
                    raise
                time.sleep(0.02)

    def tearDown(self):
        if hasattr(self, "c"):
            self.c.close()
        if hasattr(self, "proc"):
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:        # never leave a broker behind
                self.proc.kill()
                self.proc.wait()
            self.proc.stderr.close()
        if hasattr(self, "tmp"):
            shutil.rmtree(self.tmp, ignore_errors=True)    # the per-test socket directory


class Protocol(BrokerCase):
    def test_hello_reports_the_p100a(self):
        ver, ids, _, _ = self.c.call(wire.HELLO)
        self.assertEqual(ver, 1)
        self.assertEqual(ids >> 16, 0xB140)
        self.assertEqual(pcie_darwin.BOARD_TYPES[ids & 0xFFFF], "p100a")

    def test_telemetry_through_the_servers_window(self):
        self.assertEqual(self.c.call(wire.TELEMETRY, a0=14)[0], 800)          # AICLK
        self.assertEqual(self.c.call(wire.TELEMETRY, a0=34)[0], 0x0FFF)       # enabled Tensix columns
        with self.assertRaises(wire.BrokerError) as e:
            self.c.call(wire.TELEMETRY, a0=999)
        self.assertEqual(e.exception.status, -5)                               # tag not present

    def test_arc_test_echo_through_fake_firmware(self):
        _, _, data, _ = self.c.call(wire.ARC_MSG, payload=struct.pack("<8I", 0x90, 0xBEEF, *([0] * 6)))
        status, echo = struct.unpack_from("<2I", data)
        self.assertEqual((status, echo), (0, 0xBEF0))


class OneOwner(BrokerCase):
    def test_a_second_client_times_out_instead_of_hanging(self):
        os.environ["TTBH_BROKER_TIMEOUT"] = "0.5"
        try:
            second = wire.BrokerClient(self.sock)
            with self.assertRaises(TimeoutError) as e:
                second.call(wire.HELLO)
            self.assertIn("busy with another client", str(e.exception))
            second.close()
        finally:
            del os.environ["TTBH_BROKER_TIMEOUT"]


class Windows(BrokerCase):
    def test_tlb_windows_reach_the_tile_they_target(self):
        a = pcie_darwin.TLBWindow(self.c, (1, 2))
        b = pcie_darwin.TLBWindow(self.c, (3, 4))
        self.assertNotEqual(a.id, b.id)
        a.target(0x0)
        b.target(0x0)
        a.write(0x40, 0x11223344)
        b.write(0x40, 0x55667788)
        # Re-aim both (forces the sim to swap pages in and out) and read back each tile's value.
        a.target(0x200000)
        a.target(0x0)
        b.target(0x0)
        self.assertEqual(int.from_bytes(a.read(0x40), "little"), 0x11223344)
        self.assertEqual(int.from_bytes(b.read(0x40), "little"), 0x55667788)
        # Bulk bytes, and a different 2 MiB page on the same tile starts empty.
        a.write(0x100, b"blackhole-on-a-mac!!")
        self.assertEqual(a.read(0x100, 20), b"blackhole-on-a-mac!!")
        a.target(0x400000)
        self.assertEqual(a.read(0x40), b"\0\0\0\0")
        a.close(), b.close()

    def test_a_window_aimed_at_the_arc_reads_the_arcs_memory(self):
        # Absolute check (round trips alone would pass with x/y swapped consistently): the ARC tile
        # is (8, 0), and its boot status (0x5 in the sim) sits at 0x80030408. This is the same path
        # blackhole-py's _read_enabled_masks takes with a TLBWindow at (8, 0).
        with pcie_darwin.TLBWindow(self.c, (8, 0)) as w:
            w.target(0x80000000)
            self.assertEqual(int.from_bytes(w.read(0x30408), "little"), 0x5)

    def test_window_bounds_and_lifecycle(self):
        w = pcie_darwin.TLBWindow(self.c, (1, 2))
        w.target(0)
        with self.assertRaises(wire.BrokerError):
            w.read(pcie_darwin.TLBWindow.SIZE - 2, 4)          # runs off the window
        with self.assertRaises(wire.BrokerError):
            w.target(0x1000)                                   # not 2 MiB aligned
        wid = w.id
        w.close()
        with self.assertRaises(wire.BrokerError):
            self.c.call(wire.TLB_READ, wid, 0, 4)              # freed window can't be used
        with pcie_darwin.TLBWindow(self.c, (1, 2)) as again:
            self.assertEqual(again.id, wid)                    # and its id is reused

    def test_an_offset_that_wraps_64_bits_is_rejected(self):
        # Raw requests, bypassing TLBWindow's own checks: 2^64 - 2 + 4 wraps to 2, so a naive
        # `a0 + len > 2 MiB` check passes and the broker would read/write far outside BAR0.
        with pcie_darwin.TLBWindow(self.c, (1, 2)) as w:
            w.target(0)
            huge = (1 << 64) - 2
            with self.assertRaises(wire.BrokerError):
                self.c.call(wire.TLB_READ, w.id, huge, 4)
            with self.assertRaises(wire.BrokerError):
                self.c.call(wire.TLB_WRITE, w.id, huge, payload=b"\0" * 4)
            self.assertEqual(len(w.read(0, 4)), 4)            # session still healthy afterwards

    def test_multicast_rectangle_is_accepted(self):
        with pcie_darwin.TLBWindow(self.c, (1, 2)) as w:
            w.target(0x0, (1, 2), (14, 11))                    # device.py's soft-reset broadcast
            w.write(0, 1)


class HostMemory(BrokerCase):
    def setUp(self):
        super().setUp()
        pcie_darwin.Allocator = _MiniAllocator                 # blackhole-py's is used when installed

    def test_sysmem_is_shared_and_chip_visible_at_the_pcie_offset(self):
        m = pcie_darwin.Sysmem(self.c, 1 << 20)
        self.assertEqual(m.noc_addr, 4 << 58)                  # TTBH_NOC_PCIE_OFFSET + base 0
        m.write(0x123, b"dma me")
        self.assertEqual(m.read(0x123, 6), b"dma me")
        n = pcie_darwin.Sysmem(self.c, 1 << 20)
        self.assertEqual(n.noc_addr, (4 << 58) + (1 << 20))    # adjacent chip-visible range
        m.close(), n.close()

    def test_a_dropped_client_releases_everything(self):
        pcie_darwin.Sysmem(self.c, 1 << 20)                     # leaked on purpose
        self.c.close()
        self.c = wire.BrokerClient(self.sock)
        m = pcie_darwin.Sysmem(self.c, 1 << 20)
        self.assertEqual(m.noc_addr, 4 << 58)                   # fresh session, regions free again
        m.close()


class _MiniAllocator:
    def __init__(self, start, end, alignment=1):
        self.next, self.end, self.alignment = start, end, alignment

    def alloc(self, size, alignment=None):
        a = alignment or self.alignment
        off = (self.next + a - 1) & -a
        self.next = off + size
        return off


def _shape(fn):
    """A signature's caller-visible shape: names, kinds, defaults. Annotations are dropped: theirs
    say `fd: int`, and on macOS fd is deliberately a broker client, not an int."""
    return [(p.name, p.kind, p.default) for p in inspect.signature(fn).parameters.values()]


@unittest.skipUnless(BHPY_DIR, "BHPY_DIR not set: no blackhole-py checkout to compare against")
class Parity(unittest.TestCase):
    """Our classes must present blackhole-py's own pcie interface (checked against the real file)."""

    def test_same_public_interface_as_blackhole_py(self):
        import ttstation_bhpy
        shim = ttstation_bhpy.install(BHPY_DIR)
        import importlib.util
        spec = importlib.util.spec_from_file_location("_orig_pcie", os.path.join(BHPY_DIR, "pcie.py"))
        orig = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(orig)
        for cls in ("PCIDevice", "TLBWindow", "Sysmem"):
            ours, theirs = getattr(shim, cls), getattr(orig, cls)
            self.assertEqual(_shape(ours.__init__), _shape(theirs.__init__), cls)
            for name, member in vars(theirs).items():
                if name.startswith("_") and name not in ("__enter__", "__exit__"):
                    continue
                self.assertTrue(hasattr(ours, name), f"{cls}.{name} missing")
                if callable(member):
                    self.assertEqual(_shape(getattr(ours, name)), _shape(member), f"{cls}.{name}")
                else:
                    self.assertEqual(getattr(ours, name), member, f"{cls}.{name}")
        # board_config/Allocator are blackhole-py's own (loaded from their file), never re-implemented.
        self.assertTrue(shim.board_config.__code__.co_filename.endswith("pcie.py"))
        self.assertTrue(shim.Allocator.alloc.__code__.co_filename.endswith("pcie.py"))


@unittest.skipUnless(BHPY_DIR, "BHPY_DIR not set")
class DeviceBringUp(BrokerCase):
    def test_pcidevice_comes_up_as_a_p100a_with_blackhole_pys_own_board_config(self):
        self.c.close()                       # one client at a time: PCIDevice opens its own
        del self.c
        import ttstation_bhpy
        shim = ttstation_bhpy.install(BHPY_DIR)
        dev = shim.PCIDevice(sysmem_size=1 << 20)
        try:
            self.assertEqual(dev.card_type, "p100a")
            self.assertEqual(len(dev.cores), 117)               # P100_WORKER_CORES: 120 minus 3 service cores
            self.assertEqual(len(dev.dram_endpoints), 7)
            self.assertEqual(dev.sysmem.noc_addr, 4 << 58)
            with shim.TLBWindow(dev.fd, dev.cores[0]) as w:     # exactly how device.py uses it
                w.target(0)
                w.write(0, 42)
                self.assertEqual(int.from_bytes(w.read(0), "little"), 42)
        finally:
            dev.close()


@unittest.skipUnless(BHPY_DIR, "BHPY_DIR not set")
class BootFrontier(BrokerCase):
    """blackhole-py's own Device().boot() runs every host-side step on this machine, through the
    broker: firmware build (RISC-V toolchain), multicast soft-reset + firmware upload to all tiles,
    per-core images, boot params incl. the iATU-mapped sysmem address, command-queue setup in shared
    memory, GO. It must then stop exactly where the chip has to EXECUTE code (the sim has no RISC-V
    cores): "CQ DRAM engines did not start". Stopping earlier means a host-side regression."""

    def test_boot_reaches_the_point_where_the_chip_must_run_code(self):
        import importlib.util
        if importlib.util.find_spec("numpy") is None:
            self.skipTest("numpy not installed (blackhole-py's requirements)")
        from ttstation_bhpy import toolchain
        if not all(toolchain.apply().values()):
            self.skipTest("no RISC-V toolchain")
        self.c.close()
        del self.c
        import ttstation_bhpy
        ttstation_bhpy.install(BHPY_DIR)
        cwd = os.getcwd()
        os.chdir(BHPY_DIR)
        try:
            import device
            dev = device.Device(sysmem_size=64 << 20)
            with self.assertRaises(TimeoutError) as e:
                dev.boot()
            self.assertIn("CQ DRAM engines did not start", str(e.exception))
            dev.pcie.close()
        finally:
            os.chdir(cwd)


if __name__ == "__main__":
    unittest.main()

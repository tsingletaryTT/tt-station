"""The broker over Tenstorrent's ttsim: a simulated Blackhole that EXECUTES firmware and kernels.

Run: TTSIM_LIB=<ttsim>/src/_out/release_bh/libttsim.so make -C macos/TTStationDriver/bhpy test
(build ttsim with `./make.py :build` in github.com/tenstorrent/ttsim; it builds natively on macOS).
Skipped without TTSIM_LIB.

Checks the parts of our stack ttsim can model faithfully, end to end through the real C broker and
the Python client: identity, libttbh's telemetry walk over ttsim's ARC table, a client TLB window
reaching the ARC and a Tensix tile, and DMA both ways through OUR iATU programming into ttsim's
iATU model. (ttsim's ARC answers TEST with zeros, so the echo is checked on silicon only.)
"""
import os
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from ttstation_bhpy import client as wire, pcie_darwin  # noqa: E402

TTSIM_LIB = os.environ.get("TTSIM_LIB")
BROKER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "build", "ttbh-broker-ttsim")


class _Bump:
    def __init__(self, start, end, alignment=1):
        self.next, self.end, self.alignment = start, end, alignment

    def alloc(self, size, alignment=None):
        off = self.next
        self.next += size
        return off


@unittest.skipUnless(TTSIM_LIB and os.path.exists(BROKER), "TTSIM_LIB not set (or ttbh-broker-ttsim not built)")
class OverTTSim(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.sock = os.path.join(self.tmp, "t.sock")
        self.proc = subprocess.Popen([BROKER, TTSIM_LIB, self.sock], stderr=subprocess.PIPE)
        deadline = time.monotonic() + 20
        while True:
            try:
                self.c = wire.BrokerClient(self.sock)
                break
            except OSError:
                if time.monotonic() > deadline or self.proc.poll() is not None:
                    raise
                time.sleep(0.05)
        pcie_darwin.Allocator = _Bump

    def tearDown(self):
        self.c.close()
        self.proc.terminate()
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc.stderr.close()
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_identity_and_topology_come_from_ttsims_arc(self):
        _, ids, _, _ = self.c.call(wire.HELLO)
        self.assertEqual(ids >> 16, 0xB140)
        self.assertEqual(pcie_darwin.BOARD_TYPES[ids & 0xFFFF], "p150a")        # the board ttsim models
        cols = self.c.call(wire.TELEMETRY, a0=34)[0]
        gddr = self.c.call(wire.TELEMETRY, a0=36)[0]
        self.assertEqual(bin(cols & 0x3FFF).count("1") * 10, 120)              # what blackhole-py demands
        self.assertEqual(bin(gddr & 0xFF).count("1"), 8)

    def test_client_windows_reach_the_arc_and_tensix_l1(self):
        with pcie_darwin.TLBWindow(self.c, (8, 0)) as w:
            w.target(0x80000000)
            self.assertEqual(int.from_bytes(w.read(0x30408), "little") & 1, 1)  # ARC ready for messages
        with pcie_darwin.TLBWindow(self.c, (1, 2)) as t:
            t.target(0)
            t.write(0x10000, b"hello from the Mac broker")
            self.assertEqual(t.read(0x10000, 25), b"hello from the Mac broker")

    def test_dma_both_ways_through_our_iatu_into_ttsims(self):
        m = pcie_darwin.Sysmem(self.c, 1 << 20)
        self.assertEqual(m.noc_addr, 4 << 58)
        with pcie_darwin.TLBWindow(self.c, (2, 0)) as w:                        # ttsim's PCIe tile
            base = m.noc_addr & ~((1 << 21) - 1)
            w.target(base)
            for i in range(16):
                off = 0x40 + i * 0x3000
                a, b = os.urandom(4), os.urandom(4)
                w.write((m.noc_addr - base) + off, a)                            # chip -> host
                self.assertEqual(m.read(off, 4), a)
                m.write(off + 8, b)
                self.assertEqual(w.read((m.noc_addr - base) + off + 8, 4), b)     # host -> chip
        m.close()


if __name__ == "__main__":
    unittest.main()

"""blackhole-py's `pcie` interface on macOS, through the ttbh broker.

blackhole-py (github.com/boopdotpng/blackhole-py) talks to tt-kmd through pcie.py: PCIDevice,
TLBWindow and Sysmem. These classes keep the same constructors, attributes and methods, and do the
work through TTStationDriver instead:

  TLBWindow   window alloc/target/read/write  → broker (libttbh TLB packing, BAR0 windows 0..200)
  Sysmem      mmap + PinPages                  → broker SYSMEM: shm fd + dext PrepareDMA + iATU
  PCIDevice   sysfs card type, telemetry tags  → broker HELLO (subsystem id) + TELEMETRY
              SetPowerState ioctl              → ARC POWER_SETTING message (as tt-kmd sends it)

blackhole-py has no license, so none of its code is copied here. `Allocator`, `board_config` and
the layout constants come from the user's own checkout (see ttstation_bhpy.install).
"""
import ctypes
import mmap
import os
import struct

from . import client as wire

# Filled in by ttstation_bhpy.install() from the user's blackhole-py pcie module.
Allocator = None
board_config = None

# Blackhole board type from the PCI subsystem id (luwen's codes; 0x43 confirmed on a P100A).
# Mirrors libttstation::local_device::board_type_for.
BOARD_TYPES = {0x36: "p100", 0x40: "p150a", 0x41: "p150b", 0x42: "p150c", 0x43: "p100a",
               0x44: "p300b", 0x45: "p300a", 0x46: "p300c", 0x47: "galaxy-blackhole"}

TAG_ENABLED_TENSIX_COL, TAG_ENABLED_GDDR = 34, 36     # what blackhole-py's _read_enabled_masks reads
POWER_SETTING, POWER_VALIDITY = 0x21, 4               # tt-kmd blackhole_set_power_state; pcie.py _validity=4


def _xy(core):
    return (core[0] & 0xFF) | ((core[1] & 0xFF) << 8)


class TLBWindow:
    SIZE = 1 << 21
    USER_ID_LIMIT = 201

    def __init__(self, fd, core):
        # `fd` is the PCIDevice's broker client: blackhole-py passes pcie.fd straight through.
        self.fd, self.core = fd, core
        self.id, _, _, _ = fd.call(wire.TLB_ALLOC)
        self.addr = None      # no raw pointer across processes; blackhole-py never dereferences it

    def target(self, addr, start=None, end=None):
        start = self.core if start is None else start
        end = start if end is None else end
        self.fd.call(wire.TLB_TARGET, self.id, addr, _xy(start), _xy(end))

    def read(self, offset, bytes=4):
        return self.fd.call(wire.TLB_READ, self.id, offset, bytes)[2]

    def write(self, offset, value, bytes=4):
        data = value.to_bytes(bytes, "little") if isinstance(value, int) else bytes_(value)
        self.fd.call(wire.TLB_WRITE, self.id, offset, payload=data)

    def close(self):
        if self.id is not None:
            self.fd.call(wire.TLB_FREE, self.id)
            self.id = None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()


def bytes_(v):
    return v if isinstance(v, (bytes, bytearray)) else bytes(v)


class Sysmem:
    SIZE = 256 << 20
    PAGE_SIZE = os.sysconf("SC_PAGE_SIZE")

    def __init__(self, fd, size=None):
        self.fd = fd
        size = self.SIZE if size is None else size
        self.size = (size + self.PAGE_SIZE - 1) & -self.PAGE_SIZE
        noc, handle, _, mfd = fd.call(wire.SYSMEM, a0=self.size, want_fd=True)
        if mfd is None:
            raise OSError("ttbh broker returned no sysmem fd")
        try:
            self._map = mmap.mmap(mfd, self.size, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
        finally:
            os.close(mfd)      # the mapping keeps the memory alive
        self._anchor = ctypes.c_char.from_buffer(self._map)
        self.addr = ctypes.addressof(self._anchor)
        self.noc_addr, self._handle = noc, handle
        self.allocator = Allocator(0, self.size, self.PAGE_SIZE)

    def alloc(self, size, alignment=None):
        return self.allocator.alloc(size, alignment)

    def read(self, offset, size):
        return ctypes.string_at(self.addr + offset, size)

    def write(self, offset, data):
        ctypes.memmove(self.addr + offset, data, len(data))

    def close(self):
        if self.noc_addr is not None:
            self.fd.call(wire.SYSMEM_FREE, a0=self._handle)   # disable iATU + unmap DMA first
            self.noc_addr = None
        if self.addr is not None:
            del self._anchor
            self._map.close()
            self.addr = None


class PCIDevice:
    def __init__(self, index=0, sysmem_size=None):
        if index != 0:
            raise ValueError("the macOS broker serves one card (index 0)")
        self.fd = wire.BrokerClient()
        self.sysmem = None
        self.powered = False
        try:
            _ver, ids, _, _ = self.fd.call(wire.HELLO)
            subsystem = ids & 0xFFFF
            card_type = BOARD_TYPES.get(subsystem)
            if card_type is None:
                raise RuntimeError(f"unknown Blackhole subsystem id 0x{subsystem:x}")
            tensix_enabled, gddr_enabled = self._read_enabled_masks()
            config = board_config(card_type, tensix_enabled, gddr_enabled)
            self.card_type = config.card_type
            self.tensix_enabled, self.gddr_enabled = tensix_enabled, gddr_enabled
            self.dram_endpoints = config.dram_endpoints
            self.cores = list(config.cores)
            self.prefetch_core = config.prefetch_core
            self.dispatch_core = config.dispatch_core
            self.dram_core = config.dram_core
            self._set_power(0b1111)
            self.powered = True
            self.sysmem = Sysmem(self.fd, sysmem_size)
        except Exception:
            if self.powered:
                try:
                    self._set_power(0)
                except OSError:
                    pass
            self.fd.close()
            self.fd = None
            raise

    def _read_enabled_masks(self):
        tensix = self.fd.call(wire.TELEMETRY, a0=TAG_ENABLED_TENSIX_COL)[0]
        gddr = self.fd.call(wire.TELEMETRY, a0=TAG_ENABLED_GDDR)[0]
        return tensix, gddr

    def _set_power(self, flags):
        header = POWER_SETTING | (POWER_VALIDITY << 8) | (flags << 16)
        self.fd.call(wire.ARC_MSG, payload=struct.pack("<8I", header, *([0] * 7)))

    def close(self):
        if self.fd is not None:
            if self.sysmem is not None:
                self.sysmem.close()
            if self.powered:
                self._set_power(0)
                self.powered = False
            self.fd.close()
            self.fd = None

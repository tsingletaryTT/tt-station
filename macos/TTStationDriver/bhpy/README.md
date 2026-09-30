# bhpy: run blackhole-py on a Mac through TTStationDriver

[blackhole-py](https://github.com/boopdotpng/blackhole-py) is a pure-Python Blackhole runtime (it
runs Llama 3 8B on one card). It talks to tt-kmd on Linux through one file, `pcie.py`. This
directory swaps that file's `PCIDevice` / `TLBWindow` / `Sysmem` for macOS equivalents that go
through tt-station's DriverKit extension. The rest of blackhole-py runs unmodified.

```bash
# once the dext is loaded (see ../README.md):
/Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver serve &     # the broker
PYTHONPATH=macos/TTStationDriver/bhpy python3 -m ttstation_bhpy ~/code/blackhole-py examples/matmul_peak.py
```

## Why a broker

A development-signed dext only opens for clients with the `userclient-access` entitlement, and
`python3` can't have one. So the entitled host app runs `broker.c` on a Unix socket and Python
talks to it, the same approach as tinygrad's TinyGPU:

- **TLB windows** (`TLBWindow.target/read/write`) are proxied. BAR mappings can't be shared
  across processes (task self-ports have been immovable since macOS 12), and blackhole-py never
  dereferences a window pointer.
- **Host memory** (`Sysmem`) is shared, not proxied. The broker creates an shm fd, the dext
  DMA-maps those pages (`PrepareDMA`), and the iATU is pointed at them. The client receives the
  fd via `SCM_RIGHTS` and maps the same pages, so weights and command queues never cross the socket.
- **`SetPowerState`** becomes the ARC `POWER_SETTING` message tt-kmd would send. The broker also
  sends tt-kmd's device-init `ASIC_STATE0` at startup.

All the chip logic is `../libttbh`, verified against tt-kmd on real silicon.

## Licensing

blackhole-py has **no license**, so none of its code is copied here. `ttstation_bhpy.install()`
loads `Allocator`, `board_config` and the layout constants from *your* checkout.

## Tests (no hardware, macOS or Linux)

```bash
make -C macos/TTStationDriver/bhpy test                       # 10 tests
BHPY_DIR=~/code/blackhole-py make -C macos/TTStationDriver/bhpy test   # + interface parity + PCIDevice bring-up
```

The Python client runs against the **real C broker** built over libttbh's simulated Blackhole
(`ttbh-broker-sim`). The sim decodes TLB registers with an independent bitfield struct, has a fake
ARC firmware, and hands out deliberately fragmented fake IOVAs. So the wire protocol, fd passing,
TLB packing, telemetry, ARC messages and the iATU plan are exercised end to end. Only the dext
and the silicon are swapped out.

Supported cards are blackhole-py's: `p100a` and `p150a/b/c`. The QuietBox's `p300c` isn't one of
them.

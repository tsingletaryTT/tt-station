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

## Prerequisites (macOS)

```bash
brew install llvm riscv-gnu-toolchain       # Apple's clang has no RISC-V backend
python3 -m venv .venv && .venv/bin/pip install -r ~/code/blackhole-py/requirements.txt
PYTHONPATH=macos/TTStationDriver/bhpy .venv/bin/python -m ttstation_bhpy doctor ~/code/blackhole-py
```

`python -m ttstation_bhpy` points blackhole-py at a RISC-V-capable toolchain automatically
(`CC`, `TT_RISCV_LD`, `TT_RISCV_OBJCOPY`; anything you set yourself wins). blackhole-py's own
default is the first `clang` on PATH, which on a Mac is Apple's, and it can't target RISC-V.
Homebrew's binutils are named `riscv64-unknown-elf-*`, not Linux's `riscv64-linux-gnu-*`.

**What works today, with no dext:** on this Mac, against the simulated broker, blackhole-py's own
`Device().boot()` completes every host-side step:

1. builds its firmware with the Homebrew toolchain;
2. broadcasts soft-reset and resident firmware to all tiles;
3. uploads the per-core images;
4. writes the boot parameters, including the iATU-mapped sysmem address;
5. builds the command queue and sends GO.

It then stops exactly where the chip has to *run* code ("CQ DRAM engines did not start"). A test
guards that point.

## Against a simulated Blackhole that runs code: ttsim

[ttsim](https://github.com/tenstorrent/ttsim) (Apache-2.0) is Tenstorrent's full-system simulator.
It presents a Blackhole as a virtual PCIe device (config space, BAR accesses, `libttsim_clock`, DMA
callbacks), which is the same contract the dext gives the broker, and it builds natively on macOS
(`./make.py :build` gives `src/_out/release_bh/libttsim.so`, Mach-O arm64, in about 6 s).
`ttbh-broker-ttsim` runs the broker on it:

```bash
make -C macos/TTStationDriver/bhpy build/ttbh-broker-ttsim
TTSIM_LIB=~/code/ttsim/src/_out/release_bh/libttsim.so make -C macos/TTStationDriver/bhpy test
```

Verified through it, end to end through the real C broker and the Python client:

- **Identity and topology:** ttsim models a P150 (120 cores, 8 GDDR), read by libttbh's telemetry walk.
- **Windows:** client TLB windows reach the ARC (boot status `0x5`) and Tensix L1.
- **DMA:** in both directions, through our iATU programming into ttsim's iATU model. A deliberately
  wrong iATU target turns the test red.
- **blackhole-py's own `boot()`:** runs its firmware on ttsim's RISC-V cores for over 6 million
  clocks, then ttsim stops with `UnsupportedFunctionality: tensix_cfg_wr32: reg=4`. blackhole-py's
  hand-written firmware touches a Tensix config register that ttsim (validated against tt-metal)
  doesn't model. That's a gap between those two projects, not in this stack.

ttsim quirks the backend absorbs (all in `broker_ttsim.c`, none changing what silicon gets, apart
from one convention change that is valid on both):

- it doesn't implement the strided-TLB registers, so those writes are dropped;
- its TLB config registers are write-only, so the posted-write flush read is answered locally;
- it rejects unicast TLBs with start coordinates set. The broker now uses tt-kmd's own convention
  (start = 0 for unicast) on every backend, which is valid on silicon too;
- it answers the ARC `TEST` message with zeros. The echo was verified on silicon instead.

## Why a broker

The dext serves one user client at a time. A client owns the BAR mappings, TLB windows and iATU,
and two would silently overwrite each other. And in a shipping build only an app with the
`userclient-access` entitlement may open it, which `python3` can't have. (The development build
uses `allow-any-userclient-access` instead, because `userclient-access` needs Apple's approval on a
new team.) So one process, `TTStationDriver serve` or the standalone `ttbh-broker-iokit`, owns the
dext and runs `broker.c` on a Unix socket, and everything else talks to it. That's the same
approach as tinygrad's TinyGPU:

- **TLB windows** (`TLBWindow.target/read/write`) are proxied. BAR mappings can't be shared
  across processes (task self-ports have been immovable since macOS 12), and blackhole-py never
  dereferences a window pointer.
- **Host memory** (`Sysmem`) is shared, not proxied. The broker creates an shm fd, the dext
  DMA-maps those pages (`PrepareDMA`), and the iATU is pointed at them. The client receives the
  fd via `SCM_RIGHTS` and maps the same pages, so weights and command queues never cross the socket.
- **`SetPowerState`** becomes the ARC `POWER_SETTING` message tt-kmd would send. The broker also
  sends tt-kmd's device-init `ASIC_STATE0` at startup.

All the chip logic is `../libttbh`, verified against tt-kmd on real silicon.

### Sessions

The session socket (`~/Library/Application Support/TTStation/ttbh.sock`, or `serve SOCKET`) serves
up to 16 sessions at once, like several opens of `/dev/tenstorrent/N` under tt-kmd. tt-flash needs
this, because it reopens the card in-process and from worker processes. The broker is
single-threaded and handles each request whole. TLB windows and outbound iATU regions are owned
broker-wide, so two sessions are never given the same one, and a session that disconnects has only
what it held released. Coordinating use of the chip itself is up to the clients, as on Linux.

### Telemetry socket (read-only, any number of readers)

`telemetry.sock`, beside the session socket (or `TTBH_TELEMETRY_SOCKET`), is for monitors. Connect,
read one JSON line, and the broker closes the connection:

```json
{"v":1,"device_id":45376,"subsystem_id":67,"arc_ready":true,"asic_temp_c":56.4,"power_w":38,
 "vcore_mv":740,"current_a":51,"aiclk_mhz":800,"heartbeat":48141,"fan_rpm":null}
```

A field is `null` when its read failed. Readers are answered between session requests, through
the broker's own TLB window, and nothing is written to the card. So a monitor works while a model
or a flash runs.

### tt-toplike

`python3 -m ttstation_bhpy.toplike_bridge [--port 8766]` serves `tt-smi -s`-shaped frames on
`ws://127.0.0.1:8766/telemetry`, built from `tt-station local` (identity) and `telemetry.sock`
(readings). tt-toplike's existing remote mode then works unchanged:
`tt-toplike --remote 127.0.0.1:8766 --mode arcade`.

### The standalone broker

`make -C macos/TTStationDriver/bhpy build/ttbh-broker-iokit` builds the same broker as
`TTStationDriver serve`, as a plain ad-hoc-signed binary. The development dext accepts any client,
so this is the quick way to test a broker change on the card without reinstalling the host app.

### DMA-able host memory is capped at about 2 GiB

The Mac maps at most **2,146,385,920 bytes** for the device, in total across all mappings
(measured: a second 1 GiB buffer fails while the first is held, with `kIOReturnNoMemory`).
blackhole-py's Llama 3 8B asks for exactly 2 GiB, so `Sysmem` retries in 16 MiB steps, never
below 3/4 of the request, and prints the size it got. blackhole-py sizes its staging area from the
allocator and raises `MemoryError` if a transfer doesn't fit.

### pyluwen, tt-flash and the official tools

Tools built on pyluwen (tt-flash, and in principle tt-smi) run against the broker through a small
luwen fork: see `../luwen/README.md` and `docs/reference/macos-firmware-update.md`.

## Licensing

blackhole-py has **no license**, so none of its code is copied here. `ttstation_bhpy.install()`
loads `Allocator`, `board_config` and the layout constants from *your* checkout.

## Tests (no hardware, macOS or Linux)

```bash
make -C macos/TTStationDriver/bhpy test                       # sim broker: protocol, sessions, sysmem, telemetry socket
BHPY_DIR=~/code/blackhole-py make -C macos/TTStationDriver/bhpy test   # + interface parity + PCIDevice bring-up
TTSIM_LIB=~/code/ttsim/src/_out/release_bh/libttsim.so make -C macos/TTStationDriver/bhpy test   # + ttsim (checked on v1.13.0)
```

The Python client runs against the **real C broker** built over libttbh's simulated Blackhole
(`ttbh-broker-sim`). The sim decodes TLB registers with an independent bitfield struct, has a fake
ARC firmware, and hands out deliberately fragmented fake IOVAs. So the wire protocol, fd passing,
TLB packing, telemetry, ARC messages and the iATU plan are exercised end to end. Only the dext
and the silicon are swapped out.

Supported cards are blackhole-py's: `p100a` and `p150a/b/c`. The QuietBox's `p300c` isn't one of
them.

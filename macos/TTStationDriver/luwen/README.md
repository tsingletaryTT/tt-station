# luwen on macOS, through the broker

[luwen](https://github.com/tenstorrent/luwen) is the Rust library under pyluwen, and pyluwen is what
tt-flash (and tt-smi) use to reach a card. On Linux it opens `/dev/tenstorrent/N` and talks to
tt-kmd. macOS has no tt-kmd, so this patch adds a backend that reaches the card through
tt-station's broker (`../bhpy/broker.c`). With it, **tt-flash 4.1.0 from PyPI runs unmodified on a
Mac** and flashed a P100A from firmware 19.2.0 to 19.16.0 (2026-10-22,
`docs/journey/egpu-p100-on-a-mac.md`).

`0001-*.patch` applies to luwen `ecef1a8` (`bh-mod-v2.4.0`, the base of pyluwen 0.10.0). It changes:

- **`bind/pyluwen/src/ttstation_broker.rs`** (new): a luwen chip backend over the broker's wire
  protocol. It's switched on by `TTSTATION_BROKER` (`1` for the default socket, or a socket path).
  Blackhole needs only NOC reads and writes at the ARC tile, which go through one 2 MiB TLB
  window. Every access is widened to an aligned 32-bit MMIO access: on an M-series Mac, a 3-byte
  read of `ARC_MISC_CNTL` returned `ffffff` where the 4-byte read returned `00000000`.
- **`bind/pyluwen/src/lib.rs`**: `PciChip`, `pci_scan` and the `detect_chips*` functions use the
  backend when it's switched on. Blackhole's `pci_board_type`/`pci_interface_id` fall back to the
  backend's device info.
- **`crates/luwen-api/src/chip/blackhole/message.rs`** (ARC message queue). These changes apply on
  any platform:
  - ring the MSI catcher FIFO (`0x800B0000`) as well as `irq0_trig`. tt-kmd rings the FIFO, and
    firmware 19.2.0 wakes only on it;
  - drain replies nobody collected before sending, so a late reply to a timed-out request isn't
    taken as the answer to the next one (tt-kmd drains too);
  - when `TTSTATION_BROKER` is set, hold a file lock (`arc-msg.lock` beside the socket) for the
    whole exchange. Without it, concurrent processes on one queue got each other's replies.

## Build and use

```bash
git clone https://github.com/tenstorrent/luwen ~/code/luwen && cd ~/code/luwen
git checkout ecef1a8 && git am ~/code/tt-station/macos/TTStationDriver/luwen/0001-*.patch
uv venv -p 3.12 ~/tenstorrent/.venv-flash
cd bind/pyluwen && VIRTUAL_ENV=~/tenstorrent/.venv-flash maturin develop --release
uv pip install -p ~/tenstorrent/.venv-flash/bin/python tt-flash==4.1.0   # keeps this pyluwen 0.10.0
```

With the broker running, `TTSTATION_BROKER=1 ~/tenstorrent/.venv-flash/bin/python -c "import
pyluwen; print(pyluwen.pci_scan())"` prints `[0]`. The firmware procedure is in
`docs/reference/macos-firmware-update.md`.

None of this is upstream. The doorbell, drain and alignment changes would help any luwen user and
are candidates for an upstream PR; nothing has been filed.

# TTStationDriver — experimental macOS driver for a Blackhole card over Thunderbolt

> **Experiment** (branch `experiments/egpu`). It builds but has not yet been loaded against hardware.
> Design, milestones and risks: [`docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md`](../../docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md).

A PCIDriverKit extension (a "dext") that claims a Tenstorrent Blackhole (`1e52:b140`, P100/P150)
in a Thunderbolt enclosure. It hands the card's BARs to userspace. It is adapted from tinygrad's
[TinyGPU](https://github.com/tinygrad/tinygrad/tree/master/extra/usbgpu/tbgpu) (MIT).

| Path | What |
|---|---|
| `Driver/` | the dext: `TTBlackholeDriver` (matches + opens the device) and `TTBlackholeUserClient` (BAR mapping, GetInfo, CfgRead) |
| `Shared/TTBlackholeABI.h` | the user-client contract (selectors, info slots, memory types, register offsets), shared by both sides |
| `Host/` | headless `TTStationDriver.app`: embeds the dext and runs `install` / `uninstall` / `status` |
| `Probe/bh_probe.c` | `tt-station-bh-probe`, a read-only smoke test (spec milestone M1) |
| `scripts/install-dev.sh` | build → ad-hoc sign → `/Applications` → activate |

## Build (no SIP change needed)

```bash
cd macos/TTStationDriver && scripts/install-dev.sh --build
```

## Load it (needs SIP disabled, or a successful `--force` try)

```bash
scripts/install-dev.sh
build/Debug/tt-station-bh-probe
log stream --predicate 'eventMessage CONTAINS "ttbh:"'
```

Uninstall: `/Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver uninstall`.

## Safety

- The dext only matches `1e52:b140`, and `Start` re-checks the IDs before touching anything.
- v1 RPCs are read-only. Bus mastering stays **off** until DMA exists, so the chip cannot write
  to host memory.
- The probe only reads (TLB register 0 and iATU region 0). Nothing here programs a TLB or talks
  to the NOC. That is milestone M2.

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
| `Probe/bh_probe.c` | the read-only M1 smoke test: standalone `tt-station-bh-probe`, and also linked into the host app as `TTStationDriver probe` |
| `libttbh/` | dependency-free C for talking to the chip once BAR0 is mapped: TLB register packing, NOC reads/writes, ARC boot status, and the telemetry tag-table walk. Shared by the probe (`--noc`); see below |
| `scripts/install-dev.sh` | build → sign (`--team ID` development, or ad-hoc) → `/Applications` → activate |

## Build (no SIP change needed)

```bash
cd macos/TTStationDriver && scripts/install-dev.sh --build
```

## Load it with SIP ON (development-signed, the preferred route)

This needs a **paid** Apple Developer team signed in to Xcode (Settings > Accounts). No reboot.
You approve it once in System Settings.

```bash
scripts/install-dev.sh --team <TEAMID>
/Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver probe
/usr/bin/log stream --predicate 'eventMessage CONTAINS "ttbh:"'
```

Use `TTStationDriver probe`, not the standalone binary. A development-signed dext only lets
in clients that carry `userclient-access`, and only the app can carry that entitlement (a bare
CLI tool can't embed a provisioning profile).

Note the full path `/usr/bin/log`: zsh has a `log` builtin that shadows it.

## First light: the whole sequence in one command

```bash
scripts/first-light.sh --team <TEAMID> --bhpy ~/code/blackhole-py --python .venv/bin/python
scripts/first-light.sh --sim --bhpy ~/code/blackhole-py --python .venv/bin/python   # rehearsal, today
```

It runs, in order and stopping at the first failure: environment → card on the bus → dev-signed
install (waits for your approval) → driver attached → `probe` → `probe --noc` → `probe --dma` →
`tt-station local` → broker + doctor → matmul compiled → matmul **run and validated on the card**.
Every step's output is appended to `docs/journey/egpu-p100-on-a-mac.md` (the rehearsal logs to
`$TMPDIR/first-light-sim.md` unless you pass `--log`).

## Load it with SIP OFF (ad-hoc)

```bash
scripts/install-dev.sh
```

With SIP on this route cannot work, and it fails in two places:

- `systemextensionsctl developer on` refuses to run.
- AMFI kills the ad-hoc host app outright (`Code=-424 "The file is adhoc signed but contains
  restricted entitlements"`, exit 137). Even `status` dies. So on a SIP-on Mac, an app built
  with `--build` is only good for inspecting.

Uninstall: `/Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver uninstall`.

## libttbh: the M2 logic, written and verified before the dext can load

`probe --noc` (`TTStationDriver probe --noc`) aims the driver's 2 MiB window (index 201) at the ARC
processor and reads its boot status and live telemetry (temperature, power, vcore, AICLK). All of
that logic lives in `libttbh/` and is checked without the signing key:

```bash
make -C libttbh test    # bit-packing vs hand vectors + an independent bitfield decoder, in a simulated BAR0
```

On a Linux box with tt-kmd (a QuietBox), `make -C libttbh kmd-check` builds `ttbh-kmd-check`. It
runs the same ARC/telemetry code on real silicon, with tt-kmd only aiming the window, and compares
each value with tt-kmd's own hwmon/sysfs readings. Run it under a gozer lease.

GNU make 3.81 (the macOS default) compares mtimes to the whole second. After swapping a source file
back and forth quickly (for example in a mutation test), use `make -B` or it may run a stale binary.

`probe --dma` (spec M3) has the dext DMA-map a 64 KiB buffer, programs outbound iATU regions
through BAR2, and runs a chip↔host loopback over the NOC. `make -C libttbh kmd-check` plus
`ttbh-kmd-check --dma` is the same sequence on a QuietBox. There, the iATU encoder is also compared
bit for bit against the registers tt-kmd wrote.

## Safety

- The dext only matches `1e52:b140`, and `Start` re-checks the IDs before touching anything.
- v1 RPCs are read-only. Bus mastering stays **off** until DMA exists, so the chip cannot write
  to host memory.
- By default the probe only reads (TLB register 0 and iATU region 0). `--noc` additionally programs
  exactly one TLB register (window 201) to read the ARC. That's milestone M2, and it's opt-in.
- Bus mastering stays off until the first `PrepareDMA`. `--dma` disables its iATU regions and
  completes its mapping before exiting, and the dext completes any mappings a client leaves behind.

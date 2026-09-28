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

## Safety

- The dext only matches `1e52:b140`, and `Start` re-checks the IDs before touching anything.
- v1 RPCs are read-only. Bus mastering stays **off** until DMA exists, so the chip cannot write
  to host memory.
- The probe only reads (TLB register 0 and iATU region 0). Nothing here programs a TLB or talks
  to the NOC. That is milestone M2.

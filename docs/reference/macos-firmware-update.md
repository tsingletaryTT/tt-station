# Updating Blackhole firmware from a Mac

How the P100A in a Razer Core X V2 was updated from firmware bundle 19.2.0 to 19.16.0 on
2026-10-22, with Tenstorrent's own tt-flash running unmodified on macOS. The full story, with
every output, is in `docs/journey/egpu-p100-on-a-mac.md`.

## Where the official tools fit

- **tt-system-firmware** publishes the bundles: `fw_pack-<version>.fwbundle` (every board) and
  per-board ones such as `p100a.fwbundle`. Both hold the same `P100A-1` directory.
- **The `tt` CLI** has no firmware command. On Linux, `tt update` runs Tenstorrent's installer,
  which downloads `fw_pack-<golden>.fwbundle` and runs `tt-flash flash` on it. That installer needs
  Linux (apt or dnf, and sudo), so on a Mac you run the same tt-flash step yourself. `tt`'s golden
  pin tells you which version it would have picked.
- **tt-flash** does the flashing. It reaches the card through pyluwen, and on a Mac that means
  the broker backend in `macos/TTStationDriver/luwen/`.

## Before you start

1. The dext is installed and attached (`tt-station local` shows `driver: TTStationDriver`).
2. The broker is running and nothing else is using the card. Stop blackhole-py, the tt-toplike
   bridge and so on. The broker itself is `TTStationDriver serve` or `bhpy/build/ttbh-broker-iokit`.
3. pyluwen with the broker backend and tt-flash are installed (`macos/TTStationDriver/luwen/README.md`).
4. The enclosure is on stable mains power. A cut during the write needs SWD hardware to recover.

## Steps

```bash
# 1. Download the bundle and check it against the release's published sha256
gh release download v19.16.0 --repo tenstorrent/tt-system-firmware -p fw_pack-19.16.0.fwbundle

# 2. Read-only checks: what's running, what tt-flash sees, whether the bundle fits this board
TTSTATION_BROKER=1 python -c "from tt_flash import chip; d=chip.detect_chips()[0]; print(d, chip.resolve_board_type(d), d.get_bundle_version())"

# 3. Back up the SPI flash (16 MiB in about 10 s through the broker)
TTSTATION_BROKER=1 python -c "import pyluwen; c=pyluwen.PciChip(0).as_bh(); b=bytearray(16<<20); c.spi_read(0,b); open('spi-backup.bin','wb').write(b)"

# 4. Flash, without tt-flash's Linux-only reset
TTSTATION_BROKER=1 tt-flash flash fw_pack-19.16.0.fwbundle --no-reset
```

5. Stop the broker, then power-cycle the enclosure. On 2026-10-22 the card came back on PCIe only
   after the Thunderbolt cable was **replugged**, so if it doesn't reappear in a minute or two,
   replug the cable.
6. Start the broker and read telemetry tag 28. tt-flash's `get_bundle_version()` should report the
   new version both running and in the SPI.

## What to expect

- tt-flash checks the board (`P100A-1`) and the version, then the bundle's compatibility variables.
  Firmware older than about 19.15 can't report the SPI part ID (tag 80), so that check shows as
  **unchecked** and the flash goes ahead. Linux behaves the same way. Afterwards the new firmware
  reports it, and the check passes.
- When the layout differs from the bundle's, tt-flash rewrites the boot-critical images (`cmfw`,
  `safeimg`, `failover`), even without `--update-boot-images`. It said so, and the write verified.
- The board's own configuration (`boardcfg`, the `ccfgovr` banks) is preserved. The board ID
  survives.
- The board management controller (DMFW) updates itself on the first boot.

## Recovery

Normal flashing never touches the recovery image MCUBoot keeps in the SPI. If the mission image
is bad, the board boots the recovery one. If the boot path itself is lost, tt-system-firmware's
`scripts/tooling/blackhole_recovery/recover-blackhole.py` reflashes it over SWD with an ST-Link.
The SPI backup from step 3 is the board's exact previous contents.

# macOS Blackhole driver (DriverKit dext) — design

**Status:** experiment, branch `experiments/egpu`. Skeleton landed 2026-09-28; nothing below
milestone M1 has run on hardware yet.

## Goal

**North star (owner, 2026-09-28):** tt-station on a Mac detects the attached TT device, then
invokes the official **`tt` CLI / tt-model-manager** to deploy a model **right-sized for that
device**. Here that means one small P100. tt-station orchestrates; the official tooling
serves (see the naming rule in `CLAUDE.md`: prefer delegating to `tt` over our own). We get
there in small steps, and every milestone below should leave something runnable.

This document covers the first leg: drive a Tenstorrent **Blackhole** card (P100/P150, PCI
`1e52:b140`) in a Thunderbolt enclosure directly from an Apple Silicon Mac, with no Linux box
and no QuietBox. That gives tt-station a *local* device alongside the remote boxes it already
manages.

Non-goals (for now): Wormhole, multi-card, running tt-metal/TTNN unmodified, shipping to
users. This is a feasibility experiment.

## Why this is possible now

Observed on the owner's MacBook Pro (macOS 26, TB5) with a P100 in a **Razer Core X V2**
(USB4 v2), via `ioreg -r -n 'pci1e52,b140' -l`:

| Property | Value | Meaning |
|---|---|---|
| `IOName` / `compatible` | `pci1e52,b140`, `pci1e52,43`, `pciclass,120000` | Blackhole, subsystem 0x43 (P100), class 0x12 = processing accelerator |
| `IOPCITunnelled` | Yes | Reached through a Thunderbolt PCIe tunnel |
| `IOPCIExpressLinkStatus` | 0x1044 | Gen4 (16 GT/s) × 4 |
| `IODeviceMemory` | 512 MiB, 1 MiB, **16 B** | BAR0, BAR2, and **BAR5** (decoded from `assigned-addresses`, config offset 0x24). BAR4 is not assigned at all (risk R1) |
| `IOPCIDeviceMapperPageSize` | 16384 | DART IOMMU uses 16 KiB pages |
| `IOServiceDEXTEntitlements` | `driverkit.transport.pci` | macOS will hand this device to a PCIDriverKit dext |
| children | none | nothing has claimed it |

macOS already does the part **Asahi Linux cannot**: PCIe tunnelling over Thunderbolt. (The
2025 "tt-metal on Asahi" bounty, tenstorrent/tt-metal#18296, was packaging-only; tt-kmd#108
was closed as non-functional because Asahi has no PCIe. Asahi's 2026 USB4 patches still
exclude PCIe tunnels.)

## Prior art we are building on

- **tinygrad TinyGPU** — `tinygrad/tinygrad` `extra/usbgpu/tbgpu/` (MIT). An Apple-approved
  PCIDriverKit dext that runs NVIDIA/AMD GPUs over Thunderbolt on Apple Silicon. The design
  is: dext `Start` opens the `IOPCIDevice` and enables memory space; an `IOUserClient`
  exposes BARs via `CopyClientMemoryForType(type = bar index)` →
  `_CopyDeviceMemoryWithIndex`, so userspace does `IOConnectMapMemory64` and pokes MMIO
  directly; `ExternalMethod` does config reads/writes, reset, and DMA prep
  (`IODMACommand`, `maxAddressBits = 40`, ≤32 segments returned as `[addr,len]` pairs).
  **Our skeleton is a direct adaptation of this design.** The shipped TinyGPU app will not
  claim our card (its entitlement is scoped to vendors 0x10de/0x1002, and it matches
  display class 0x03).
- **boopdotpng/blackhole-py** — pure-Python Blackhole runtime (Linux) that runs Llama 3 8B
  on one card using only a handful of tt-kmd ioctls: PIN_PAGES/UNPIN_PAGES,
  ALLOCATE/CONFIGURE/FREE_TLB (2 MiB windows mmapped from the device), SET_POWER_STATE.
  That surface is the natural target for our userspace shim (M3).
- **tenstorrent/tt-kmd** `blackhole.c` — the spec for register layout:
  - BAR0: 202 × 2 MiB TLB windows; TLB config regs at `0x1FC00000` (12 B each:
    low32/mid32/high32); NOC2AXI config at `0x1FD00000`. Window 201 is the kernel's own.
  - BAR2: iATU at `0x1000` (16 outbound regions, stride 0x100) — how the chip reaches host memory.
  - BAR4: up to 8 × 4 GiB windows; count = `BAR4 length / 4 GiB`, so a small BAR4 just means zero 4G windows.
  - ARC is NOC (8,0); `RESET_SCRATCH(n) = 0x80030400 + 4n`; `ARC_BOOT_STATUS = RESET_SCRATCH(2)`, bit0 = ready for messages.
  - Uses MSI; recommends IOMMU translation on (no hugepages / passthrough needed).
- **tenstorrent/tt-umd#3411** (2026-09) — native macOS UMD builds against ttsim, with a
  Darwin `tt-kmd-lib` stub returning `-ENOTSUP`. Eventual seam for plugging this driver
  into the official stack (M5).
- Other dext references: `b-ostrov/MelonDMA` (ConnectX RDMA), `tech2077/litepcie-macos-driver`
  (FPGA BAR+DMA), `lokm01/ThunderLlamaX` (`docs/DEXT_LAWS.md` — practical dext limits).

## Architecture

```
┌ TTStationDriver.app  (/Applications, host app — activates the dext)
│   └ Contents/Library/SystemExtensions/com.tenstorrent.ttstation.driver.dext
│        TTBlackholeDriver        : IOService       — matches 1e52:b140, opens IOPCIDevice
│        TTBlackholeUserClient    : IOUserClient    — BAR mapping + RPCs
│
└ userspace client (IOKit: IOServiceOpen → IOConnectCallScalarMethod / IOConnectMapMemory64)
     M1: tt-station-bh-probe (C)   — read-only smoke test
     M3: blackhole-py backend / tt-umd Darwin backend
```

### User-client contract (v1, skeleton)

| Mechanism | Selector / type | In | Out | Notes |
|---|---|---|---|---|
| `ExternalMethod` | 0 `GetInfo` | — | vendor, device, subsys vendor, subsys id, BAR0/2/4 sizes, ABI version | sizes via `GetBARInfo`; answers risk R1 |
| `ExternalMethod` | 1 `CfgRead` | offset, width (1/2/4) | value | read-only; bounds-checked to 4 KiB |
| `ExternalMethod` | 2 `PrepareDMA` *(v2)* | struct input = the buffer (>4 KiB → memory descriptor) | dma id, segment count; struct output = `TTBHDMASegment[]` | first call enables bus mastering; ≤16 segments (one iATU region each) |
| `ExternalMethod` | 3 `CompleteDMA` *(v2)* | dma id | — | unmaps; `Stop` completes any leftovers |
| `CopyClientMemoryForType` | type = 0, 2, 4 | — | BAR memory | map with `IOConnectMapMemory64(conn, bar, …)` |

Deliberately **not** in v1: config writes, reset, DMA. Writes to config space and resets
over a TB tunnel are the riskiest operations and nothing needs them to prove M1.

## Milestones

- **M0 — skeleton builds** *(done 2026-09-28)*: dext + host app + probe compile with the
  DriverKit 27 SDK via xcodegen (universal arm64/x86_64). The dext is embedded at
  `Contents/Library/SystemExtensions/`. `install-dev.sh --build` ad-hoc signs with the dev
  entitlements. Verified without hardware: the probe reports "no service" and exits 1; the host
  app refuses to install from outside `/Applications`.
- **M1 — dext attaches, BARs map (read-only):** install development-signed with SIP on (`install-dev.sh --team`, see Dev install); `systemextensionsctl
  list` shows `[activated enabled]`; `ioreg` shows our service under `pci1e52,b140`;
  `TTStationDriver probe` prints vendor/device, the three BAR sizes, and reads TLB register 0
  from BAR0 `+0x1FC00000`.
- **M2 — first NOC read:** *(logic written and verified 2026-09-29, before the dext can load:
  `libttbh` packs TLB registers (checked against hand vectors and an independent bitfield decoder in
  a simulated BAR0, and seen to fail under two encoder mutations). Its ARC and telemetry code was run
  on a real Blackhole in qb2-lab through tt-kmd, and it agreed with tt-kmd's hwmon on temperature,
  power, vcore, current and AICLK, with a live heartbeat. What remains is running the same code
  through the dext's BAR0: `TTStationDriver probe --noc`.)* program one 2 MiB TLB window (as tt-kmd does with window 201) to
  ARC (8,0) at `0x80000000`, read `ARC_BOOT_STATUS` (`0x80030408`), expect bit0 = 1. That
  proves MMIO → NOC works end to end over Thunderbolt. First *write* to the device.
- **M3 — DMA + blackhole-py:** *(DMA half written 2026-09-29. On silicon via tt-kmd: libttbh found
  the active PCIe tile (x = 11), its iATU encoder matched all 9 registers tt-kmd programmed,
  and a chip→host NOC write landed in host memory in ~6 µs, with a host→chip read-back. One
  hardware fact surfaced: `UPPER_LIMIT` keeps only 8 bits (limit bits 32–39), so an iATU region
  must not cross a 1 TiB boundary, and `ttbh_dma_plan` enforces that. On the Mac, the dext gained
  `PrepareDMA`/`CompleteDMA` (ABI v2; bus mastering switches on at the first mapping, and
  mappings are cleaned up in `Stop`). `TTStationDriver probe --dma` replays the proven
  loopback. **blackhole-py backend written the same day** (`macos/TTStationDriver/bhpy/`): a broker
  in the entitled host app (`TTStationDriver serve`; windows proxied, host memory shared through an
  shm fd plus the dext's PrepareDMA plus iATU; power via ARC messages) and a Python drop-in for
  blackhole-py's `pcie` module. It's tested end to end against the real C broker over libttbh's
  simulated chip, including blackhole-py's own `board_config` bringing up a P100A. **ttsim backend (2026-09-30):** the broker also runs over Tenstorrent's ttsim (built natively on
  macOS). Identity, telemetry, windows and DMA both ways through our iATU into ttsim's are verified
  and tested. blackhole-py's `boot()` runs its firmware on ttsim for over 6 million clocks, until
  ttsim hits a Tensix config register it doesn't model (`reg=4`). ARC messages are verified on
  silicon too (2026-09-30, qb2-lab: 16/16 TEST echoes through libttbh's ring code; the real queue
  has 4 entries). Running a real kernel waits on the dext loading.)* add `PrepareDMA` (IODMACommand, single segment, respect 16 KiB
  pages) and host-buffer mapping through the iATU; add a Darwin backend to blackhole-py
  replacing its tt-kmd ioctls with user-client calls. Target: a matmul, then Llama 3 8B.
- **M4 — tt-station sees the local card:** *(first half done 2026-09-28, with no driver:
  `tt-station local` reads the IORegistry, names the card `p100a` → device config `p100`, and
  delegates right-sizing to the official `tt model list --hw p100`. That returns Llama-3.1-8B and
  Llama-3.1-8B-Instruct, both EXPERIMENTAL. ARC telemetry still needs the dext.)* `tt-station` reports the local card (identity,
  and ARC telemetry like tt-smi) as a device class, e.g. `P100 x1`, the same way it reports a
  box's `device_mesh`. It feeds that class into the existing hardware-aware model catalog, so
  "runs on this device" is computed for a P100.
- **M5 — deploy through the official tooling:** tt-station invokes the `tt` CLI /
  tt-model-manager to deploy a model right-sized for the detected device. This needs the
  official stack to reach the card, either through tt-umd's Darwin backend
  (tt-umd#3411's stub, filled in with our user client) or through whatever device seam
  tt-model-manager exposes. **To investigate before M5:** what tt-model-manager is, where it
  lives, and how it chooses a device. Nothing here assumes its interface yet.
- **M6 — shipping:** request `com.apple.developer.driverkit.transport.pci` scoped to
  `IOPCIPrimaryMatch 0x00001e52&0x0000FFFF` + `driverkit.userclient-access` for the host
  app from Apple under Tenstorrent's developer account.

## Dev install

Tested 2026-09-28 on this Mac, with SIP **on**:

| Route | Result |
|---|---|
| `systemextensionsctl developer on` | **Dead.** "this tool cannot be used if System Integrity Protection is enabled" |
| Ad-hoc sign (`install-dev.sh` / `--build`) | **Dead with SIP on.** AMFI kills the host app: `Code=-424 "adhoc signed but contains restricted entitlements"` (exit 137). |
| Development signing from a **paid** team (`install-dev.sh --team ID`) | **Expected to work**; not yet tried because there's no team on this Mac (`security find-identity` found 0 identities). Apple DTS: DriverKit development entitlement variants are "available on all paid developer accounts without any special approval" and "allow a DEXT to match against any hardware". Xcode 16+ automatic signing covers PCI for development. ([forum thread 809202](https://developer.apple.com/forums/thread/809202)) |
| SIP off + ad-hoc | Works per TinyGPU's `install_nosip.sh`; the owner prefers to avoid this. |

With development signing, the dext carries `TTBlackholeDriver.Team.entitlements`: no
`allow-any-userclient-access`, so clients get in through the host app's `userclient-access`.
That is why the probe is also linked into the host app (`TTStationDriver probe`).

**Fallback that needs no driver at all:** tinygrad's USB path
(`tinygrad/runtime/support/usb.py`, `CustomASM24Controller`) sends raw PCIe config and memory
TLPs to a device behind an **ASMedia ASM2464PD** USB4/USB3-to-PCIe bridge through libusb.
There's no dext, no signing and no SIP change. The cost is a different enclosure (the Razer
uses an Intel Thunderbolt controller, not an ASM2464), tinygrad's patched bridge firmware
(`extra/usbgpu/patch.py`), slow MMIO, and DMA limited to the bridge's small internal buffer.
That's enough for M1/M2 register pokes, not for M3+. Unverified against a Blackhole.

## Possible paths forward (not committed to)

Recorded 2026-09-28 so they aren't lost. None of these are milestones yet.

- **Linux VM with a paravirtual Tenstorrent device.** Apple Silicon virtualization has **no PCIe
  passthrough**: `Hypervisor.framework` has no device assignment, and `Virtualization.framework`
  passes through USB only (`VZUSBPassthroughDevice`). So a VM can't simply be handed
  `/dev/tenstorrent`, and on macOS that node doesn't exist anyway (it comes from tt-kmd, a Linux
  driver). The **macOS 27** SDK adds `VZCustomVirtioDevice`. A host app implements a virtio
  device, and `VZVirtioSharedMemoryRegion mapMemory:atOffset:size:` maps host memory into the
  guest, while `guestMemoryMappingAtPhysicalAddress:` exposes guest RAM to the host. That suggests:
  the host maps BAR0 via our dext and exposes it as a shared-memory region; guest "pin pages"
  requests become dext `PrepareDMA` calls on guest RAM; a small guest virtio driver presents a
  tt-kmd-compatible `/dev/tenstorrent`. The payoff is running the existing Linux stack (tt-kmd
  ABI, tt-metal, vLLM containers) nearly unchanged. The costs:
  - it **still needs the dext** (only the dext can map BAR0), so it doesn't sidestep signing;
  - it needs macOS 27 (this Mac is on 26.7);
  - it's unverified whether device memory (not RAM) can be mapped into a guest with correct MMIO
    semantics;
  - it needs a new guest driver;
  - the vLLM images are amd64, so they'd run under Rosetta for Linux.
- **…built on Apple's Containerization package** *(added 2026-09-30)*. `apple/containerization`
  (Swift, Apache-2.0; the engine under macOS 27's Container Machine) has a supported hook:
  `VZVirtualMachineInstance.Configuration.extensions` takes `VZInstanceExtension`s whose
  `configureVZ(_ config: inout VZVirtualMachineConfiguration, …)` runs before the VM is created,
  which is exactly where a `VZCustomVirtioDeviceConfiguration` for a `virtio-tt` device goes.
  Containerization already brings a tuned kernel (a custom one via `config.kernel`), OCI images
  (tt-inference-server ships as OCI), Rosetta for x86-64 guests (`config.rosetta`; the vLLM images
  are amd64), networking and a vsock agent. The **`container machine` CLI itself exposes no such
  hook** (`apple/container` never uses `VZInstanceExtension`), so this means our own small host app
  on the package, not Apple's CLI. The sketch:
  - the host maps BAR0 via the dext into `sharedMemoryRegions`;
  - a "pin pages" virtqueue feeds `guestMemoryMapping(atPhysicalAddress:length:)` into the dext's
    PrepareDMA and the iATU;
  - a control virtqueue carries config reads, ARC messages and reset (libttbh, silicon-verified);
  - a small guest `virtio-tt` driver presents tt-kmd's ioctl ABI, so tt-smi / vLLM / `tt serve`
    run unmodified in the guest.
  The blockers: dext signing, macOS 27, and **whether `VZVirtioSharedMemoryRegion.mapMemory`
  accepts device memory with correct MMIO semantics** (RiftVM shares ordinary memory). The
  fallback proxies window access over a virtqueue, as the broker does. This is the first route to
  *serving* on the Mac without porting the Linux stack. Apple docs: `VZCustomVirtioDevice`
  (macOS 27.0+), WWDC26 session 224.
- **ASM2464PD USB enclosure** (see Dev install): no driver at all, register pokes only.
- **Native macOS stack**: tt-umd's Darwin backend (tt-umd#3411) filled in with our user client,
  then blackhole-py or tt-metal on top.

## Risks

- **R1 — BAR4 is not assigned.** *(Corrected 2026-09-28: the early reading "BAR4 is 16 bytes" was
  wrong.)* Decoding `assigned-addresses` shows the three ranges sit at config offsets 0x10 (BAR0,
  512 MiB), 0x18 (BAR2, 1 MiB) and **0x24 (BAR5, 16 B)**. BAR4 (0x20) has no assignment, most
  likely because the Thunderbolt bridge window can't fit a 64-bit prefetchable BAR of up to
  32 GiB. So there are zero 4 GiB TLB windows on this link. tt-kmd tolerates that, and blackhole-py
  lives in 2 MiB windows, so this degrades rather than blocks. `GetInfo` will confirm from inside
  the dext.
- **R2 — DMA through DART.** 16 KiB IOMMU pages, per-device IOVA limits, segment counts.
  Blackhole's iATU wants contiguous IOVA per region; request single-segment mappings.
- **R3 — reset.** Blackhole reset normally goes through ARC messages; PCIe FLR/hot reset
  over a TB tunnel is untested (a TinyGPU fork needed fixes for Blackwell in enclosures).
- **R4 — entitlements for shipping** are Apple's call (TinyGPU shows they grant compute use).
- **R5 — bandwidth.** Gen4 ×4 link inside a TB tunnel; weight loading will be slower than a
  desktop slot. Irrelevant to feasibility.
- **R6 — hot-unplug / sleep.** The enclosure can vanish; the dext must handle `Stop` cleanly
  and userspace must treat a dead mapping (reads of `0xFFFFFFFF`) as disconnection.

## Naming

Nothing here is called `tt` (the official CLI owns it). Bundle IDs live under
`com.tenstorrent.ttstation.*`; the probe binary is `tt-station-bh-probe`.

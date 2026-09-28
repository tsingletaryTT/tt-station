# A Tenstorrent P100 on a MacBook, over Thunderbolt: a journey log

A running, append-only log of getting a Tenstorrent Blackhole P100 to run models from a Mac,
kept so it can become a blog post someday. It records the story (what we tried, what
surprised us, what was wrong) and leaves the design to the spec:
[`docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md`](../superpowers/specs/2026-09-28-macos-blackhole-dext-design.md).

**How to keep this log:** add a dated entry per session. Quote the real command output that
made each point. Keep the dead ends, because they are the interesting part. Leave the old
entries alone: when something turns out wrong later, say so in a new entry.

**The goal:** tt-station detects the Tenstorrent card attached to a Mac and asks the official
`tt` CLI / tt-model-manager to deploy a model right-sized for it. The card here is one modest
P100. We get there in small steps.

---

## 2026-09-28: "Macs don't do eGPUs. This isn't a GPU."

### The setup

- A MacBook Pro with Thunderbolt 5 (120 Gb/s ports) running macOS 26.
- A **Razer Core X V2** eGPU enclosure (USB4 v2).
- Inside it, instead of a graphics card, Taylor's own **Tenstorrent Blackhole P100**.

Apple Silicon Macs don't support eGPUs *as GPUs*. But an enclosure is just a PCIe slot at the
end of a Thunderbolt cable, and a Tenstorrent card doesn't need to be a GPU. The first prompt:
*"look for prior art on GH for drivers like this."*

### First look: does macOS even see it?

It does. `system_profiler` lists the enclosure on bus 0 at 80 Gb/s, and `ioreg` shows the card
sitting there with nothing attached to it:

```
+-o pci1e52,b140@0  <class IOPCIDevice, ... registered, matched, active>
  "compatible"               = "pci1e52,43","pci1e52,b140","pciclass,120000"
  "IOPCITunnelled"           = Yes
  "IOPCIExpressLinkStatus"   = 4164          # 0x1044 → Gen4, x4
  "IODeviceMemory"           = 512 MiB, 1 MiB, 16 bytes
  "IOPCIDeviceMapperPageSize"= 16384         # Apple's DART IOMMU: 16 KiB pages
  "IOServiceDEXTEntitlements"= ("com.apple.developer.driverkit.transport.pci")
```

What each line means:

- **`1e52:b140`** is Tenstorrent's vendor ID and the Blackhole chip.
- **Subsystem `0x43`** is the P100.
- **Class `0x12`** is "processing accelerator".
- The last line is macOS saying, in effect, *"a PCIDriverKit driver extension may claim this."*
- The oddity is the **16-byte third memory region**. On Linux, Blackhole's BAR4 holds up to
  32 GiB of 4 GiB windows. That is filed as risk R1: park it and let the driver measure it.

### "There was an Apple Silicon Linux that had a driver working"

Taylor remembered Asahi Linux. There *was* a Tenstorrent bounty:

- [tt-metal#18296](https://github.com/tenstorrent/tt-metal/issues/18296), "Bring Up TT-Metal on
  Asahi", $2500, closed in August 2025.
- It turned out to be **packaging only**. The tt-kmd PR was closed as "non-functional on the
  Asahi platform due to its lack of PCIe support."

Taylor's comeback: *"it didn't work because no one tried an egpu."* That was fair (no one had),
but an eGPU wouldn't have helped:

- Asahi's 2026 USB4/Thunderbolt patches for M1–M3 support only XDomain and USB3 tunnels.
- **PCIe tunnels, the thing an enclosure needs, still aren't done.**
- This MacBook's TB5 also puts it at M4 or later, beyond what Asahi supports today.

The twist is that **macOS already does the part Asahi can't.** It tunnels PCIe and enumerates
the card. All that's missing is a driver.

### Prior art

Searching GitHub, the web and Tenstorrent's internal Glean turned up no attempt anywhere to
drive a Tenstorrent card from a Mac. The closest things were a tt-umd PR that builds on macOS
against the *simulator* only ([tt-umd#3411](https://github.com/tenstorrent/tt-umd/pull/3411),
where every hardware call returns `-ENOTSUP`), and some Slack chatter. The chatter included
the Razer Core X V2 being suggested for TT cards, and someone noting that *"tinycorp recently
got NVIDIA and AMD UMDs up on the Mac+eGPU front."*

That last lead was the key one:

- **[tinygrad's TinyGPU](https://github.com/tinygrad/tinygrad/tree/master/extra/usbgpu/tbgpu)**
  is an Apple-approved PCIDriverKit extension that runs NVIDIA/AMD GPUs over Thunderbolt on
  Apple Silicon.
- Its design is tiny. The driver opens the PCI device, and a user client hands the BARs to
  userspace through `IOConnectMapMemory64`. Everything else (config reads, reset, DMA setup)
  is a few RPCs.
- Its shipped app won't claim our card (the entitlement is scoped to NVIDIA/AMD vendor IDs and
  display-class devices), but the *design* ports almost one to one.
- **[blackhole-py](https://github.com/boopdotpng/blackhole-py)** runs Llama 3 8B on one
  Blackhole from pure Python, using only a handful of tt-kmd ioctls. That is a small surface
  to rebuild on top of a TinyGPU-style driver.

### The plan, and the real goal

Mid-build, Taylor sharpened the objective:

> our goal is to make tt-station able to invoke the new tt CLI / tt-model-manager and deploy
> models right-sized for the attached TT device. in this case, a small meager p100.
>
> we will get there in small steps.

So the driver is the first leg, not the destination. The milestones:

- **M1**: the driver attaches and the BARs map.
- **M2**: the first read from the chip's ARC firmware processor, over the NOC.
- **M3**: DMA, plus blackhole-py running on the Mac.
- **M4**: tt-station reports "P100 x1".
- **M5**: the official tooling deploys a right-sized model.
- **M6**: Apple signs the driver.

### Building the skeleton (M0)

`macos/TTStationDriver/` holds:

- **the driver extension** (`TTBlackholeDriver` + `TTBlackholeUserClient`), adapted from
  TinyGPU;
- **a headless host app** that asks macOS to activate it;
- **`tt-station-bh-probe`**, a read-only C smoke test;
- all generated with xcodegen.

Deliberate choices:

- **Match only `1e52:b140`.** TinyGPU's dev entitlements match *any* PCI device; ours stay
  scoped to Tenstorrent even in development.
- **Read-only RPCs in v1.**
- **Bus mastering stays off** until DMA exists, so the chip can't write host memory before
  we can give it a safe buffer.
- **Nothing is named `tt`.** That name belongs to the official CLI, and this project renamed
  itself away from it just last month.

The first build failed on one line:

```
TTBlackholeDriver.cpp:78:24: error: cannot deduce type of initializer list because
std::initializer_list was not found; include <initializer_list>
```

The line was `for (uint8_t bar : {0, 2, 4})`. DriverKit's C++ runtime is a stripped-down
subset, with no `std::initializer_list`. A plain `static const uint8_t kBars[]` fixed it. It's
a small reminder that a dext is its own little world.

The second build succeeded, with the result verified:

- a universal (arm64 + x86_64) `.dext` embedded at `Contents/Library/SystemExtensions/`;
- the personality resolved to `IOPCIPrimaryMatch 0xb1401e52` and `IOPCITunnelCompatible = true`;
- ad-hoc signed carrying the Tenstorrent-scoped `transport.pci` entitlement.

With no driver loaded, the failure paths behave:

```
$ ./tt-station-bh-probe
no 'ttstation-blackhole' service — is the dext activated and the card connected?
exit=1
$ TTStationDriver.app/Contents/MacOS/TTStationDriver install
Run from /Applications/TTStationDriver.app — macOS refuses dexts from elsewhere.
exit=3
```

### Corrections made along the way

- A research pass assumed "Thunderbolt 3, ~22–32 Gb/s". The actual hardware says otherwise:
  a USB4 v2 enclosure on an 80 Gb/s port, and a Gen4 x4 link to the card.

### Where it stands

- The driver compiles and signs, but **it has not been loaded yet.**
- Loading needs SIP disabled; whether `systemextensionsctl developer on` alone is enough is
  unknown.
- The next entry should be the first time macOS hands a Tenstorrent chip to our code, or the
  first reason it won't.

---

## 2026-09-28 (later): "…without booting into safe mode"

Taylor's constraint: *"try ways to make this work that don't require me to boot into safe
mode."* By safe mode they meant Recovery, where `csrutil disable` lives. So the question became
**which routes let an unsigned or self-built dext load while SIP stays on?**

### Dead end 1: developer mode

`systemextensionsctl developer on` is the documented switch for testing system extensions. It
turns you away at the door:

```
$ systemextensionsctl developer
At this time, this tool cannot be used if System Integrity Protection is enabled.
This limitation will be removed in the near future.
Please remember to re-enable System Integrity Protection!
```

It is a nice irony that the tool for *developing* extensions needs you to switch off the
system that makes extensions safe.

### Dead end 2: ad-hoc signing

We had already built an ad-hoc-signed app. Running it, even just `status`, was killed on the
spot: exit 137, no output. The unified log says why:

```
amfid: ... not valid: Error Domain=AppleMobileFileIntegrityError Code=-424
       "The file is adhoc signed but contains restricted entitlements"
kernel: proc 8524: load code signature error 4 for file "TTStationDriver"
```

`com.apple.developer.system-extension.install` is a *restricted* entitlement. With SIP on,
AMFI won't let an ad-hoc binary claim it at all, so we never even got as far as asking for the
dext.

(A side quest while reading that log: in zsh, `log` is a **shell builtin** that shadows
`/usr/bin/log`, so `log show …` fails with "too many arguments". This week's whole project
started with a name collision, `tt` versus our old CLI, and here was another one. The docs now
say `/usr/bin/log`.)

### What actually works with SIP on: a real team signature

Apple's DriverKit engineers wrote a forum post on this
([thread 809202](https://developer.apple.com/forums/thread/809202)). The old "disable SIP" advice
dates from when DriverKit was new. DriverKit now has **development entitlement variants** that are
*"available on all paid developer accounts without any special approval"* and that *"allow a DEXT
to match against any hardware"*. Xcode 16+ automatic signing handles the PCI family for
development. So the gate is not SIP: it's **a paid Apple Developer team**. This Mac has none
(`security find-identity` found 0 identities).

Getting ready for that meant one more design change. A development-signed dext shouldn't carry
`allow-any-userclient-access`, so clients need their own `userclient-access` entitlement.
That entitlement is restricted too, and a bare CLI tool can't embed a provisioning profile.
So the probe now also lives *inside the host app*, as `TTStationDriver probe`, and
`install-dev.sh --team <ID>` does the whole development-signed build in one pass.

### Plan B that needs no driver at all

tinygrad has a second eGPU trick:

- `CustomASM24Controller` in `tinygrad/runtime/support/usb.py` sends raw PCIe config and memory
  transactions through **libusb**, to a card sitting behind an **ASMedia ASM2464PD** USB-to-PCIe
  bridge.
- That needs no dext, no signing and no SIP change.
- The catches: a different (cheaper) enclosure, patched bridge firmware, slow MMIO, and almost
  no DMA.
- It's good enough to poke Blackhole registers (M1/M2), not to run models.

### Where it stands

- The skeleton is committed (`3501330`).
- The SIP-on route is ready to try the moment a paid team is signed in to Xcode.
- The open question is whose team: Tenstorrent's (signing reportedly got approved last
  week), or a personal $99 account to get moving.

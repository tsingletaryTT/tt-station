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

![A Tenstorrent p100a card installed in a Razer Core X V2 enclosure, below a Corsair RM750e power supply](images/2026-09-28-p100a-in-razer-core-x-v2.jpg)

*The rig: a Tenstorrent **p100a** in the Razer Core X V2's slot, below the enclosure's Corsair
RM750e (750 W) power supply and a bundle of unused modular PSU cables. There's no GPU anywhere
in the box.*

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

---

## 2026-09-28 (evening): what fits a P100, and a ghost on this very Mac

Taylor started Apple's developer ID validation. Until that clears, the driver can't load, so
the question became what could move forward without it.

### The card tells us its name, and the silkscreen agrees

A Mac will show *any* process the IORegistry, driver or not. The card's PCI subsystem ID is
`0x43`, and luwen's board-type table (`crates/luwen-api/src/chip/mod.rs`) maps `0x43 => "p100a"`.
The photo at the top of this log, taken inside the enclosure, reads **p100a** on the shroud.
So tt-station can name the exact board from the Mac with no driver loaded.

### What's right-sized for a P100?

The official `tt` CLI already answers this. `tt model list --hw <device>` filters its catalog
to one device config and skips auto-detection, which is exactly what a Mac needs because it
can't auto-detect yet. The catalog it ships (release 0.22.0, 70 models) lists two for a
`p100`:

```
Llama-3.1-8B            p100   EXPERIMENTAL   ctx=65536
Llama-3.1-8B-Instruct   p100   EXPERIMENTAL   ctx=65536
```

That's a nice echo: Llama 3 8B is also the model blackhole-py runs on a single Blackhole.
"A small meager P100", in Taylor's words, gets one model family, in 8B.

**tt-model-manager** turned out to be `tt-model`. It publishes and pulls self-contained model
bundles over the Hugging Face Hub, and `tt model` / `tt serve` forward to it for community
bundles. Its compatibility check refuses an arch mismatch outright and warns on other
mismatches, such as too few chips.

The sobering part is that *serving* is built for Linux: tt-inference-server containers or
tt-model bundles, a host with direct access to the card. Detection and right-sizing can land on
the Mac now. Serving locally is still the long pole, even after the driver works.

### A ghost in `~/.local/bin`

While checking whether the official CLI was installed here, `which -a tt` turned up
`~/.local/bin/tt`. Running `--help` printed:

```
Operator CLI for tt-station
Usage: tt [OPTIONS] <COMMAND>
```

It was **our own old CLI**, built July 15, still squatting on `tt` on this Mac. That's the
exact shadowing bug last month's rename fixed. The rename stopped *creating* the collision, but
nothing ever cleaned up an existing one, and the official `tt` wasn't installed here at all.
Before tt-station can delegate to `tt`, the real `tt` has to be the one on the PATH.

---

## 2026-09-28 (night): the Mac becomes its own box

Taylor, with Apple's identity validation still pending:

> since we'll be our own host in this case, we'll need to reuse some of the plumbing.
> We want to prove it today, with the enclosure we have.

### Evicting the ghost

The stale `~/.local/bin/tt` (our own pre-rename CLI) was deleted. Then:

```
$ uv tool install tenstorrent
Installed 1 executable: tt
$ tt --version
tt 1.0.1
```

The real `tt` runs happily on macOS. `tt device status` fails with *"Required tool 'tt-smi' is
not installed"*, because its detection is tt-smi-based and a Mac can't reach the card that way.
But `tt model list --hw p100` doesn't need detection at all.

### `tt-station local`

Every tt-station command until now talked to a remote box's agent. `local` is the first to
treat the machine it runs on as the box. It reuses the plumbing:

- **The device table.** The `(board_type, count) -> mesh` table the box agent uses moved out of
  `tt-station-agentd` into `libttstation::device_mesh`, gained `p100`/`p100a → p100` to match the
  official CLI's own mapping, and is re-exported to the agent unchanged. One table, two hosts.
- **Detection without a driver.** `ioreg -a` gives any process the card's IDs, link and BARs.
  A real capture from this Mac is now the test fixture.
- **Right-sizing delegated.** The official CLI does it: `tt --json model list --hw p100`.

On the real card, with no driver loaded:

```
$ tt-station local
╔══ tt-station local
║  p100a  (blackhole 1e52:b140, subsys 0043)  via Thunderbolt  PCIe Gen4 x4  @3:0:0
║    memory ranges: 512 MiB, 1 MiB, 16 B
║  device config: p100
║
║  right-sized models (official tt 1.0.1: `tt model list --hw p100`):
║    Llama-3.1-8B                 llm   EXPERIMENTAL  ctx 65536
║    Llama-3.1-8B-Instruct        llm   EXPERIMENTAL  ctx 65536
╚══ serving on this Mac is not wired up yet (needs the dext; see macos/TTStationDriver)
```

That's half of the north star, working today: *detect the attached device → ask the official
tooling what's right-sized for it.*

### The ghost gets a test

Having just been bitten, `tt-station local` refuses to trust a `tt` that isn't the official one.
The official CLI answers `--version` with `tt <semver>`, and our old CLI had no `--version`.
There's an end-to-end test with a fake `tt` that behaves exactly like the stale binary, and it
has been seen to fail: loosening the guard turns it red.

The same lesson went into the Mac install. `macos/scripts/ensure-official-tt.sh` is run by
`install.sh` and bundled into the app for a first-run offer. It removes a stale tt-station-as-`tt`
(ours, so safe), refuses to touch a *foreign* `tt`, installs uv via Homebrew if needed, runs
`uv tool install tenstorrent`, and verifies the result.

Its first draft had its own instrument bug. It misfiled the stale binary as "foreign" because,
under `set -o pipefail`, `tt --help | grep -q …` fails when the binary exits non-zero, even
though grep matched. Our real old binary exits 0 on `--help`, so it would have *happened* to
work on this Mac. Only a fake that exits 2 exposed it. The fix is to capture first, then match.

### A correction: BAR4 was never 16 bytes

Decoding `assigned-addresses` (which records the config-space offset of each range) showed the
three ranges are **BAR0, BAR2 and BAR5**. The 16-byte one is BAR5. **BAR4 was never assigned at
all**, probably because a Thunderbolt bridge can't fit a 64-bit BAR of up to 32 GiB. So the
first entry's "16-byte BAR4" was a misreading: a list of ranges isn't a list indexed by BAR
number. The practical upshot is unchanged. There are no 4 GiB windows over this link, which
tt-kmd tolerates.

### "Prove it today": the Apple wall, mapped

A research pass on SIP-on options came back with one conclusion:

- A **free** Personal Team can't do it. Apple's capability table doesn't offer System Extension
  or DriverKit to free accounts.
- Borrowing tinygrad's signed dext can't work, because its match lives in its signed Info.plist.
- There's no generic IOKit user client that maps BARs on Apple Silicon.

The *only* legitimate SIP-on route is a development signature from a **paid** team. Options:

1. Get invited to an existing paid team (a colleague's; one exists behind a Developer ID signing
   pipeline internally). This takes hours.
2. Finish the individual enrollment in the iPhone Apple Developer app. That can take minutes to
   48 hours.
3. Tenstorrent's org enrollment (legal approved the terms on 9/23). That takes weeks.

One more adjustment, per Apple DTS: the development entitlement now uses Apple's wildcard PCI
match. The dext still only ever claims `1e52:b140`, through its Info.plist personality plus an
ID re-check in `Start`.

---

## 2026-09-28 (late): "what if a Linux VM got /dev/tenstorrent?", and the app notices the card

### The VM question

The idea: if macOS is the problem, run Linux in a VM and give it the card. Two walls:

- On a Mac there is no `/dev/tenstorrent` to give. That node is made by tt-kmd, a Linux
  kernel driver.
- Apple Silicon virtualization has **no PCIe passthrough**. Grepping the macOS 27 SDK confirms
  it: `Hypervisor.framework` has CPUs, memory and interrupt controllers, and
  `Virtualization.framework` passes through USB only.

The SDK did hold a surprise, though. `VZCustomVirtioDevice` (new in macOS 27) lets a host app
*be* a virtio device for a Linux guest, with shared memory regions it can map host memory into.
Put our dext's BAR mapping behind that, and a Linux guest could see something tt-kmd-shaped
while the unmodified Linux stack runs on top. It doesn't get around signing, because the host
still needs the dext. Taylor filed it under **possible paths forward**, together with the USB
bridge and a native macOS stack. They're recorded in the spec, not scheduled.

### The app acknowledges the card

The menu-bar app now knows about "This Mac":

- a sidebar section and a popover row, which only appear when a card is attached;
- a detail pane with what macOS can see without a driver: `1e52:b140`, P100A (subsystem `0x43`),
  PCIe Gen4 ×4 over Thunderbolt, memory `512 MiB · 1 MiB · 16 B`, and a note that there are no
  4 GiB windows on this link;
- the official CLI's right-sized models (Llama-3.1-8B and -Instruct, EXPERIMENTAL);
- a **Status** list that says plainly what works (detection, right-sizing) and what doesn't yet
  (live telemetry, serving on this Mac), and why.

Two small instrument lessons on the way:

- **GUI apps get launchd's PATH** (`/usr/bin:/bin:/usr/sbin:/sbin`), which never includes
  `~/.local/bin`, where uv puts `tt`. `tt-station local` would have quietly reported "no
  official tt" when launched from the app, while working perfectly in a terminal. It now falls
  back to uv's and Homebrew's install locations when `tt` isn't on PATH, and it's tested under
  `env -i` with launchd's PATH. A `tt` that *is* on PATH but isn't official is still refused,
  never silently skipped.
- **The popover's only "Open window" button lived inside a box's detail.** On a Mac with a
  local card and no box selected, the new pane would have been unreachable from the menu bar.
  The "This Mac" row got its own button.

I couldn't screenshot the result: this terminal has no Screen Recording permission, and granting
it would be a security-settings change. So the first look at the pane is Taylor's.

### First look

![The TTStation menu-bar popover: a "This Mac" row reading "P100A via Thunderbolt · 2 right-sized models" above the remote QuietBox "qb2-lab, 4xBH"](images/2026-09-28-popover-this-mac.png)

*Taylor's screenshot of the menu-bar popover, the first look at the new UI by anyone. **This Mac**
(P100A via Thunderbolt, 2 right-sized models) sits above **qb2-lab**, the QuietBox 2 on the LAN
(4× Blackhole). The card in the enclosure and the box across the network now appear in one list.
One is reached over the network through a paired agent. The other is read from the IORegistry
and right-sized by the official `tt` CLI, with no driver at all.*

![The TTStation window with "This Mac" selected: cards for identity (P100A, Blackhole, Thunderbolt, P100 badge), what macOS can see without a driver, right-sized models (Llama-3.1-8B and -Instruct, Experimental, 64K context), and a status list](images/2026-09-28-window-this-mac.png)

*The control-room window with **This Mac** selected. Everything here was learned without a driver.
The PCI identity and the Thunderbolt link come from the IORegistry. The P100 device config
comes from the shared mesh table. The two Llama 3.1 8B models come from the official
`tt model list --hw p100`. The status list says plainly that live telemetry and serving are
still "not yet".*

*One more instrument lesson from this screenshot: the "Serving on this Mac" reason shows literal
backticks around `tt serve`. SwiftUI only parses Markdown in `Text` built from a literal
(`LocalizedStringKey`), not from a runtime `String`. The footer under the models, a literal,
rendered as code; the reasons, passed in as `String`s, didn't. Fixed right after the screenshot
was taken.*

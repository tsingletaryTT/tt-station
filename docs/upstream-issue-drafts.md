# Upstream issue drafts (NOT filed)

Found while building the macOS Blackhole driver path (`macos/TTStationDriver/`, spec
`docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md`). Kept here to file later.
**Nothing below has been posted anywhere.** Filing means public issues under Taylor's account, so
it waits for their go-ahead.

Environment for every item: ttsim **v1.10.12** (public source drop, commit `6cab92f`), built natively
on macOS 26.7 / Apple Silicon with `./make.py :build` (`src/_out/release_bh/libttsim.so`, single-chip
Blackhole, which models a P150), driven through `libttsim_pci_*` by `bhpy/broker_ttsim.c` in this
repo. All messages are ttsim's own; each one terminates the host process (`_Exit`).

---

## 1. ttsim: Tensix config register 4 is not modelled on Blackhole

**Repo:** tenstorrent/ttsim

**What happens:** blackhole-py (github.com/boopdotpng/blackhole-py, commit `d8eae8f`) boots on ttsim
through a libttsim host. Its firmware uploads and runs on the RISC-V cores for over 6 million clocks,
then:

```
[6044609] ERROR: UnsupportedFunctionality: tensix_cfg_wr32: reg=4
```

**Where:** `src/tensix.cpp` `tensix_cfg_wr32` handles an explicit list of config registers (0–3
banked, then a set of others; 97 `CFG_REG_*_WR` entries). Register 4 isn't on the Blackhole list.
tt-metal's kernels evidently never write it; blackhole-py's hand-written firmware does.

**Ask:** model register 4 (or document it as out of scope). Knowing which firmware core wrote it
would help. The error doesn't say, so a core/tile id in the message would make this class of report
easier to act on.

**Repro:** `ttbh-broker-ttsim libttsim.so SOCK`, then blackhole-py's `Device().boot()` via
`python3 -m ttstation_bhpy` (see `macos/TTStationDriver/bhpy/README.md`).

---

## 2. ttsim: strided-TLB registers unimplemented (fatal on write)

**Repo:** tenstorrent/ttsim

**What happens:** programming 2 MiB TLB window 0 the way tt-kmd does (`blackhole.c`
`blackhole_configure_tlb_2M`) includes clearing that window's strided (non-rectangular multicast)
register, which follows the 210 TLB config registers in BAR0:

```
ERROR: UnimplementedFunctionality: pci_mem_wr_cur: bar0: offset=0x1fc009d8 size=4
```

`0x1FC009D8` = TLB regs base `0x1FC00000` + 210 × 12 (strided block) + window 0 × 4.

**Ask:** accept writes to the strided registers for windows 0–31 (a write of 0 = "no stride" would
cover tt-kmd's usage), or make it non-fatal. Real silicon and tt-kmd both use them.

**Workaround here:** `broker_ttsim.c` drops writes to that range.

---

## 3. ttsim: TLB config registers can't be read back (fatal on read)

**Repo:** tenstorrent/ttsim

**What happens:** after programming a TLB window, reading one of its config registers (a common
posted-write flush on real PCIe) fails:

```
ERROR: UnimplementedFunctionality: pci_mem_rd_cur: bar0: offset=0x1fc00008 size=4
```

**Ask:** return the last written value (the registers are readable on silicon), or at least make the
read non-fatal.

**Workaround here:** `broker_ttsim.c` answers reads of the TLB-register block with 0.

---

## 4. ttsim: unicast TLB with start coordinates set is rejected

**Repo:** tenstorrent/ttsim

**What happens:** a unicast window (multicast bit clear) with `x_start`/`y_start` equal to the
target, as blackhole-py's `ConfigureTlbPayload` programs it (`start = end = core`), fails:

```
ERROR: UnsupportedFunctionality: tlb_translate: x_start/y_start set without mcast: tlb_cfg1=0x4004000 tlb_cfg2=0x40
```

Silicon accepts this: blackhole-py runs on hardware with exactly this programming.

**Ask:** ignore start coordinates when multicast is clear, matching hardware, or document the
stricter rule.

**Workaround here:** the broker now uses tt-kmd's own kernel-window convention (start = 0 for
unicast) on every backend. That's valid on silicon too.

---

## 5. ttsim: ARC TEST message (0x90) isn't echoed

**Repo:** tenstorrent/ttsim

**What happens:** `TEST` completes with status 0 but an all-zero response. On silicon the firmware
replies `{0, value + 1, …}` (tt-kmd `test/arc_msg_test.c`; verified 16/16 on a p300c in qb2-lab
through the same queue code). In `src/tile.cpp` `arc_service_message`, `0x90` is in the
"does not modify modeled subsystems" group.

**Ask:** echo `payload[0] + 1` so host-side message-queue tests behave the same on ttsim and silicon.

---

## 6. (maybe) blackhole-py: unicast TLB programming sets start = end

**Repo:** boopdotpng/blackhole-py. Lower priority, and arguably not a bug.

`pcie.py` `ConfigureTlbPayload` sets `x_start/y_start = core` for unicast windows. Silicon ignores it,
but ttsim rejects it (item 4), and tt-kmd's own kernel window uses start = 0. Using 0 for unicast
would make blackhole-py run on ttsim without host-side normalisation. Only worth raising once item 1
is resolved, since that's what currently stops blackhole-py on ttsim anyway.

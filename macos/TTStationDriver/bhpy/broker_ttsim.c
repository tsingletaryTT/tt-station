// broker_ttsim.c — `ttbh-broker-ttsim LIBTTSIM.so SOCKET`: the broker with Tenstorrent's ttsim
// (github.com/tenstorrent/ttsim, Apache-2.0) standing in for the card and the dext.
//
// ttsim presents a simulated Blackhole as a virtual PCIe device (docs/libttsim_api.md): the host
// forwards config-space and BAR accesses (libttsim_pci_*), advances time (libttsim_clock, which
// runs the RISC-V cores and Tensix units), and serves the chip's DMA through callbacks. That's the
// same contract the dext gives the real broker, so everything above this file (libttbh's TLB packing
// and ARC/telemetry code, the iATU plan, the SCM_RIGHTS sysmem, ttstation_bhpy, blackhole-py) runs
// unchanged against a chip that actually EXECUTES firmware and kernels.
//
// Mapping onto broker_backend:
//   BARs      libttsim_pci_mem_rd/wr_bytes at the BAR bases read from config space (mmio_rd/wr)
//   own win   TLB 201 programmed through the same calls (ttbh_window rd32/wr32)
//   DMA       prepare_dma hands out a fake IOVA per sysmem buffer; ttsim's outbound iATU (which
//             libttbh programs) turns chip accesses into callbacks at that IOVA, which we translate
//             back to the shm pages the Python client also has mapped
//   time      tick → libttsim_clock(TTBH_TTSIM_CLOCKS, default 2000)
//
// Identity: ttsim's single-chip Blackhole is a P150 (logical harvesting mask 0xC0 → 12 of 14
// Tensix columns = 120 cores, 8 GDDR channels; tile.cpp) but reports subsystem id 0. The broker
// presents it as p150a (0x40), the board it models, unless TTBH_SIM_SUBSYSTEM says otherwise.
//
// ttsim is a process-wide singleton and single-threaded; the broker is too. POSIX + dlopen, so it
// runs on macOS (native Mach-O ttsim builds) and Linux.

#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "broker.h"

static void (*sim_init)(void);
static void (*sim_set_dma)(void (*)(uint64_t, void *, uint32_t), void (*)(uint64_t, const void *, uint32_t));
static uint32_t (*sim_cfg_rd)(uint32_t, uint32_t);
static void (*sim_cfg_wr)(uint32_t, uint32_t, uint32_t);
static void (*sim_mem_rd)(uint64_t, void *, uint32_t);
static void (*sim_mem_wr)(uint64_t, const void *, uint32_t);
static void (*sim_clock)(uint32_t);

static uint64_t bar_base[3];          // index 0 = BAR0, 2 = BAR2
static uint32_t clocks_per_tick = 2000;

// ── DMA: fake IOVAs ↔ the shm pages ───────────────────────────────────────────────────────
#define MAX_MAPS 16
#define IOVA_START 0x1000000000ull   // 64 GiB: clear of ttsim's BAR windows, 16 KiB-aligned
static struct { bool used; uint64_t iova, len; uint8_t *host; } maps[MAX_MAPS];
static uint64_t next_iova = IOVA_START;

static uint8_t *translate(uint64_t paddr, uint32_t size)
{
    for (int i = 0; i < MAX_MAPS; i++)
        if (maps[i].used && paddr >= maps[i].iova && paddr + size <= maps[i].iova + maps[i].len)
            return maps[i].host + (paddr - maps[i].iova);
    fprintf(stderr, "ttbh broker (ttsim): chip DMA to unmapped host address 0x%llx (+%u)\n",
            (unsigned long long)paddr, size);
    return NULL;
}
static void dma_rd(uint64_t paddr, void *dst, uint32_t size)
{
    uint8_t *p = translate(paddr, size);
    if (p) memcpy(dst, p, size); else memset(dst, 0xFF, size);
}
static void dma_wr(uint64_t paddr, const void *src, uint32_t size)
{
    uint8_t *p = translate(paddr, size);
    if (p) memcpy(p, src, size);
}

static int prepare(void *ctx, void *addr, uint64_t len, ttbh_dma_segment *segs, uint32_t *count, uint32_t *handle)
{
    (void)ctx;
    for (uint32_t i = 0; i < MAX_MAPS; i++) {
        if (maps[i].used) continue;
        maps[i].used = true;
        maps[i].iova = next_iova;
        maps[i].len = len;
        maps[i].host = addr;
        next_iova += (len + 0x3FFF) & ~(uint64_t)0x3FFF;
        segs[0] = (ttbh_dma_segment){ maps[i].iova, len };
        *count = 1;
        *handle = i;
        return 0;
    }
    return -1;
}
static void complete(void *ctx, uint32_t h) { (void)ctx; if (h < MAX_MAPS) maps[h].used = false; }

// ── BARs and time ─────────────────────────────────────────────────────────────────────────
// ttsim's TLB config registers are write-only: reading one is a fatal UnimplementedFunctionality
// (observed: "pci_mem_rd_cur: bar0: offset=0x1fc00008", the posted-write flush libttbh and the broker
// do after programming a window, which matters on a real PCIe link and means nothing in a sim).
// Answer those reads with 0 instead of forwarding them.
#define TLB_REGS_END (TTBH_TLB_REGS_START + 0x1000u)
static void mmio_rd(void *ctx, int bar, uint64_t off, void *dst, uint32_t n)
{
    (void)ctx;
    if (bar == 0 && off >= TTBH_TLB_REGS_START && off + n <= TLB_REGS_END) { memset(dst, 0, n); return; }
    sim_mem_rd(bar_base[bar] + off, dst, n);
}
// ttsim (v1.10.12) doesn't implement the strided-TLB registers that follow the 210 TLB config
// registers in BAR0 (tt-kmd clears them for windows 0..31, and so do we on hardware); writing one is
// a fatal UnimplementedFunctionality that _Exit()s the whole process. Drop those writes here, and
// only here. The broker's TLB programming stays identical to what real silicon gets. (Observed:
// "pci_mem_wr_cur: bar0: offset=0x1fc009d8 size=4", i.e. window 0's strided register.)
#define STRIDED_START (TTBH_TLB_REGS_START + TTBH_TLB_STRIDED_REGS_OFF)
#define STRIDED_END (STRIDED_START + TTBH_TLB_STRIDED_COUNT * 4u)
static void mmio_wr(void *ctx, int bar, uint64_t off, const void *src, uint32_t n)
{
    (void)ctx;
    if (bar == 0 && off >= STRIDED_START && off + n <= STRIDED_END) return;
    sim_mem_wr(bar_base[bar] + off, src, n);
}
static void tick(void *ctx) { (void)ctx; sim_clock(clocks_per_tick); }

// The server's own window (201) through function calls.
static void own_wr32(void *ctx, uint32_t off, uint32_t v) { mmio_wr(ctx, 0, off, &v, 4); }
static volatile uint8_t *own_aim(void *ctx, uint32_t x, uint32_t y, uint64_t base)
{
    ttbh_tlb_config c = { .addr = base, .x_end = (uint8_t)x, .y_end = (uint8_t)y, .ordering = 1 };
    if (ttbh_tlb2m_program_with(own_wr32, ctx, TTBH_DRIVER_TLB_INDEX, &c) != TTBH_OK) return NULL;
    return (volatile uint8_t *)1;                          // non-NULL token: access goes via rd32/wr32
}
static int own_rd32(void *ctx, uint32_t off, uint32_t *v)
{
    tick(ctx);                                             // let firmware answer between polls
    mmio_rd(ctx, 0, (uint64_t)TTBH_DRIVER_TLB_INDEX * TTBH_TLB_2M_SIZE + off, v, 4);
    return TTBH_OK;
}
static int own_rd32_wrap(void *ctx, uint32_t off, uint32_t *v) { return own_rd32(ctx, off, v); }
static int own_wr32_win(void *ctx, uint32_t off, uint32_t v)
{
    mmio_wr(ctx, 0, (uint64_t)TTBH_DRIVER_TLB_INDEX * TTBH_TLB_2M_SIZE + off, &v, 4);
    return TTBH_OK;
}
static ttbh_window own_window(void *ctx)
{
    return (ttbh_window){ .ctx = ctx, .aim = own_aim, .rd32 = own_rd32_wrap, .wr32 = own_wr32_win };
}

#define RESOLVE(var, name) do { *(void **)&var = dlsym(h, name); if (!var) { fprintf(stderr, "missing %s in %s\n", name, lib); return 1; } } while (0)

int main(int argc, char **argv)
{
    if (argc != 3) { fprintf(stderr, "usage: %s LIBTTSIM.so SOCKET\n", argv[0]); return 2; }
    const char *lib = argv[1];
    void *h = dlopen(lib, RTLD_NOW | RTLD_LOCAL);
    if (!h) { fprintf(stderr, "dlopen %s: %s\n", lib, dlerror()); return 1; }
    RESOLVE(sim_init, "libttsim_init");
    RESOLVE(sim_set_dma, "libttsim_set_pci_dma_mem_callbacks");
    RESOLVE(sim_cfg_rd, "libttsim_pci_config_rd32");
    RESOLVE(sim_cfg_wr, "libttsim_pci_config_wr32");
    RESOLVE(sim_mem_rd, "libttsim_pci_mem_rd_bytes");
    RESOLVE(sim_mem_wr, "libttsim_pci_mem_wr_bytes");
    RESOLVE(sim_clock, "libttsim_clock");
    const char *cl = getenv("TTBH_TTSIM_CLOCKS");
    if (cl && atoi(cl) > 0) clocks_per_tick = (uint32_t)atoi(cl);

    sim_set_dma(dma_rd, dma_wr);                           // must precede init (libttsim_api.md)
    sim_init();

    const uint32_t bdf = 0;                                // single-chip build: the one endpoint
    uint32_t id = sim_cfg_rd(bdf, 0);
    if ((id & 0xFFFF) != 0x1E52 || (id >> 16) != 0xB140) {
        fprintf(stderr, "ttsim presents %08x, not a Blackhole (1e52:b140)\n", id);
        return 1;
    }
    for (int bar = 0; bar <= 2; bar += 2) {                // 64-bit memory BARs: low | high << 32
        uint32_t lo = sim_cfg_rd(bdf, 0x10 + 4u * (uint32_t)bar), hi = sim_cfg_rd(bdf, 0x14 + 4u * (uint32_t)bar);
        bar_base[bar] = ((uint64_t)hi << 32) | (lo & ~0xFu);
    }
    sim_cfg_wr(bdf, 4, sim_cfg_rd(bdf, 4) | 0x6);          // memory space + bus master, as the dext does
    const char *ss = getenv("TTBH_SIM_SUBSYSTEM");
    uint16_t subsystem = ss ? (uint16_t)strtoul(ss, NULL, 0) : 0x40;   // p150a: the board ttsim models

    fprintf(stderr, "ttbh broker (ttsim): %s  BAR0 0x%llx  BAR2 0x%llx  presenting subsystem 0x%x, %u clocks/tick\n",
            lib, (unsigned long long)bar_base[0], (unsigned long long)bar_base[2], subsystem, clocks_per_tick);

    broker_backend be = {
        .ctx = NULL,
        .device_id = 0xB140, .subsystem_id = subsystem,
        .prepare_dma = prepare, .complete_dma = complete,
        .own_window = own_window,
        .mmio_rd = mmio_rd, .mmio_wr = mmio_wr, .tick = tick,
    };
    return broker_serve(&be, argv[2]);
}

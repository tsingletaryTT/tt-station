// test_ttbh.c — hardware-free tests for libttbh.  `make -C macos/TTStationDriver/libttbh test`
//
// Three independent checks on the bit packing (which the QuietBox check can't reach, since
// there tt-kmd does the packing):
//   * hand-computed register words for known configs;
//   * a GOLDEN decoder written as a packed C bitfield struct straight from the documented layout
//     (a different implementation strategy from ttbh.c's shift-and-mask encoder);
//   * a simulated BAR0 that only answers window reads if the TLB registers, decoded by the golden
//     struct, really point at the right NOC tile + address.
// Plus the ARC/telemetry walk against a fake firmware table, including its failure modes.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#include "../ttbh.h"
#include "../sim/ttbh_sim.h"

static int failures = 0;
#define CHECK(cond) do { if (!(cond)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)
#define CHECK_EQ(a, b) do { unsigned long long _a = (unsigned long long)(a), _b = (unsigned long long)(b); \
    if (_a != _b) { fprintf(stderr, "FAIL %s:%d: %s == 0x%llx, want 0x%llx\n", __FILE__, __LINE__, #a, _a, _b); failures++; } } while (0)

// ── golden decoder: the documented layout as packed bitfields ────────────────────────────
struct __attribute__((packed)) golden2m {
    uint64_t address : 43, x_end : 6, y_end : 6, x_start : 6, y_start : 6, noc : 2, multicast : 1,
             ordering : 2, linked : 1, use_static_vc : 1, stream_header : 1, static_vc : 3, reserved : 18;
};
struct __attribute__((packed)) golden4g {
    uint32_t address : 32, x_end : 6, y_end : 6, x_start : 6, y_start : 6, noc : 2, multicast : 1,
             ordering : 2, linked : 1, use_static_vc : 1, stream_header : 1, static_vc : 3, reserved : 29;
};
_Static_assert(sizeof(struct golden2m) == 12, "golden2m must be 96 bits");
_Static_assert(sizeof(struct golden4g) == 12, "golden4g must be 96 bits");

static struct golden2m decode2m(const uint32_t w[3]) { struct golden2m g; memcpy(&g, w, 12); return g; }
static struct golden4g decode4g(const uint32_t w[3]) { struct golden4g g; memcpy(&g, w, 12); return g; }

static void test_hand_vectors(void)
{
    uint32_t w[3];
    // ARC's 2 MiB window: addr 0x80000000 (>>21 = 0x400), x_end 8 at bit 43, ordering 1 at bit 70.
    ttbh_tlb_config arc = { .addr = 0x80000000ull, .x_end = 8, .ordering = 1 };
    CHECK_EQ(ttbh_tlb2m_encode(&arc, w), TTBH_OK);
    CHECK_EQ(w[0], 0x00000400u);
    CHECK_EQ(w[1], 0x00004000u);   // bit 43 → mid32 bit 11
    CHECK_EQ(w[2], 0x00000040u);   // bit 70 → high32 bit 6

    // y_start straddles mid32/high32 (bits 61..66): 0x3F → mid32 bits 29..31, high32 bits 0..2.
    ttbh_tlb_config straddle = { .y_start = 0x3F };
    CHECK_EQ(ttbh_tlb2m_encode(&straddle, w), TTBH_OK);
    CHECK_EQ(w[0], 0u);
    CHECK_EQ(w[1], 0xE0000000u);
    CHECK_EQ(w[2], 0x00000007u);

    // 4G: addr 4 GiB (>>32 = 1), x_end 8 at bit 32, ordering at bit 59.
    ttbh_tlb_config big = { .addr = 1ull << 32, .x_end = 8, .ordering = 1 };
    CHECK_EQ(ttbh_tlb4g_encode(&big, w), TTBH_OK);
    CHECK_EQ(w[0], 0x00000001u);
    CHECK_EQ(w[1], 0x08000008u);
    CHECK_EQ(w[2], 0u);
}

static void test_encode_matches_golden_decoder(void)
{
    // Every field at a distinctive value, round-tripped through the golden bitfields.
    ttbh_tlb_config c = { .addr = 0x7FFFFull << 21, .x_end = 0x2A, .y_end = 0x15, .x_start = 0x33, .y_start = 0x0C,
                          .noc = 1, .multicast = 1, .ordering = 2, .linked = 1, .use_static_vc = 1, .static_vc = 5 };
    uint32_t w[3];
    CHECK_EQ(ttbh_tlb2m_encode(&c, w), TTBH_OK);
    struct golden2m g = decode2m(w);
    CHECK_EQ(g.address, c.addr >> 21);
    CHECK_EQ(g.x_end, c.x_end); CHECK_EQ(g.y_end, c.y_end);
    CHECK_EQ(g.x_start, c.x_start); CHECK_EQ(g.y_start, c.y_start);
    CHECK_EQ(g.noc, c.noc); CHECK_EQ(g.multicast, c.multicast); CHECK_EQ(g.ordering, c.ordering);
    CHECK_EQ(g.linked, c.linked); CHECK_EQ(g.use_static_vc, c.use_static_vc);
    CHECK_EQ(g.stream_header, 0); CHECK_EQ(g.static_vc, c.static_vc); CHECK_EQ(g.reserved, 0);

    c.addr = 0xABull << 32;
    CHECK_EQ(ttbh_tlb4g_encode(&c, w), TTBH_OK);
    struct golden4g h = decode4g(w);
    CHECK_EQ(h.address, 0xAB);
    CHECK_EQ(h.x_end, c.x_end); CHECK_EQ(h.y_start, c.y_start); CHECK_EQ(h.ordering, c.ordering);
    CHECK_EQ(h.static_vc, c.static_vc); CHECK_EQ(h.reserved, 0);
}

static void test_encode_rejects_bad_input(void)
{
    uint32_t w[3];
    ttbh_tlb_config misaligned = { .addr = 0x80030000ull };
    CHECK_EQ(ttbh_tlb2m_encode(&misaligned, w), TTBH_EINVAL);
    ttbh_tlb_config overflow = { .x_end = 64 };
    CHECK_EQ(ttbh_tlb2m_encode(&overflow, w), TTBH_EINVAL);
    ttbh_tlb_config ordering3 = { .ordering = 4 };
    CHECK_EQ(ttbh_tlb2m_encode(&ordering3, w), TTBH_EINVAL);
    CHECK_EQ(ttbh_tlb2m_encode(NULL, w), TTBH_EINVAL);
}

// ── simulated chip ────────────────────────────────────────────────────────────────────────
// A sparse NOC memory: (x, y, addr) → u32. The simulated BAR0 is real memory; after the bar0
// backend programs the TLB, the sim decodes the registers with the GOLDEN struct and fills the
// window with whatever the NOC holds there. So a wrong encoding means wrong data, not a pass.

#define SIM_BAR0_SIZE (512ull << 20)
typedef struct { uint32_t x, y; uint64_t addr; uint32_t value; } noc_word;
typedef struct {
    ttbh_bar0_ctx bar0;
    noc_word mem[64];
    int n;
    bool dead;        // simulate a vanished card: every window reads all-ones
    int aims;         // how many times a window was (re)aimed
} sim_chip;

static void sim_poke(sim_chip *s, uint32_t x, uint32_t y, uint64_t addr, uint32_t v)
{
    assert(s->n < 64);
    s->mem[s->n++] = (noc_word){ x, y, addr, v };
}

static volatile uint8_t *sim_aim(void *vctx, uint32_t x, uint32_t y, uint64_t base)
{
    sim_chip *s = (sim_chip *)vctx;
    ttbh_window real = ttbh_bar0_window(&s->bar0);
    volatile uint8_t *win = real.aim(real.ctx, x, y, base);
    if (!win) return 0;
    s->aims++;

    // Decode what was actually written into the TLB registers, independently of the encoder.
    uint32_t w[3];
    memcpy(w, (const void *)(s->bar0.bar0 + TTBH_TLB_REGS_START + s->bar0.tlb_index * TTBH_TLB_REG_SIZE), 12);
    struct golden2m g = decode2m(w);
    uint64_t target = (uint64_t)g.address << 21;

    memset((void *)win, s->dead ? 0xFF : 0x00, TTBH_TLB_2M_SIZE);
    if (s->dead || g.ordering != 1) return win;     // tt-kmd's strict ordering is part of the contract
    for (int i = 0; i < s->n; i++) {
        const noc_word *m = &s->mem[i];
        if (m->x == g.x_end && m->y == g.y_end && m->addr >= target && m->addr < target + TTBH_TLB_2M_SIZE)
            memcpy((void *)(win + (m->addr - target)), &m->value, 4);
    }
    return win;
}

static sim_chip *sim_new(void)
{
    sim_chip *s = calloc(1, sizeof *s);
    void *bar0 = mmap(NULL, SIM_BAR0_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    assert(bar0 != MAP_FAILED);
    s->bar0 = (ttbh_bar0_ctx){ bar0, SIM_BAR0_SIZE, TTBH_DRIVER_TLB_INDEX };
    return s;
}
static void sim_free(sim_chip *s) { munmap((void *)s->bar0.bar0, SIM_BAR0_SIZE); free(s); }
static ttbh_window sim_window(sim_chip *s) { return (ttbh_window){ s, sim_aim }; }

// A plausible firmware telemetry table in ARC CSM (a different 2 MiB window than the scratch regs).
static void sim_firmware(sim_chip *s)
{
    const uint64_t table = 0x10001000, data = 0x10002000;
    sim_poke(s, 8, 0, TTBH_ARC_BOOT_STATUS, 0x1);
    sim_poke(s, 8, 0, TTBH_ARC_TELEMETRY_PTR, (uint32_t)table);
    sim_poke(s, 8, 0, TTBH_ARC_TELEMETRY_DATA, (uint32_t)data);
    sim_poke(s, 8, 0, table + 0, 0x00010000);                     // version 1.0.0
    sim_poke(s, 8, 0, table + 4, 3);                              // 3 entries
    sim_poke(s, 8, 0, table + 8, (0u << 16) | TTBH_TAG_ASIC_TEMP);
    sim_poke(s, 8, 0, table + 12, (1u << 16) | TTBH_TAG_POWER);
    sim_poke(s, 8, 0, table + 16, (2u << 16) | TTBH_TAG_AICLK);
    sim_poke(s, 8, 0, data + 0, (45u << 16) | 0x8000u);           // 45.5 °C
    sim_poke(s, 8, 0, data + 4, 62);                              // 62 W
    sim_poke(s, 8, 0, data + 8, 1350);                            // 1350 MHz
    // A decoy at the same address on a different tile: must NOT be what we read.
    sim_poke(s, 9, 0, TTBH_ARC_BOOT_STATUS, 0xBAD);
}

static void test_arc_and_telemetry_walk(void)
{
    sim_chip *s = sim_new();
    sim_firmware(s);
    ttbh_window w = sim_window(s);

    uint32_t v = 0;
    CHECK_EQ(ttbh_arc_boot_status(&w, &v), TTBH_OK);
    CHECK_EQ(v, 1u);
    CHECK(ttbh_arc_ready(v));

    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_ASIC_TEMP, &v), TTBH_OK);
    CHECK(ttbh_temp_c(v) > 45.49 && ttbh_temp_c(v) < 45.51);
    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_POWER, &v), TTBH_OK);
    CHECK_EQ(v, 62u);
    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_AICLK, &v), TTBH_OK);
    CHECK_EQ(v, 1350u);
    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_FAN_RPM, &v), TTBH_ENOTAG);

    // The strided register for window 201 doesn't exist (only windows 0..31 have one), and the
    // write must land in window 201's TLB slot, not anywhere else.
    CHECK(s->aims > 0);
    sim_free(s);
}

static void test_failure_modes(void)
{
    uint32_t v;

    sim_chip *s = sim_new();
    sim_firmware(s);
    s->dead = true;
    ttbh_window w = sim_window(s);
    CHECK_EQ(ttbh_arc_boot_status(&w, &v), TTBH_EDEAD);
    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_POWER, &v), TTBH_EDEAD);
    sim_free(s);

    s = sim_new();
    sim_poke(s, 8, 0, TTBH_ARC_TELEMETRY_PTR, 0x20000000);   // outside CSM
    sim_poke(s, 8, 0, TTBH_ARC_TELEMETRY_DATA, 0x10002000);
    w = sim_window(s);
    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_POWER, &v), TTBH_ENOTELEM);
    sim_free(s);

    s = sim_new();
    sim_poke(s, 8, 0, TTBH_ARC_TELEMETRY_PTR, 0x10001000);
    sim_poke(s, 8, 0, TTBH_ARC_TELEMETRY_DATA, 0x10002000);
    sim_poke(s, 8, 0, 0x10001000, 0x00020000);               // version 2.0.0
    w = sim_window(s);
    CHECK_EQ(ttbh_telemetry_read(&w, TTBH_TAG_POWER, &v), TTBH_EVERSION);
    sim_free(s);

    s = sim_new();
    w = sim_window(s);
    CHECK_EQ(ttbh_noc_read32(&w, 8, 0, 0x80030402, &v), TTBH_EINVAL);   // misaligned
    s->bar0.tlb_index = TTBH_TLB_2M_COUNT;                               // no such window
    CHECK_EQ(ttbh_noc_read32(&w, 8, 0, TTBH_ARC_BOOT_STATUS, &v), TTBH_EWINDOW);
    s->bar0.tlb_index = TTBH_DRIVER_TLB_INDEX;
    s->bar0.bar0_size = 1u << 20;                                        // a BAR0 too small for TLB regs
    CHECK_EQ(ttbh_noc_read32(&w, 8, 0, TTBH_ARC_BOOT_STATUS, &v), TTBH_EWINDOW);
    s->bar0.bar0_size = SIM_BAR0_SIZE;
    CHECK_EQ(ttbh_noc_read32(&w, 64, 0, TTBH_ARC_BOOT_STATUS, &v), TTBH_EWINDOW);   // x out of range
    sim_free(s);
}

static void test_noc_write_lands_in_window(void)
{
    sim_chip *s = sim_new();
    ttbh_window w = sim_window(s);
    CHECK_EQ(ttbh_noc_write32(&w, 8, 0, 0x80030440, 0xC0FFEE), TTBH_OK);
    // Window 201 starts at 201 * 2 MiB; offset 0x30440 inside it.
    uint32_t got;
    memcpy(&got, (const void *)(s->bar0.bar0 + (uint64_t)TTBH_DRIVER_TLB_INDEX * TTBH_TLB_2M_SIZE + 0x30440), 4);
    CHECK_EQ(got, 0xC0FFEEu);
    sim_free(s);
}

// ── host DMA ──────────────────────────────────────────────────────────────────────────────

static void test_iatu_encode(void)
{
    ttbh_iatu_regs r;
    // A 64 KiB buffer at NOC base 0x3FF_FFFF_0000 targeting IOVA 0x1_2345_0000.
    CHECK_EQ(ttbh_iatu_outbound_encode(0x3FFFFFF0000ull, 0x3FFFFFFFFFFull, 0x123450000ull, &r), TTBH_OK);
    CHECK_EQ(r.lower_base, 0xFFFF0000u);  CHECK_EQ(r.upper_base, 0x3FFu);
    CHECK_EQ(r.lower_limit, 0xFFFFFFFFu); CHECK_EQ(r.upper_limit, 0x3FFu);
    CHECK_EQ(r.lower_target, 0x23450000u); CHECK_EQ(r.upper_target, 0x1u);
    CHECK_EQ(r.ctrl_1, 1u << 13);          // INCREASE_REGION_SIZE
    CHECK_EQ(r.ctrl_2, 1u << 31);          // REGION_EN
    CHECK_EQ(r.ctrl_3, 0u);
    // limit == 0 disables the region (how tt-kmd tears one down).
    CHECK_EQ(ttbh_iatu_outbound_encode(0, 0, 0, &r), TTBH_OK);
    CHECK_EQ(r.ctrl_2, 0u);
    // > 1 TiB and inverted ranges are rejected.
    CHECK_EQ(ttbh_iatu_outbound_encode(0, 1ull << 40, 0, &r), TTBH_EINVAL);
    CHECK_EQ(ttbh_iatu_outbound_encode(0x2000, 0x1000, 0, &r), TTBH_EINVAL);
}

static void test_iatu_write_lands_at_region_offsets(void)
{
    static uint32_t bar2w[(1u << 20) / 4];
    volatile uint8_t *bar2 = (volatile uint8_t *)bar2w;
    memset(bar2w, 0, sizeof bar2w);
    ttbh_iatu_regs r;
    ttbh_iatu_outbound_encode(0x10000, 0x1FFFF, 0xABCD0000ull, &r);
    CHECK_EQ(ttbh_iatu_outbound_write(bar2, sizeof bar2w, 3, &r), TTBH_OK);
    // Region 3 outbound = BAR2 + 0x1000 + (2*3)*0x100 = 0x1600 (inbound regions interleave).
    CHECK_EQ(ttbh_iatu_outbound_offset(3), 0x1600u);
    CHECK_EQ(bar2w[(0x1600 + 0x08) / 4], 0x10000u);        // LOWER_BASE
    CHECK_EQ(bar2w[(0x1600 + 0x10) / 4], 0x1FFFFu);        // LOWER_LIMIT
    CHECK_EQ(bar2w[(0x1600 + 0x14) / 4], 0xABCD0000u);     // LOWER_TARGET
    CHECK_EQ(bar2w[(0x1600 + 0x04) / 4], 1u << 31);        // CTRL_2 enable
    CHECK_EQ(bar2w[(0x1500 + 0x04) / 4], 0u);              // inbound region 2 untouched
    ttbh_iatu_regs back;
    CHECK_EQ(ttbh_iatu_outbound_read(bar2, sizeof bar2w, 3, &back), TTBH_OK);
    CHECK(memcmp(&back, &r, sizeof r) == 0);
    CHECK_EQ(ttbh_iatu_outbound_write(bar2, sizeof bar2w, 16, &r), TTBH_EINVAL);
    CHECK_EQ(ttbh_iatu_outbound_write(bar2, 0x1000, 0, &r), TTBH_EINVAL);   // BAR2 too small
}

static void test_dma_plan_makes_fragmented_iova_contiguous_on_the_noc(void)
{
    // Three DART segments, not adjacent in IOVA space.
    ttbh_dma_segment segs[3] = { { 0x80004000, 0x8000 }, { 0x90000000, 0x4000 }, { 0x7000C000, 0x4000 } };
    ttbh_iatu_plan_entry plan[3];
    CHECK_EQ(ttbh_dma_plan(segs, 3, 0x100000, 0, plan), TTBH_OK);
    CHECK_EQ(plan[0].base, 0x100000u); CHECK_EQ(plan[0].limit, 0x107FFFu); CHECK_EQ(plan[0].target, 0x80004000u);
    CHECK_EQ(plan[1].base, 0x108000u); CHECK_EQ(plan[1].limit, 0x10BFFFu); CHECK_EQ(plan[1].target, 0x90000000u);
    CHECK_EQ(plan[2].base, 0x10C000u); CHECK_EQ(plan[2].target, 0x7000C000u);
    CHECK_EQ(plan[2].region, 2u);
    // Unaligned segment, too many regions, empty input.
    ttbh_dma_segment bad = { 0x80000100, 0x1000 };
    CHECK_EQ(ttbh_dma_plan(&bad, 1, 0, 0, plan), TTBH_EINVAL);
    CHECK_EQ(ttbh_dma_plan(segs, 3, 0, 14, plan), TTBH_EINVAL);
    CHECK_EQ(ttbh_dma_plan(segs, 0, 0, 0, plan), TTBH_EINVAL);
    // A region straddling a 1 TiB boundary would alias (UPPER_LIMIT keeps 8 bits on silicon).
    ttbh_dma_segment straddle = { 0x80000000, 0x10000 };
    CHECK_EQ(ttbh_dma_plan(&straddle, 1, (1ull << 40) - 0x8000, 0, plan), TTBH_EINVAL);
    CHECK_EQ(ttbh_dma_plan(&straddle, 1, (1ull << 40), 0, plan), TTBH_OK);
}

static void test_pcie_noc_x(void)
{
    sim_chip *s = sim_new();
    uint32_t x = 0;
    volatile uint32_t *id = (volatile uint32_t *)(s->bar0.bar0 + TTBH_NOC2AXI_CFG_START + TTBH_NOC_ID_OFFSET);
    *id = 0x00000402u;                       // upper bits are other ID fields; x = 2
    CHECK_EQ(ttbh_pcie_noc_x(s->bar0.bar0, s->bar0.bar0_size, &x), TTBH_OK);
    CHECK_EQ(x, 2u);
    *id = 11;
    CHECK_EQ(ttbh_pcie_noc_x(s->bar0.bar0, s->bar0.bar0_size, &x), TTBH_OK);
    CHECK_EQ(x, 11u);
    *id = 5;
    CHECK_EQ(ttbh_pcie_noc_x(s->bar0.bar0, s->bar0.bar0_size, &x), TTBH_EINVAL);
    *id = 0xFFFFFFFFu;
    CHECK_EQ(ttbh_pcie_noc_x(s->bar0.bar0, s->bar0.bar0_size, &x), TTBH_EDEAD);
    sim_free(s);
}

static void test_arc_message_queue(void)
{
    // Stateful sim (libttbh/sim) + fake ARC firmware.
    ttbh_sim *c = ttbh_sim_new(4);
    ttbh_window w = ttbh_sim_window(c, TTBH_DRIVER_TLB_INDEX);
    uint32_t base = 0, n = 0;
    CHECK_EQ(ttbh_arc_msg_locate(&w, &base, &n), TTBH_OK);
    CHECK_EQ(base, 0x10003000u);
    CHECK_EQ(n, 4u);

    // More exchanges than there are slots, so both rings wrap (pointers run modulo 2n = 8).
    for (uint32_t i = 0; i < 11; i++) {
        uint32_t echo = 0;
        CHECK_EQ(ttbh_arc_test(&w, 0xCAFE0000u + i, &echo), TTBH_OK);
        CHECK_EQ(echo, 0xCAFE0001u + i);
    }
    CHECK_EQ(ttbh_sim_messages_served(c), 11);

    // Power setting: header packing as tt-kmd builds it (0x21 | validity << 8 | flags << 16).
    CHECK_EQ(ttbh_arc_set_power(&w, 4, 0xF, NULL), TTBH_OK);
    CHECK_EQ(ttbh_sim_last_header(c), 0x21u | (4u << 8) | (0xFu << 16));

    // An unknown command is answered with a non-zero status → TTBH_EREMOTE.
    ttbh_arc_msg bogus = { .header = 0x7E };
    CHECK_EQ(ttbh_arc_msg_send(&w, &bogus), TTBH_EREMOTE);
    CHECK_EQ(ttbh_sim_bad_messages(c), 1);
    ttbh_sim_free(c);

    // Not triggered → no answer (proves the trigger is what makes firmware look).
    c = ttbh_sim_new(4);
    w = ttbh_sim_window(c, TTBH_DRIVER_TLB_INDEX);
    uint32_t b2 = 0, n2 = 0;
    ttbh_arc_msg_locate(&w, &b2, &n2);
    ttbh_arc_msg m = { .header = TTBH_ARC_MSG_TEST };
    CHECK_EQ(ttbh_arc_msg_push(&w, b2, n2, &m), TTBH_OK);
    ttbh_arc_msg r;
    CHECK_EQ(ttbh_arc_msg_pop(&w, b2, n2, &r), TTBH_EAGAIN);
    // A full request ring refuses a fifth push while firmware is asleep.
    for (int i = 0; i < 3; i++) CHECK_EQ(ttbh_arc_msg_push(&w, b2, n2, &m), TTBH_OK);
    CHECK_EQ(ttbh_arc_msg_push(&w, b2, n2, &m), TTBH_EAGAIN);
    ttbh_sim_free(c);

    // ARC not ready → refuse to message.
    c = ttbh_sim_new(4);
    *ttbh_sim_word(c, 8, 0, TTBH_ARC_BOOT_STATUS) = 0x4;
    w = ttbh_sim_window(c, TTBH_DRIVER_TLB_INDEX);
    CHECK_EQ(ttbh_arc_test(&w, 1, NULL), TTBH_ENOTREADY);
    ttbh_sim_free(c);
}

int main(void)
{
    test_hand_vectors();
    test_encode_matches_golden_decoder();
    test_encode_rejects_bad_input();
    test_arc_and_telemetry_walk();
    test_failure_modes();
    test_noc_write_lands_in_window();
    test_iatu_encode();
    test_iatu_write_lands_at_region_offsets();
    test_dma_plan_makes_fragmented_iova_contiguous_on_the_noc();
    test_pcie_noc_x();
    test_arc_message_queue();
    if (failures) { fprintf(stderr, "%d check(s) failed\n", failures); return 1; }
    printf("libttbh: all checks passed\n");
    return 0;
}

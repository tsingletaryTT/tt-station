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

int main(void)
{
    test_hand_vectors();
    test_encode_matches_golden_decoder();
    test_encode_rejects_bad_input();
    test_arc_and_telemetry_walk();
    test_failure_modes();
    test_noc_write_lands_in_window();
    if (failures) { fprintf(stderr, "%d check(s) failed\n", failures); return 1; }
    printf("libttbh: all checks passed\n");
    return 0;
}

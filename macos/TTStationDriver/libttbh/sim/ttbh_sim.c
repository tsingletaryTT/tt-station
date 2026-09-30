// ttbh_sim.c — see ttbh_sim.h.

#ifndef _DEFAULT_SOURCE
#define _DEFAULT_SOURCE
#endif
#include "ttbh_sim.h"

#include <assert.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define PAGE TTBH_TLB_2M_SIZE
#define NWIN (TTBH_TLB_2M_COUNT)
#define MAX_PAGES 64
#define MSI_SENTINEL 0x5EE7A11Cu
#define BAR0_SIZE (512ull << 20)
#define BAR2_SIZE (1ull << 20)

// The documented TLB layout as packed bitfields: deliberately a different strategy from libttbh's
// shift-and-mask encoder (tt-kmd blackhole.c struct TLB_2M_REG describes the same layout).
struct __attribute__((packed)) golden2m {
    uint64_t address : 43, x_end : 6, y_end : 6, x_start : 6, y_start : 6, noc : 2, multicast : 1,
             ordering : 2, linked : 1, use_static_vc : 1, stream_header : 1, static_vc : 3, reserved : 18;
};
_Static_assert(sizeof(struct golden2m) == 12, "golden2m must be 96 bits");

typedef struct { uint32_t x, y; uint64_t base; uint8_t *data; } page;

struct ttbh_sim {
    uint8_t *bar0, *bar2;
    page pages[MAX_PAGES];
    int npages;
    int shown[NWIN];              // page index each window currently shows, -1 = none
    uint32_t qbase, entries;
    int served, bad;
    uint32_t last_header;
};

static page *find_page(ttbh_sim *s, uint32_t x, uint32_t y, uint64_t base)
{
    for (int i = 0; i < s->npages; i++)
        if (s->pages[i].x == x && s->pages[i].y == y && s->pages[i].base == base) return &s->pages[i];
    assert(s->npages < MAX_PAGES);
    page *p = &s->pages[s->npages++];
    *p = (page){ x, y, base, calloc(1, PAGE) };
    return p;
}

static uint8_t *window(ttbh_sim *s, uint32_t idx) { return s->bar0 + (uint64_t)idx * PAGE; }

static void save_all(ttbh_sim *s)
{
    for (uint32_t i = 0; i < NWIN; i++)
        if (s->shown[i] >= 0) memcpy(s->pages[s->shown[i]].data, window(s, i), PAGE);
}
static void load_all(ttbh_sim *s)
{
    for (uint32_t i = 0; i < NWIN; i++)
        if (s->shown[i] >= 0) memcpy(window(s, i), s->pages[s->shown[i]].data, PAGE);
}

uint32_t *ttbh_sim_word(ttbh_sim *s, uint32_t x, uint32_t y, uint64_t addr)
{
    page *p = find_page(s, x, y, addr & ~(uint64_t)(PAGE - 1));
    return (uint32_t *)(p->data + (addr & (PAGE - 1)));
}

// One firmware step against the backing store (windows must be saved first).
static void firmware(ttbh_sim *s)
{
    uint32_t *msi = ttbh_sim_word(s, 8, 0, TTBH_ARC_MSI_FIFO);
    if (*msi != 0) return;                               // not triggered
    *msi = MSI_SENTINEL;
    uint32_t n = s->entries, b = s->qbase;
    uint32_t *req_w = ttbh_sim_word(s, 8, 0, b + 0x00), *req_r = ttbh_sim_word(s, 8, 0, b + 0x10);
    uint32_t *res_w = ttbh_sim_word(s, 8, 0, b + 0x14);
    while ((*req_w - *req_r) % (2 * n) != 0) {
        uint32_t *req = ttbh_sim_word(s, 8, 0, b + TTBH_ARC_MSG_HEADER_SIZE + (*req_r % n) * 32u);
        uint32_t *res = ttbh_sim_word(s, 8, 0, b + TTBH_ARC_MSG_HEADER_SIZE + n * 32u + (*res_w % n) * 32u);
        memset(res, 0, 32);
        s->last_header = req[0];
        switch (req[0] & 0xFFu) {
        case TTBH_ARC_MSG_TEST: res[1] = req[1] + 1; break;
        case TTBH_ARC_MSG_POWER_SETTING: case TTBH_ARC_MSG_ASIC_STATE0: break;   // status 0
        default: res[0] = 1; s->bad++; break;
        }
        *req_r = (*req_r + 1) % (2 * n);
        *res_w = (*res_w + 1) % (2 * n);
        s->served++;
    }
}

void ttbh_sim_sync(ttbh_sim *s)
{
    save_all(s);
    firmware(s);
    load_all(s);
}

ttbh_sim_tlb ttbh_sim_decode_tlb(ttbh_sim *s, uint32_t idx)
{
    struct golden2m g;
    memcpy(&g, s->bar0 + TTBH_TLB_REGS_START + idx * TTBH_TLB_REG_SIZE, 12);
    return (ttbh_sim_tlb){ (uint64_t)g.address << 21, g.x_start, g.y_start, g.x_end, g.y_end, g.noc, g.multicast, g.ordering };
}

void ttbh_sim_retarget(ttbh_sim *s, uint32_t idx)
{
    assert(idx < NWIN);
    save_all(s);
    firmware(s);
    ttbh_sim_tlb t = ttbh_sim_decode_tlb(s, idx);
    // A window only reaches memory with tt-kmd's settings: strict ordering. (Multicast writes land
    // on the end tile's page here; the sim doesn't fan them out.)
    if (t.ordering == 1) s->shown[idx] = (int)(find_page(s, t.x_end, t.y_end, t.addr) - s->pages);
    else s->shown[idx] = -1;
    load_all(s);
    if (s->shown[idx] < 0) memset(window(s, idx), 0, PAGE);
}

typedef struct { ttbh_sim *sim; ttbh_bar0_ctx bar0; } sim_window_ctx;
static sim_window_ctx window_ctxs[NWIN];

static volatile uint8_t *sim_aim(void *vctx, uint32_t x, uint32_t y, uint64_t base)
{
    sim_window_ctx *c = vctx;
    ttbh_window real = ttbh_bar0_window(&c->bar0);
    volatile uint8_t *win = real.aim(real.ctx, x, y, base);
    if (win) ttbh_sim_retarget(c->sim, c->bar0.tlb_index);
    return win;
}

ttbh_window ttbh_sim_window(ttbh_sim *s, uint32_t idx)
{
    window_ctxs[idx] = (sim_window_ctx){ s, { s->bar0, BAR0_SIZE, idx } };
    return (ttbh_window){ &window_ctxs[idx], sim_aim };
}

ttbh_sim *ttbh_sim_new(uint32_t entries)
{
    ttbh_sim *s = calloc(1, sizeof *s);
    s->bar0 = mmap(NULL, BAR0_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    s->bar2 = calloc(1, BAR2_SIZE);
    assert(s->bar0 != MAP_FAILED && s->bar2);
    for (uint32_t i = 0; i < NWIN; i++) s->shown[i] = -1;
    s->qbase = 0x10003000u;
    s->entries = entries;

    // ARC: ready, message queue, trigger sentinel.
    *ttbh_sim_word(s, 8, 0, TTBH_ARC_BOOT_STATUS) = 0x5;
    *ttbh_sim_word(s, 8, 0, TTBH_ARC_MSG_QCB_PTR) = 0x10002F00u;
    *ttbh_sim_word(s, 8, 0, 0x10002F00u) = s->qbase;
    *ttbh_sim_word(s, 8, 0, 0x10002F04u) = entries;
    *ttbh_sim_word(s, 8, 0, TTBH_ARC_MSI_FIFO) = MSI_SENTINEL;

    // Telemetry table (P100A-shaped): temp 45.5 °C, 62 W, 715 mV, 26 A, 800 MHz, heartbeat, and
    // the topology tags blackhole-py requires: 12 Tensix columns (×10 = 120 cores), 7 GDDR banks.
    const uint64_t table = 0x10001000, data = 0x10002000;
    const uint32_t tags[][2] = { { TTBH_TAG_ASIC_TEMP, (45u << 16) | 0x8000u }, { TTBH_TAG_POWER, 62 },
                                 { TTBH_TAG_VCORE, 715 }, { TTBH_TAG_CURRENT, 26 }, { TTBH_TAG_AICLK, 800 },
                                 { TTBH_TAG_TIMER_HEARTBEAT, 1000 }, { 34, 0x0FFF }, { 36, 0x7F } };
    const uint32_t ntags = sizeof tags / sizeof tags[0];
    *ttbh_sim_word(s, 8, 0, TTBH_ARC_TELEMETRY_PTR) = (uint32_t)table;
    *ttbh_sim_word(s, 8, 0, TTBH_ARC_TELEMETRY_DATA) = (uint32_t)data;
    *ttbh_sim_word(s, 8, 0, table) = 0x00010000;        // version 1.0.0
    *ttbh_sim_word(s, 8, 0, table + 4) = ntags;
    for (uint32_t i = 0; i < ntags; i++) {
        *ttbh_sim_word(s, 8, 0, table + 8 + 4 * i) = (i << 16) | tags[i][0];
        *ttbh_sim_word(s, 8, 0, data + 4 * i) = tags[i][1];
    }
    // PCIe tile x = 11 (as on qb2-lab's p300c), in the NOC2AXI config block.
    *(uint32_t *)(s->bar0 + TTBH_NOC2AXI_CFG_START + TTBH_NOC_ID_OFFSET) = 11;
    return s;
}

void ttbh_sim_free(ttbh_sim *s)
{
    for (int i = 0; i < s->npages; i++) free(s->pages[i].data);
    munmap(s->bar0, BAR0_SIZE);
    free(s->bar2);
    free(s);
}

volatile uint8_t *ttbh_sim_bar0(ttbh_sim *s) { return s->bar0; }
uint64_t ttbh_sim_bar0_size(ttbh_sim *s) { (void)s; return BAR0_SIZE; }
volatile uint8_t *ttbh_sim_bar2(ttbh_sim *s) { return s->bar2; }
uint64_t ttbh_sim_bar2_size(ttbh_sim *s) { (void)s; return BAR2_SIZE; }
int ttbh_sim_messages_served(ttbh_sim *s) { return s->served; }
int ttbh_sim_bad_messages(ttbh_sim *s) { return s->bad; }
uint32_t ttbh_sim_last_header(ttbh_sim *s) { return s->last_header; }

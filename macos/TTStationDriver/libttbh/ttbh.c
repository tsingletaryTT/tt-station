// ttbh.c — see ttbh.h for the layering and where each fact comes from.

#include "ttbh.h"

const char *ttbh_strerror(int err)
{
    switch (err) {
    case TTBH_OK: return "ok";
    case TTBH_EINVAL: return "invalid argument";
    case TTBH_EWINDOW: return "could not aim a TLB window";
    case TTBH_EDEAD: return "all-ones read: link down or card gone";
    case TTBH_ENOTELEM: return "telemetry table missing or malformed";
    case TTBH_ENOTAG: return "telemetry tag not present";
    case TTBH_EVERSION: return "unsupported telemetry table version";
    default: return "unknown error";
    }
}

// ── bit packing ──────────────────────────────────────────────────────────────────────────
// A TLB config register is 96 bits written as three little-endian u32s. We build it in a
// 128-bit accumulator with explicit shifts, one field at a time, instead of relying on C
// bitfield layout (which is implementation-defined). The test decodes with bitfields as an
// independent check.

typedef struct { uint64_t lo, hi; } u96;   // bits 0..63 in lo, 64..127 in hi

static int put(u96 *r, unsigned pos, unsigned width, uint64_t value)
{
    if (width < 64 && (value >> width) != 0) return TTBH_EINVAL;   // field overflow
    for (unsigned i = 0; i < width; i++) {
        if (!((value >> i) & 1u)) continue;
        unsigned bit = pos + i;
        if (bit < 64) r->lo |= (1ull << bit);
        else r->hi |= (1ull << (bit - 64));
    }
    return TTBH_OK;
}

static void split(const u96 *r, uint32_t out[3])
{
    out[0] = (uint32_t)r->lo;
    out[1] = (uint32_t)(r->lo >> 32);
    out[2] = (uint32_t)r->hi;
}

// Field order after the address is identical for 2M and 4G registers; only the address width
// (and so every later position) differs: 43 bits for 2M (addr >> 21), 32 bits for 4G (addr >> 32).
static int encode(const ttbh_tlb_config *c, unsigned addr_shift, unsigned addr_bits, uint32_t out[3])
{
    if (!c || !out) return TTBH_EINVAL;
    if (c->addr & ((1ull << addr_shift) - 1)) return TTBH_EINVAL;   // window-aligned only
    u96 r = {0, 0};
    unsigned p = 0;
    int err = 0;
    err |= put(&r, p, addr_bits, c->addr >> addr_shift);  p += addr_bits;
    err |= put(&r, p, 6, c->x_end);                       p += 6;
    err |= put(&r, p, 6, c->y_end);                       p += 6;
    err |= put(&r, p, 6, c->x_start);                     p += 6;
    err |= put(&r, p, 6, c->y_start);                     p += 6;
    err |= put(&r, p, 2, c->noc);                         p += 2;
    err |= put(&r, p, 1, c->multicast);                   p += 1;
    err |= put(&r, p, 2, c->ordering);                    p += 2;
    err |= put(&r, p, 1, c->linked);                      p += 1;
    err |= put(&r, p, 1, c->use_static_vc);               p += 1;
    p += 1;                                               // stream_header: always 0 here
    err |= put(&r, p, 3, c->static_vc);                   p += 3;
    if (err) return TTBH_EINVAL;
    split(&r, out);
    return TTBH_OK;
}

int ttbh_tlb2m_encode(const ttbh_tlb_config *cfg, uint32_t out[3]) { return encode(cfg, 21, 43, out); }
int ttbh_tlb4g_encode(const ttbh_tlb_config *cfg, uint32_t out[3]) { return encode(cfg, 32, 32, out); }

// ── raw BAR0 window backend ───────────────────────────────────────────────────────────────

static volatile uint8_t *bar0_aim(void *vctx, uint32_t x, uint32_t y, uint64_t base)
{
    ttbh_bar0_ctx *ctx = (ttbh_bar0_ctx *)vctx;
    if (!ctx || !ctx->bar0 || ctx->tlb_index >= TTBH_TLB_2M_COUNT) return 0;
    uint64_t window_off = (uint64_t)ctx->tlb_index * TTBH_TLB_2M_SIZE;
    if (ctx->bar0_size < TTBH_BAR0_MIN_SIZE || window_off + TTBH_TLB_2M_SIZE > ctx->bar0_size) return 0;

    // Same settings tt-kmd uses for its own window: unicast to (x, y), strict ordering, NOC 0.
    ttbh_tlb_config cfg = { .addr = base, .x_end = (uint8_t)x, .y_end = (uint8_t)y, .ordering = 1 };
    uint32_t words[3];
    if (x > 63 || y > 63 || ttbh_tlb2m_encode(&cfg, words) != TTBH_OK) return 0;

    volatile uint32_t *regs = (volatile uint32_t *)(ctx->bar0 + TTBH_TLB_REGS_START + ctx->tlb_index * TTBH_TLB_REG_SIZE);
    regs[0] = words[0];
    regs[1] = words[1];
    regs[2] = words[2];
    if (ctx->tlb_index < TTBH_TLB_STRIDED_COUNT) {
        // Clear any strided (non-rectangular multicast) pattern left by someone else, as tt-kmd does.
        volatile uint32_t *strided = (volatile uint32_t *)(ctx->bar0 + TTBH_TLB_REGS_START + TTBH_TLB_STRIDED_REGS_OFF + ctx->tlb_index * 4u);
        *strided = 0;
    }
    // Read one register back before using the window. PCIe writes are posted, and a read from the
    // same device can't pass them, so this guarantees the TLB is programmed before the window is
    // accessed. tt-kmd doesn't do this. Over a Thunderbolt tunnel we'd rather be explicit.
    (void)regs[2];
    return ctx->bar0 + window_off;
}

ttbh_window ttbh_bar0_window(ttbh_bar0_ctx *ctx)
{
    ttbh_window w = { ctx, bar0_aim };
    return w;
}

// ── NOC access ────────────────────────────────────────────────────────────────────────────

static volatile uint32_t *locate(const ttbh_window *w, uint32_t x, uint32_t y, uint64_t addr)
{
    if (!w || !w->aim || (addr & 3u)) return 0;
    uint64_t base = addr & ~(uint64_t)(TTBH_TLB_2M_SIZE - 1);
    volatile uint8_t *win = w->aim(w->ctx, x, y, base);
    return win ? (volatile uint32_t *)(win + (addr - base)) : 0;
}

int ttbh_noc_read32(const ttbh_window *w, uint32_t x, uint32_t y, uint64_t addr, uint32_t *out)
{
    if (!out || (addr & 3u)) return TTBH_EINVAL;
    volatile uint32_t *p = locate(w, x, y, addr);
    if (!p) return TTBH_EWINDOW;
    *out = *p;
    return TTBH_OK;
}

int ttbh_noc_write32(const ttbh_window *w, uint32_t x, uint32_t y, uint64_t addr, uint32_t value)
{
    if (addr & 3u) return TTBH_EINVAL;
    volatile uint32_t *p = locate(w, x, y, addr);
    if (!p) return TTBH_EWINDOW;
    *p = value;
    return TTBH_OK;
}

// ── ARC + telemetry ───────────────────────────────────────────────────────────────────────

static int arc_read(const ttbh_window *w, uint64_t addr, uint32_t *out)
{
    return ttbh_noc_read32(w, TTBH_ARC_X, TTBH_ARC_Y, addr, out);
}

int ttbh_arc_boot_status(const ttbh_window *w, uint32_t *status)
{
    int err = arc_read(w, TTBH_ARC_BOOT_STATUS, status);
    if (err) return err;
    return *status == 0xFFFFFFFFu ? TTBH_EDEAD : TTBH_OK;
}

int ttbh_telemetry_read(const ttbh_window *w, uint16_t tag, uint32_t *raw)
{
    if (!raw) return TTBH_EINVAL;
    uint32_t base = 0, data = 0, version = 0, count = 0;
    int err;
    if ((err = arc_read(w, TTBH_ARC_TELEMETRY_PTR, &base))) return err;
    if ((err = arc_read(w, TTBH_ARC_TELEMETRY_DATA, &data))) return err;
    if (base == 0xFFFFFFFFu && data == 0xFFFFFFFFu) return TTBH_EDEAD;
    // Header is {version, entry count}, then `count` tag words.
    if (!ttbh_in_csm(base, 8) || !ttbh_in_csm(data, 4)) return TTBH_ENOTELEM;
    if ((err = arc_read(w, base, &version))) return err;
    if (((version >> 16) & 0xFFu) > 1) return TTBH_EVERSION;   // same gate as tt-kmd
    if ((err = arc_read(w, base + 4, &count))) return err;
    if (count > (1u << 16) || !ttbh_in_csm(base + 8, (uint64_t)count * 4)) return TTBH_ENOTELEM;

    for (uint32_t i = 0; i < count; i++) {
        uint32_t entry = 0;
        if ((err = arc_read(w, base + 8 + 4ull * i, &entry))) return err;
        if ((entry & 0xFFFFu) != tag) continue;
        uint64_t addr = data + 4ull * (entry >> 16);
        if (!ttbh_in_csm(addr, 4)) return TTBH_ENOTELEM;
        return arc_read(w, addr, raw);
    }
    return TTBH_ENOTAG;
}

// ── host DMA ──────────────────────────────────────────────────────────────────────────────

int ttbh_iatu_outbound_encode(uint64_t base, uint64_t limit, uint64_t target, ttbh_iatu_regs *out)
{
    if (!out) return TTBH_EINVAL;
    if (limit != 0) {
        if (limit < base) return TTBH_EINVAL;
        if (limit - base + 1 > TTBH_IATU_MAX_REGION_SIZE) return TTBH_EINVAL;   // iATU region max 1 TiB
    }
    out->lower_base = (uint32_t)base;
    out->upper_base = (uint32_t)(base >> 32);
    out->lower_target = (uint32_t)target;
    out->upper_target = (uint32_t)(target >> 32);
    out->lower_limit = (uint32_t)limit;
    out->upper_limit = (uint32_t)(limit >> 32);
    out->ctrl_1 = TTBH_IATU_INCREASE_REGION_SIZE;          // limit is 64-bit (upper_limit is honored)
    out->ctrl_2 = limit == 0 ? 0 : TTBH_IATU_REGION_EN;
    out->ctrl_3 = 0;
    return TTBH_OK;
}

static bool iatu_in_range(uint64_t bar2_size, uint32_t region)
{
    return region < TTBH_IATU_REGIONS && (uint64_t)ttbh_iatu_outbound_offset(region) + 0x24u <= bar2_size;
}

int ttbh_iatu_outbound_write(volatile uint8_t *bar2, uint64_t bar2_size, uint32_t region, const ttbh_iatu_regs *r)
{
    if (!bar2 || !r || !iatu_in_range(bar2_size, region)) return TTBH_EINVAL;
    volatile uint8_t *b = bar2 + ttbh_iatu_outbound_offset(region);
#define W(off, v) (*(volatile uint32_t *)(b + (off)) = (v))
    // Same order as tt-kmd: addresses first, enable (CTRL_2) after everything it gates.
    W(TTBH_IATU_LOWER_BASE, r->lower_base);
    W(TTBH_IATU_UPPER_BASE, r->upper_base);
    W(TTBH_IATU_LOWER_TARGET, r->lower_target);
    W(TTBH_IATU_UPPER_TARGET, r->upper_target);
    W(TTBH_IATU_LOWER_LIMIT, r->lower_limit);
    W(TTBH_IATU_UPPER_LIMIT, r->upper_limit);
    W(TTBH_IATU_CTRL_1, r->ctrl_1);
    W(TTBH_IATU_CTRL_2, r->ctrl_2);
    W(TTBH_IATU_CTRL_3, r->ctrl_3);
#undef W
    (void)*(volatile uint32_t *)(b + TTBH_IATU_CTRL_2);    // flush posted writes before first use
    return TTBH_OK;
}

int ttbh_iatu_outbound_read(volatile uint8_t *bar2, uint64_t bar2_size, uint32_t region, ttbh_iatu_regs *r)
{
    if (!bar2 || !r || !iatu_in_range(bar2_size, region)) return TTBH_EINVAL;
    volatile uint8_t *b = bar2 + ttbh_iatu_outbound_offset(region);
#define R(off) (*(volatile uint32_t *)(b + (off)))
    r->lower_base = R(TTBH_IATU_LOWER_BASE);
    r->upper_base = R(TTBH_IATU_UPPER_BASE);
    r->lower_target = R(TTBH_IATU_LOWER_TARGET);
    r->upper_target = R(TTBH_IATU_UPPER_TARGET);
    r->lower_limit = R(TTBH_IATU_LOWER_LIMIT);
    r->upper_limit = R(TTBH_IATU_UPPER_LIMIT);
    r->ctrl_1 = R(TTBH_IATU_CTRL_1);
    r->ctrl_2 = R(TTBH_IATU_CTRL_2);
    r->ctrl_3 = R(TTBH_IATU_CTRL_3);
#undef R
    return TTBH_OK;
}

int ttbh_pcie_noc_x(volatile uint8_t *bar0, uint64_t bar0_size, uint32_t *x)
{
    if (!bar0 || !x || bar0_size < (uint64_t)TTBH_NOC2AXI_CFG_START + TTBH_NOC_ID_OFFSET + 4) return TTBH_EINVAL;
    uint32_t id = *(volatile uint32_t *)(bar0 + TTBH_NOC2AXI_CFG_START + TTBH_NOC_ID_OFFSET);
    if (id == 0xFFFFFFFFu) return TTBH_EDEAD;
    *x = id & 0x3Fu;
    return (*x == 2 || *x == 11) ? TTBH_OK : TTBH_EINVAL;
}

int ttbh_dma_plan(const ttbh_dma_segment *segs, uint32_t n, uint64_t noc_base, uint32_t first_region,
                  ttbh_iatu_plan_entry *out)
{
    if (!segs || !out || n == 0 || first_region + n > TTBH_IATU_REGIONS) return TTBH_EINVAL;
    uint64_t base = noc_base;
    for (uint32_t i = 0; i < n; i++) {
        uint64_t len = segs[i].len;
        // 4 KiB is the iATU's minimum granule; macOS DART segments are 16 KiB-aligned anyway.
        if (len == 0 || (len & 0xFFFu) || (segs[i].addr & 0xFFFu) || (base & 0xFFFu)) return TTBH_EINVAL;
        if (len > TTBH_IATU_MAX_REGION_SIZE || base + len - 1 > TTBH_NOC_DMA_LIMIT || base + len < base) return TTBH_EINVAL;
        // Must stay inside one 1 TiB-aligned block: the iATU only compares limit bits 0..39
        // (see TTBH_IATU_UPPER_LIMIT_IMPL_MASK), so a crossing region would alias.
        if ((base >> 40) != ((base + len - 1) >> 40)) return TTBH_EINVAL;
        out[i] = (ttbh_iatu_plan_entry){ first_region + i, base, base + len - 1, segs[i].addr };
        base += len;
    }
    return TTBH_OK;
}

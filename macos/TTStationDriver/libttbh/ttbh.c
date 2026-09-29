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

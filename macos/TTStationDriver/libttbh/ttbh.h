// ttbh.h — a tiny, dependency-free C library for talking to a Tenstorrent Blackhole chip once
// you can reach its BARs. It's the logic the macOS dext path needs for spec milestone M2
// ("first NOC read") and the telemetry half of M4, written BEFORE we can load the dext, and
// verified two ways that need no signing key:
//
//   1. the bit-packing (TLB register encoding) against an independently written bitfield
//      decoder and hand-computed vectors, in a simulated BAR0 (tests/test_ttbh.c);
//   2. everything above the bit-packing (NOC addressing, ARC boot status, the telemetry tag-table
//      walk, value units) on a REAL Blackhole in a QuietBox via tt-kmd (tools/ttbh_kmd_check.c),
//      cross-checked against tt-kmd's own hwmon/sysfs readings.
//
// Layering (why there are two "window" backends):
//
//   ttbh_arc_* / ttbh_telemetry_*      ← chip facts: ARC at NOC (8,0), scratch regs, tag table
//        │ uses
//   ttbh_noc_read32 / ttbh_noc_write32 ← split an address into 2 MiB window + offset
//        │ uses
//   ttbh_window (aim a 2 MiB window at (x, y, addr))
//        ├── ttbh_bar0_window  — raw BAR0 TLB registers (macOS dext mapping; simulated in tests)
//        └── ttbh_kmd_window   — Linux tt-kmd ALLOCATE/CONFIGURE_TLB ioctls (QuietBox check)
//
// Facts come from tenstorrent/tt-kmd (blackhole.c, telemetry.h), cited at each constant. This
// file restates the register layout from those facts; it does not copy tt-kmd code.

#ifndef TTBH_H
#define TTBH_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// ── Blackhole BAR0 layout (tt-kmd blackhole.c) ─────────────────────────────────────────────
#define TTBH_TLB_2M_SHIFT          21u
#define TTBH_TLB_2M_SIZE           (1u << TTBH_TLB_2M_SHIFT)       // 2 MiB windows...
#define TTBH_TLB_2M_COUNT          202u                              // ...202 of them at BAR0+0
#define TTBH_TLB_4G_COUNT          8u                                // (4 GiB windows live in BAR4)
#define TTBH_TLB_REGS_START        0x1FC00000u                       // TLB config regs, in BAR0
#define TTBH_TLB_REG_SIZE          12u                               // low32, mid32, high32
#define TTBH_TLB_STRIDED_COUNT     32u                               // first 32 2M windows have one
#define TTBH_TLB_STRIDED_REGS_OFF  ((TTBH_TLB_2M_COUNT + TTBH_TLB_4G_COUNT) * TTBH_TLB_REG_SIZE)
#define TTBH_BAR0_MIN_SIZE         (TTBH_TLB_REGS_START + 0x1000u)   // must reach the TLB regs

// The window tt-kmd reserves for itself (the last 2 MiB window). On macOS our dext owns the
// whole device, so we use the same one: nothing else is programming TLBs there.
#define TTBH_DRIVER_TLB_INDEX      (TTBH_TLB_2M_COUNT - 1u)

// ── ARC firmware processor (tt-kmd blackhole.c / telemetry.h) ─────────────────────────────
#define TTBH_ARC_X                 8u
#define TTBH_ARC_Y                 0u
#define TTBH_RESET_SCRATCH(n)      (0x80030400ull + 4ull * (n))
#define TTBH_ARC_BOOT_STATUS       TTBH_RESET_SCRATCH(2)   // bit0: ready for messages
#define TTBH_ARC_TELEMETRY_DATA    TTBH_RESET_SCRATCH(12)
#define TTBH_ARC_TELEMETRY_PTR     TTBH_RESET_SCRATCH(13)
#define TTBH_ARC_CSM_BASE          0x10000000ull            // telemetry lives in ARC CSM
#define TTBH_ARC_CSM_SIZE          (1ull << 19)

// Telemetry tag IDs (tt-kmd telemetry.h, enum tt_telemetry_tags) — the subset we display.
enum ttbh_telemetry_tag {
    TTBH_TAG_BOARD_ID       = 1,
    TTBH_TAG_VCORE          = 6,    // mV
    TTBH_TAG_POWER          = 7,    // W
    TTBH_TAG_CURRENT        = 8,    // A
    TTBH_TAG_ASIC_TEMP      = 11,   // °C, 16.16 fixed point
    TTBH_TAG_AICLK          = 14,   // MHz
    TTBH_TAG_FAN_RPM        = 41,   // RPM; 0xFFFFFFFF = fan control disabled
    TTBH_TAG_TIMER_HEARTBEAT = 32,
};

// ── Errors ────────────────────────────────────────────────────────────────────────────────
enum ttbh_err {
    TTBH_OK = 0,
    TTBH_EINVAL = -1,       // bad argument (misaligned address, out-of-range field)
    TTBH_EWINDOW = -2,      // the window backend failed to aim
    TTBH_EDEAD = -3,        // read 0xFFFFFFFF where that can't be valid: link down / card gone
    TTBH_ENOTELEM = -4,     // telemetry table missing or malformed
    TTBH_ENOTAG = -5,       // tag not present in this firmware's table
    TTBH_EVERSION = -6,     // telemetry table version we don't understand
};
const char *ttbh_strerror(int err);

// ── TLB register encoding (pure) ──────────────────────────────────────────────────────────
typedef struct ttbh_tlb_config {
    uint64_t addr;          // NOC address; must be aligned to the window size
    uint8_t x_end, y_end;   // target NOC coordinate (unicast: start == 0, end == target)
    uint8_t x_start, y_start;
    uint8_t noc;            // 0 or 1
    uint8_t multicast;
    uint8_t ordering;       // 1 = strict (what tt-kmd uses for its own reads/writes)
    uint8_t linked;
    uint8_t use_static_vc;
    uint8_t static_vc;
} ttbh_tlb_config;

// Pack `cfg` into the three 32-bit TLB config words (low32, mid32, high32).
// Returns TTBH_EINVAL if addr isn't 2 MiB-aligned or a field overflows its width.
int ttbh_tlb2m_encode(const ttbh_tlb_config *cfg, uint32_t out[3]);
// Same for a 4 GiB (BAR4) window: addr must be 4 GiB-aligned.
int ttbh_tlb4g_encode(const ttbh_tlb_config *cfg, uint32_t out[3]);

// ── Window backends ───────────────────────────────────────────────────────────────────────
// A window backend points one 2 MiB window at (x, y, window_base) and returns the host pointer
// to the window's start. `window_base` is always 2 MiB-aligned (ttbh_noc_* guarantee it).
typedef struct ttbh_window {
    void *ctx;
    volatile uint8_t *(*aim)(void *ctx, uint32_t x, uint32_t y, uint64_t window_base);
} ttbh_window;

// Raw BAR0 backend: programs TLB register `tlb_index` directly. `bar0` is the host mapping of
// BAR0 (the dext's IOConnectMapMemory64 of memory type 0). Pure C, no OS calls.
typedef struct ttbh_bar0_ctx {
    volatile uint8_t *bar0;
    uint64_t bar0_size;
    uint32_t tlb_index;     // normally TTBH_DRIVER_TLB_INDEX
} ttbh_bar0_ctx;
ttbh_window ttbh_bar0_window(ttbh_bar0_ctx *ctx);

// ── NOC access ────────────────────────────────────────────────────────────────────────────
// 32-bit NOC read/write at (x, y, addr); addr must be 4-byte aligned. A read of 0xFFFFFFFF is
// returned as-is (it can be legitimate data); callers that know better check it.
int ttbh_noc_read32(const ttbh_window *w, uint32_t x, uint32_t y, uint64_t addr, uint32_t *out);
int ttbh_noc_write32(const ttbh_window *w, uint32_t x, uint32_t y, uint64_t addr, uint32_t value);

// ── ARC + telemetry ───────────────────────────────────────────────────────────────────────
// ARC_BOOT_STATUS; TTBH_EDEAD if it reads all-ones (a PCIe read from a gone device).
int ttbh_arc_boot_status(const ttbh_window *w, uint32_t *status);
static inline bool ttbh_arc_ready(uint32_t boot_status) { return (boot_status & 1u) != 0; }

// Look up one telemetry tag by walking the ARC firmware's tag table, exactly as tt-kmd does:
// PTR → {version, count, tags[count]}; each tag entry = (offset << 16) | tag_id; value lives at
// DATA + offset*4. All addresses are bounds-checked against the ARC CSM.
int ttbh_telemetry_read(const ttbh_window *w, uint16_t tag, uint32_t *raw);

// Unit conversions for display (tt-kmd telemetry hwmon semantics).
static inline double ttbh_temp_c(uint32_t raw) { return (double)(raw >> 16) + (double)(raw & 0xFFFFu) / 65536.0; }

static inline bool ttbh_in_csm(uint64_t addr, uint64_t len)
{
    return addr >= TTBH_ARC_CSM_BASE && len <= TTBH_ARC_CSM_SIZE && addr <= TTBH_ARC_CSM_BASE + TTBH_ARC_CSM_SIZE - len;
}

#ifdef __cplusplus
}
#endif
#endif // TTBH_H

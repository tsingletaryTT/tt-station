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

// A register writer (BAR offset, value), for backends whose BARs are function calls (ttsim).
typedef void (*ttbh_wr32_fn)(void *ctx, uint32_t offset, uint32_t value);

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

// ARC message queue (tt-kmd msgqueue.[ch], blackhole.c). The firmware publishes a queue control
// block at RESET_SCRATCH(11): {queue_base, info (low byte = entries)}. Each queue has a 32-byte
// header (REQ_WPTR +0, RES_RPTR +4, REQ_RPTR +0x10, RES_WPTR +0x14), then `n` request slots, then
// `n` response slots, 32 bytes each (header + 7 payload words). Pointers wrap at 2n. Writing 0 to
// ARC_MSI_FIFO makes the firmware look. A response header of 0 means success.
#define TTBH_ARC_MSG_QCB_PTR       TTBH_RESET_SCRATCH(11)
#define TTBH_ARC_MSI_FIFO          0x800B0000ull
#define TTBH_ARC_MSG_HEADER_SIZE   32u
#define TTBH_ARC_MSG_TIMEOUT_MS    1000u
#define TTBH_ARC_MSG_TEST          0x90u   // echo: response payload[0] = request payload[0] + 1
#define TTBH_ARC_MSG_ASIC_STATE0   0xA0u   // tt-kmd sends this at device init ("A0 state")
#define TTBH_ARC_MSG_POWER_SETTING 0x21u   // header = 0x21 | validity << 8 | flags << 16

typedef struct ttbh_arc_msg { uint32_t header; uint32_t payload[7]; } ttbh_arc_msg;

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

// ── Host DMA: PCIe tile + outbound iATU (tt-kmd blackhole.c / memory.c) ───────────────────
// The chip reaches host memory by NOC-accessing its active PCIe tile at
//   TTBH_NOC_PCIE_OFFSET + base
// where one of 16 outbound iATU regions (in BAR2) translates [base, limit] → a host DMA address
// (on macOS: the DART IOVA the dext's IODMACommand returns; on Linux: tt-kmd's dma_handle).
#define TTBH_NOC2AXI_CFG_START     0x1FD00000u                // in BAR0
#define TTBH_NOC_ID_OFFSET         0x4044u                    // within NOC2AXI cfg; bits 5:0 = NOC x
#define TTBH_PCIE_NOC_Y            0u
#define TTBH_NOC_PCIE_OFFSET       (4ull << 58)                // noc_pcie_offset
#define TTBH_NOC_DMA_LIMIT         ((1ull << 58) - 1)          // noc_dma_limit
#define TTBH_IATU_BASE             0x1000u                     // in BAR2
#define TTBH_IATU_REGIONS          16u
#define TTBH_IATU_REGION_STRIDE    0x100u
#define TTBH_IATU_MAX_REGION_SIZE  (1ull << 40)                // 1 TiB
#define TTBH_IATU_INCREASE_REGION_SIZE (1u << 13)              // CTRL_1
#define TTBH_IATU_REGION_EN        (1u << 31)                  // CTRL_2
// Observed on silicon (qb2-lab, p300c, 2026-09-29): UPPER_LIMIT implements only 8 bits. tt-kmd
// wrote upper_32_bits(limit) = 0x03ffffff and it read back 0x000000ff. So the hardware compares
// limit bits 0..39 only (the 1 TiB region maximum) and takes the higher bits from the base. A
// region that crossed a 1 TiB boundary would therefore alias. ttbh_dma_plan refuses those.
#define TTBH_IATU_UPPER_LIMIT_IMPL_MASK 0xFFu

// Register offsets within one outbound region, in the order tt-kmd writes them.
enum ttbh_iatu_reg {
    TTBH_IATU_CTRL_1 = 0x00, TTBH_IATU_CTRL_2 = 0x04, TTBH_IATU_LOWER_BASE = 0x08, TTBH_IATU_UPPER_BASE = 0x0C,
    TTBH_IATU_LOWER_LIMIT = 0x10, TTBH_IATU_LOWER_TARGET = 0x14, TTBH_IATU_UPPER_TARGET = 0x18,
    TTBH_IATU_CTRL_3 = 0x1C, TTBH_IATU_UPPER_LIMIT = 0x20,
};

typedef struct ttbh_iatu_regs {
    uint32_t lower_base, upper_base, lower_target, upper_target, lower_limit, upper_limit, ctrl_1, ctrl_2, ctrl_3;
} ttbh_iatu_regs;

// Pure: the nine register values for mapping [base, limit] → target. limit == 0 means "disable".
int ttbh_iatu_outbound_encode(uint64_t base, uint64_t limit, uint64_t target, ttbh_iatu_regs *out);
// BAR2 offset of outbound region `region`'s register block.
static inline uint32_t ttbh_iatu_outbound_offset(uint32_t region) { return TTBH_IATU_BASE + 2u * region * TTBH_IATU_REGION_STRIDE; }
// Write / read one outbound region through a BAR2 mapping. Write order matches tt-kmd.
int ttbh_iatu_outbound_write(volatile uint8_t *bar2, uint64_t bar2_size, uint32_t region, const ttbh_iatu_regs *r);
// Same register sequence through a writer callback (BAR2 offset, value), for function-call BARs.
int ttbh_iatu_outbound_write_with(ttbh_wr32_fn wr, void *ctx, uint32_t region, const ttbh_iatu_regs *r);
int ttbh_iatu_outbound_read(volatile uint8_t *bar2, uint64_t bar2_size, uint32_t region, ttbh_iatu_regs *r);

// The active PCIe tile's NOC x (Blackhole has two PCIe instances, at x = 2 and x = 11), read from
// the NOC2AXI config block in BAR0. TTBH_EINVAL if the register holds anything else.
int ttbh_pcie_noc_x(volatile uint8_t *bar0, uint64_t bar0_size, uint32_t *x);

// Plan the iATU regions for a host buffer that the DMA mapper returned as `n` segments (IOVA,
// length). Consecutive regions get ADJACENT NOC bases starting at `noc_base`, so the chip sees
// one contiguous range at TTBH_NOC_PCIE_OFFSET + noc_base even when the IOVA space is fragmented.
// Uses regions first_region.. first_region+n-1. Pure; the caller writes the plan with
// ttbh_iatu_outbound_write.
typedef struct ttbh_dma_segment { uint64_t addr, len; } ttbh_dma_segment;
typedef struct ttbh_iatu_plan_entry { uint32_t region; uint64_t base, limit, target; } ttbh_iatu_plan_entry;
int ttbh_dma_plan(const ttbh_dma_segment *segs, uint32_t n, uint64_t noc_base, uint32_t first_region,
                  ttbh_iatu_plan_entry *out);

// ── Errors ────────────────────────────────────────────────────────────────────────────────
enum ttbh_err {
    TTBH_OK = 0,
    TTBH_EINVAL = -1,       // bad argument (misaligned address, out-of-range field)
    TTBH_EWINDOW = -2,      // the window backend failed to aim
    TTBH_EDEAD = -3,        // read 0xFFFFFFFF where that can't be valid: link down / card gone
    TTBH_ENOTELEM = -4,     // telemetry table missing or malformed
    TTBH_ENOTAG = -5,       // tag not present in this firmware's table
    TTBH_EVERSION = -6,     // telemetry table version we don't understand
    TTBH_EAGAIN = -7,       // ARC queue full (push) or empty (pop)
    TTBH_ETIMEDOUT = -8,    // ARC didn't answer in time
    TTBH_EREMOTE = -9,      // ARC answered with a non-zero status
    TTBH_ENOTREADY = -10,   // ARC isn't ready for messages (boot status bit0 clear)
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

// The three TLB register words (and the strided-register clear for windows < 32) for window
// `index`, written through a callback (BAR0 offset, value). ttbh_bar0_window uses the same order.
int ttbh_tlb2m_program_with(ttbh_wr32_fn wr, void *ctx, uint32_t index, const ttbh_tlb_config *cfg);

// ── Window backends ───────────────────────────────────────────────────────────────────────
// A window backend points one 2 MiB window at (x, y, window_base) and returns the host pointer
// to the window's start. `window_base` is always 2 MiB-aligned (ttbh_noc_* guarantee it).
typedef struct ttbh_window {
    void *ctx;
    volatile uint8_t *(*aim)(void *ctx, uint32_t x, uint32_t y, uint64_t window_base);
    // Optional: for backends whose BARs are FUNCTION CALLS rather than mapped memory (ttsim's
    // libttsim_pci_mem_*). When set, the NOC helpers access the aimed window through these
    // (offset within the 2 MiB window) instead of dereferencing aim's return value, which then
    // only has to be non-NULL. Leave NULL for mapped BARs (the dext, the simulators).
    int (*rd32)(void *ctx, uint32_t offset, uint32_t *value);
    int (*wr32)(void *ctx, uint32_t offset, uint32_t value);
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

// Find the message queue: {base, entries}. Requires ARC ready. Never touches the rings.
int ttbh_arc_msg_locate(const ttbh_window *w, uint32_t *queue_base, uint32_t *entries);
// Ring operations, exactly tt-kmd's. push writes the 8 words then advances REQ_WPTR; pop reads a
// response then advances RES_RPTR. TTBH_EAGAIN when full/empty. Pure ring math plus CSM access.
int ttbh_arc_msg_push(const ttbh_window *w, uint32_t base, uint32_t entries, const ttbh_arc_msg *m);
int ttbh_arc_msg_pop(const ttbh_window *w, uint32_t base, uint32_t entries, ttbh_arc_msg *m);
// Synchronous send like tt-kmd's arc_msg_send_sync: drain stale responses, push, trigger, poll for
// the response (≤ TTBH_ARC_MSG_TIMEOUT_MS). `m` is replaced by the response. TTBH_EREMOTE if the
// response header (status) is non-zero. Caller must be the only sender: on Linux tt-kmd
// serializes its own senders but not us, so only use this on a device nothing else is messaging.
int ttbh_arc_msg_send(const ttbh_window *w, ttbh_arc_msg *m);
// Convenience: the TEST echo, and blackhole-py's SetPowerState (tt-kmd blackhole_set_power_state).
int ttbh_arc_test(const ttbh_window *w, uint32_t value, uint32_t *echo);
int ttbh_arc_set_power(const ttbh_window *w, uint8_t validity, uint16_t flags, const uint16_t settings[14]);

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

// TTBlackholeABI.h — the user-client contract shared by the dext and its userspace clients.
//
// This header is included from BOTH sides:
//   * the dext (DriverKit SDK, C++), and
//   * userspace clients (macOS SDK, plain C — the probe, later blackhole-py via ctypes).
// So it must stay dependency-free: fixed-width integers and enums only.
//
// Versioning: bump TTBH_ABI_VERSION on any incompatible change to a selector's inputs or
// outputs. Clients read it back from GetInfo and refuse to talk to a mismatched dext.
//
// See docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md ("User-client contract").

#ifndef TTBlackholeABI_h
#define TTBlackholeABI_h

#include <stdint.h>

#define TTBH_ABI_VERSION 2u   // v2 (2026-09-29): PrepareDMA / CompleteDMA

// The name the dext registers its service under (IOService::SetName). Userspace finds the
// driver with IOServiceNameMatching(TTBH_SERVICE_NAME).
#define TTBH_SERVICE_NAME "ttstation-blackhole"

// Tenstorrent Blackhole PCI identity (tt-kmd enumerate.h).
#define TTBH_PCI_VENDOR 0x1e52u
#define TTBH_PCI_DEVICE 0xb140u

// ExternalMethod selectors.
enum TTBHSelector {
    // in: none
    // out (scalars, in this order): see TTBHInfoIndex
    kTTBHGetInfo = 0,

    // in:  [0] config-space offset (< 4096), [1] width in bytes (1, 2 or 4)
    // out: [0] value
    kTTBHCfgRead = 1,

    // Make a client buffer reachable by the chip (spec M3).
    // in:  structure input = the buffer itself (a >4 KiB struct input arrives in the dext as an
    //      IOMemoryDescriptor over the client's pages; that's what gets DMA-mapped)
    // out: scalars [0] dma id (for CompleteDMA), [1] segment count;
    //      structure output = segment count × TTBHDMASegment (DART IOVA + length)
    // The first PrepareDMA turns on PCI bus mastering; until then the chip cannot write host memory.
    kTTBHPrepareDMA = 2,

    // in: [0] dma id from PrepareDMA. Unmaps it (IODMACommand::CompleteDMA).
    kTTBHCompleteDMA = 3,

    kTTBHSelectorCount
};

// Scalar output slots of kTTBHGetInfo.
enum TTBHInfoIndex {
    kTTBHInfoABIVersion = 0,
    kTTBHInfoVendorID,
    kTTBHInfoDeviceID,
    kTTBHInfoSubsysVendorID,
    kTTBHInfoSubsysID,
    kTTBHInfoBar0Size,   // 0 if the BAR is absent / not assigned
    kTTBHInfoBar2Size,
    kTTBHInfoBar4Size,
    kTTBHInfoCount
};

// CopyClientMemoryForType memory types: the value IS the BAR number. Only the BARs
// Blackhole actually implements are accepted.
//   BAR0 — 2 MiB TLB windows + TLB/NOC2AXI config registers
//   BAR2 — iATU (outbound address translation) registers
//   BAR4 — 4 GiB TLB windows (may be missing/tiny behind a Thunderbolt bridge)
enum TTBHMemoryType {
    kTTBHMemoryBar0 = 0,
    kTTBHMemoryBar2 = 2,
    kTTBHMemoryBar4 = 4,
};

// PrepareDMA limits and output format. 16 = the number of outbound iATU regions, since each
// segment needs one (libttbh ttbh_dma_plan).
#define TTBH_MAX_DMA_SEGMENTS 16u
#define TTBH_MAX_DMA_MAPPINGS 16u      // live PrepareDMA mappings per client
typedef struct TTBHDMASegment { uint64_t address; uint64_t length; } TTBHDMASegment;

// Blackhole register offsets used by the probe (tt-kmd blackhole.c).
#define TTBH_BAR0_TLB_REGS_START 0x1FC00000u   // 12 bytes per window: low32, mid32, high32
#define TTBH_TLB_REG_SIZE        12u
#define TTBH_BAR2_IATU_BASE      0x1000u

#endif /* TTBlackholeABI_h */

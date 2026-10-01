// tt-station-bh-probe — read-only smoke test for the TTStationDriver dext (spec milestone M1).
//
// What it does, in order, stopping at the first failure:
//   1. find the dext's service by name (TTBH_SERVICE_NAME) and IOServiceOpen it;
//   2. GetInfo  → ABI version, PCI IDs, and the BAR sizes the Thunderbolt bridge assigned;
//   3. CfgRead  → the command register (should show memory-space decode enabled);
//   4. map BAR0 and read TLB config register 0 (three u32s at BAR0 + 0x1FC00000);
//   5. map BAR2 and read the first iATU outbound region's CTRL_1 (BAR2 + 0x1000).
//
// Everything is a READ by default. `--noc` (spec M2) additionally programs ONE TLB register (window
// 201) to reach the ARC processor over the NOC and read its boot status + telemetry via libttbh.
// `--dma` (spec M3) DMA-maps a 64 KiB buffer through the dext (which turns on bus mastering),
// programs iATU regions, and runs a chip↔host loopback. It then disables the regions again.
// A dead link shows up as 0xFFFFFFFF on MMIO reads; the probe flags it.
//
// Two ways to run it:
//   * standalone `tt-station-bh-probe` (target in project.yml) — works when the dext grants
//     any client access (the SIP-off dev entitlements);
//   * `TTStationDriver probe` — the same code linked into the host app, which carries the
//     com.apple.developer.driverkit.userclient-access entitlement. This is the path that
//     works with a development-signed (SIP-on) dext, because a bare CLI tool can't embed the
//     provisioning profile that restricted entitlement needs. Built with TTBH_PROBE_NO_MAIN.

#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <IOKit/IOKitLib.h>

#include "../Shared/TTBlackholeABI.h"
#include "bh_probe.h"
#include "../libttbh/ttbh.h"

// The dext's copy of the outbound-iATU layout (it can't include libttbh) must match libttbh's.
_Static_assert(TTBH_BAR2_IATU_BASE == TTBH_IATU_BASE, "iATU base");
_Static_assert(TTBH_BAR2_IATU_REGIONS == TTBH_IATU_REGIONS, "iATU region count");
_Static_assert(TTBH_BAR2_IATU_STRIDE == 2u * TTBH_IATU_REGION_STRIDE, "iATU outbound stride");
_Static_assert(TTBH_BAR2_IATU_CTRL_2 == TTBH_IATU_CTRL_2, "iATU CTRL_2");

static const char * bar_label(uint64_t size, char * buf, size_t n)
{
    if (size == 0) snprintf(buf, n, "absent");
    else if (size >= (1ull << 30)) snprintf(buf, n, "%llu GiB", size >> 30);
    else if (size >= (1ull << 20)) snprintf(buf, n, "%llu MiB", size >> 20);
    else if (size >= (1ull << 10)) snprintf(buf, n, "%llu KiB", size >> 10);
    else snprintf(buf, n, "%llu B", size);
    return buf;
}

// Map `bar` into this process. Returns 0 on failure.
static mach_vm_address_t map_bar(io_connect_t conn, uint32_t bar, mach_vm_size_t * size)
{
    mach_vm_address_t addr = 0;
    kern_return_t kr = IOConnectMapMemory64(conn, bar, mach_task_self(), &addr, size, kIOMapAnywhere);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "  BAR%u map failed: 0x%08x\n", bar, kr);
        return 0;
    }
    return addr;
}

static uint32_t mmio_read32(mach_vm_address_t base, uint64_t offset)
{
    return *(volatile uint32_t *)(uintptr_t)(base + offset);
}

// ── --dma: spec M3 on the Mac ─────────────────────────────────────────────────────────────
// The same sequence ttbh-kmd-check --dma proved on a real Blackhole under tt-kmd, with the dext in
// tt-kmd's place:
//   dext PrepareDMA(host buffer) → DART segments
//   ttbh_dma_plan                → iATU regions with adjacent NOC bases from 0
//   ttbh_iatu_outbound_write     → BAR2 (the encoder was bit-compared against tt-kmd's registers)
//   chip → host: NOC write to the PCIe tile at TTBH_NOC_PCIE_OFFSET + 0x40 → lands in our buffer
//   host → chip: we write, the chip NOC-reads it back
// Then the regions are disabled and the mapping completed. Returns 0 on success.
#include <stdlib.h>
#include <time.h>
#define PROBE_DMA_SIZE (64u * 1024u)

static int dma_loopback(io_connect_t conn)
{
    int bad = 0;
    mach_vm_size_t size0 = 0, size2 = 0;
    mach_vm_address_t bar0 = map_bar(conn, kTTBHMemoryBar0, &size0);
    mach_vm_address_t bar2 = map_bar(conn, kTTBHMemoryBar2, &size2);
    if (!bar0 || !bar2) return 1;

    uint32_t pcie_x = 0;
    int err = ttbh_pcie_noc_x((volatile uint8_t *)(uintptr_t)bar0, size0, &pcie_x);
    printf("║  PCIe     tile x=%u y=%u  %s\n", pcie_x, TTBH_PCIE_NOC_Y, err ? ttbh_strerror(err) : "detected");
    if (err) { bad = 1; goto out_maps; }

    // A 16 KiB-aligned buffer (the DART page size) so segments are iATU-granule aligned.
    volatile uint32_t *host = NULL;
    if (posix_memalign((void **)&host, 16384, PROBE_DMA_SIZE) != 0) { bad = 1; goto out_maps; }
    memset((void *)host, 0, PROBE_DMA_SIZE);

    TTBHDMASegment segs[TTBH_MAX_DMA_SEGMENTS];
    size_t segs_size = sizeof segs;
    uint64_t out[2] = {0};
    uint32_t out_n = 2;
    kern_return_t kr = IOConnectCallMethod(conn, kTTBHPrepareDMA, NULL, 0, (const void *)host, PROBE_DMA_SIZE,
                                           out, &out_n, segs, &segs_size);
    if (kr != KERN_SUCCESS) { printf("║  DMA      PrepareDMA failed 0x%08x\n", kr); bad = 1; goto out_buf; }
    uint32_t dma_id = (uint32_t)out[0], nseg = (uint32_t)out[1];
    printf("║  DMA      %u KiB → %u DART segment(s), first IOVA 0x%llx\n", PROBE_DMA_SIZE / 1024, nseg,
           (unsigned long long)segs[0].address);

    ttbh_dma_segment plan_in[TTBH_MAX_DMA_SEGMENTS];
    ttbh_iatu_plan_entry plan[TTBH_MAX_DMA_SEGMENTS];
    for (uint32_t i = 0; i < nseg; i++) plan_in[i] = (ttbh_dma_segment){ segs[i].address, segs[i].length };
    const uint64_t noc_base = 0;          // we own the device: start the chip-visible range at 0
    if ((err = ttbh_dma_plan(plan_in, nseg, noc_base, 0, plan)) != TTBH_OK) {
        printf("║  DMA      cannot plan iATU regions: %s\n", ttbh_strerror(err)); bad = 1; goto out_dma;
    }
    for (uint32_t i = 0; i < nseg; i++) {
        ttbh_iatu_regs r;
        ttbh_iatu_outbound_encode(plan[i].base, plan[i].limit, plan[i].target, &r);
        ttbh_iatu_outbound_write((volatile uint8_t *)(uintptr_t)bar2, size2, plan[i].region, &r);
    }

    {
        ttbh_bar0_ctx ctx = { (volatile uint8_t *)(uintptr_t)bar0, size0, TTBH_DRIVER_TLB_INDEX };
        ttbh_window w = ttbh_bar0_window(&ctx);
        const uint64_t noc = TTBH_NOC_PCIE_OFFSET + noc_base;
        const uint32_t magic = 0xC0DE1234u ^ (uint32_t)time(NULL);

        err = ttbh_noc_write32(&w, pcie_x, TTBH_PCIE_NOC_Y, noc + 0x40, magic);
        for (int spin = 0; !err && host[0x40 / 4] != magic && spin < 5000000; spin++) { }
        int wrote = !err && host[0x40 / 4] == magic;
        printf("║  chip→host 0x%08x  %s\n", magic, wrote ? "landed in host memory" : "DID NOT ARRIVE");
        bad |= !wrote;

        host[0x80 / 4] = ~magic;
        __sync_synchronize();
        uint32_t back = 0;
        err = ttbh_noc_read32(&w, pcie_x, TTBH_PCIE_NOC_Y, noc + 0x80, &back);
        int read_ok = !err && back == ~magic;
        printf("║  host→chip 0x%08x  %s\n", back, read_ok ? "read back through the NOC" : "MISMATCH");
        bad |= !read_ok;
    }

    for (uint32_t i = 0; i < nseg; i++) {                 // disable (limit 0), as tt-kmd tears down
        ttbh_iatu_regs off;
        ttbh_iatu_outbound_encode(0, 0, 0, &off);
        ttbh_iatu_outbound_write((volatile uint8_t *)(uintptr_t)bar2, size2, plan[i].region, &off);
    }
out_dma:
    {
        uint64_t id_in = dma_id;
        IOConnectCallScalarMethod(conn, kTTBHCompleteDMA, &id_in, 1, NULL, NULL);
    }
out_buf:
    free((void *)host);
out_maps:
    IOConnectUnmapMemory64(conn, kTTBHMemoryBar0, mach_task_self(), bar0);
    IOConnectUnmapMemory64(conn, kTTBHMemoryBar2, mach_task_self(), bar2);
    return bad;
}

// ── telemetry --json: what `tt-station local` asks for once our dext has claimed the card ─────
// One JSON object on stdout: {"abi":N,"boot_status":…,"arc_ready":…,"asic_temp_c":…,"power_w":…,
// "vcore_mv":…,"current_a":…,"aiclk_mhz":…}. A value is null when its read failed. Exit 0 if the
// ARC answered, 1 if the dext couldn't be reached, 2 if the card looks dead. The reads are
// libttbh's, verified against tt-kmd hwmon on a real Blackhole (libttbh/tools/ttbh_kmd_check.c).
static void json_u32(const char *k, int ok, uint32_t v, int comma) { if (ok) printf("\"%s\":%u", k, v); else printf("\"%s\":null", k); if (comma) putchar(','); }

int ttbh_telemetry_json(void)
{
    io_service_t service = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceNameMatching(TTBH_SERVICE_NAME));
    if (service == IO_OBJECT_NULL) { fprintf(stderr, "no '%s' service\n", TTBH_SERVICE_NAME); return 1; }
    io_connect_t conn = IO_OBJECT_NULL;
    kern_return_t kr = IOServiceOpen(service, mach_task_self(), 0, &conn);
    IOObjectRelease(service);
    if (kr != KERN_SUCCESS) { fprintf(stderr, "IOServiceOpen failed: 0x%08x\n", kr); return 1; }

    uint64_t info[kTTBHInfoCount] = {0};
    uint32_t n = kTTBHInfoCount;
    if (IOConnectCallScalarMethod(conn, kTTBHGetInfo, NULL, 0, info, &n) != KERN_SUCCESS || info[kTTBHInfoABIVersion] != TTBH_ABI_VERSION) {
        fprintf(stderr, "dext ABI mismatch or GetInfo failed\n"); IOServiceClose(conn); return 1;
    }
    mach_vm_size_t size0 = 0;
    mach_vm_address_t bar0 = map_bar(conn, kTTBHMemoryBar0, &size0);
    if (!bar0) { IOServiceClose(conn); return 1; }

    ttbh_bar0_ctx ctx = { (volatile uint8_t *)(uintptr_t)bar0, size0, TTBH_DRIVER_TLB_INDEX };
    ttbh_window w = ttbh_bar0_window(&ctx);
    uint32_t boot = 0, temp = 0, power = 0, vcore = 0, current = 0, aiclk = 0;
    int boot_err = ttbh_arc_boot_status(&w, &boot);
    int ok = boot_err == TTBH_OK;
    int t_ok = ok && !ttbh_telemetry_read(&w, TTBH_TAG_ASIC_TEMP, &temp);
    int p_ok = ok && !ttbh_telemetry_read(&w, TTBH_TAG_POWER, &power);
    int v_ok = ok && !ttbh_telemetry_read(&w, TTBH_TAG_VCORE, &vcore);
    int c_ok = ok && !ttbh_telemetry_read(&w, TTBH_TAG_CURRENT, &current);
    int a_ok = ok && !ttbh_telemetry_read(&w, TTBH_TAG_AICLK, &aiclk);
    IOConnectUnmapMemory64(conn, kTTBHMemoryBar0, mach_task_self(), bar0);
    IOServiceClose(conn);

    printf("{\"abi\":%u,", TTBH_ABI_VERSION);
    json_u32("boot_status", ok, boot, 1);
    printf("\"arc_ready\":%s,", ok && ttbh_arc_ready(boot) ? "true" : "false");
    if (t_ok) printf("\"asic_temp_c\":%.3f,", ttbh_temp_c(temp)); else printf("\"asic_temp_c\":null,");
    json_u32("power_w", p_ok, power, 1);
    json_u32("vcore_mv", v_ok, vcore, 1);
    json_u32("current_a", c_ok, current, 1);
    json_u32("aiclk_mhz", a_ok, aiclk, 0);
    printf("}\n");
    return boot_err == TTBH_EDEAD ? 2 : ok ? 0 : 1;
}

int ttbh_probe_run(int flags)
{
    const int noc = flags & TTBH_PROBE_NOC, dma = flags & TTBH_PROBE_DMA;
    // ── 1. find + open ──────────────────────────────────────────────
    io_service_t service = IOServiceGetMatchingService(kIOMainPortDefault,
                                                       IOServiceNameMatching(TTBH_SERVICE_NAME));
    if (service == IO_OBJECT_NULL) {
        fprintf(stderr, "no '%s' service — is the dext activated and the card connected?\n"
                        "  check: systemextensionsctl list ; ioreg -r -n 'pci1e52,b140' -l\n",
                TTBH_SERVICE_NAME);
        return 1;
    }
    io_connect_t conn = IO_OBJECT_NULL;
    kern_return_t kr = IOServiceOpen(service, mach_task_self(), 0, &conn);
    IOObjectRelease(service);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "IOServiceOpen failed: 0x%08x (user-client entitlement?)\n", kr);
        return 1;
    }
    printf("╔══ tt-station-bh-probe\n");

    // ── 2. GetInfo ──────────────────────────────────────────────────
    uint64_t info[kTTBHInfoCount] = {0};
    uint32_t infoCount = kTTBHInfoCount;
    kr = IOConnectCallScalarMethod(conn, kTTBHGetInfo, NULL, 0, info, &infoCount);
    if (kr != KERN_SUCCESS) { fprintf(stderr, "GetInfo failed: 0x%08x\n", kr); return 1; }
    if (info[kTTBHInfoABIVersion] != TTBH_ABI_VERSION) {
        fprintf(stderr, "ABI mismatch: dext v%llu, probe v%u — rebuild both\n",
                info[kTTBHInfoABIVersion], TTBH_ABI_VERSION);
        return 1;
    }
    char b0[32], b2[32], b4[32];
    printf("║  device   %04llx:%04llx  subsys %04llx:%04llx  (abi v%llu)\n",
           info[kTTBHInfoVendorID], info[kTTBHInfoDeviceID],
           info[kTTBHInfoSubsysVendorID], info[kTTBHInfoSubsysID], info[kTTBHInfoABIVersion]);
    printf("║  BAR0     %s\n", bar_label(info[kTTBHInfoBar0Size], b0, sizeof b0));
    printf("║  BAR2     %s\n", bar_label(info[kTTBHInfoBar2Size], b2, sizeof b2));
    printf("║  BAR4     %s%s\n", bar_label(info[kTTBHInfoBar4Size], b4, sizeof b4),
           info[kTTBHInfoBar4Size] < (1ull << 32) ? "  → no 4 GiB TLB windows (spec risk R1)" : "");

    // ── 3. CfgRead: command register ───────────────────────────────
    uint64_t in[2] = { 0x04, 2 };  // PCI_COMMAND, 16-bit
    uint64_t cmd = 0; uint32_t one = 1;
    kr = IOConnectCallScalarMethod(conn, kTTBHCfgRead, in, 2, &cmd, &one);
    if (kr == KERN_SUCCESS)
        printf("║  COMMAND  0x%04llx  (memory space %s, bus master %s)\n", cmd,
               (cmd & 0x2) ? "on" : "OFF", (cmd & 0x4) ? "on" : "off");
    else
        fprintf(stderr, "CfgRead failed: 0x%08x\n", kr);

    // ── 4. BAR0: TLB config register 0 ─────────────────────────────
    int dead = 0;    // MMIO read back all-ones: link down, decode off, or card in reset
    int failed = 0;  // MMIO answered, but a check on top of it (ARC boot status, DMA) didn't pass
    mach_vm_size_t size0 = 0;
    mach_vm_address_t bar0 = map_bar(conn, kTTBHMemoryBar0, &size0);
    if (bar0 && size0 > TTBH_BAR0_TLB_REGS_START + TTBH_TLB_REG_SIZE) {
        uint32_t lo  = mmio_read32(bar0, TTBH_BAR0_TLB_REGS_START + 0);
        uint32_t mid = mmio_read32(bar0, TTBH_BAR0_TLB_REGS_START + 4);
        uint32_t hi  = mmio_read32(bar0, TTBH_BAR0_TLB_REGS_START + 8);
        dead |= (lo == 0xFFFFFFFFu && mid == 0xFFFFFFFFu && hi == 0xFFFFFFFFu);
        printf("║  TLB[0]   %08x %08x %08x\n", lo, mid, hi);

        // ── 4b. --noc: spec M2, the first NOC access (and first WRITE: one TLB register) ──
        // Aim the driver's 2 MiB window (index 201, the one tt-kmd reserves for itself) at the
        // ARC processor and read its boot status + telemetry through libttbh. That logic is
        // verified against tt-kmd on a real Blackhole (libttbh/tools/ttbh_kmd_check.c).
        if (noc && !dead) {
            ttbh_bar0_ctx ctx = { (volatile uint8_t *)(uintptr_t)bar0, size0, TTBH_DRIVER_TLB_INDEX };
            ttbh_window w = ttbh_bar0_window(&ctx);
            uint32_t boot = 0, raw = 0;
            int err = ttbh_arc_boot_status(&w, &boot);
            printf("║  ARC      boot 0x%08x  %s\n", boot,
                   err ? ttbh_strerror(err) : (ttbh_arc_ready(boot) ? "ready" : "not ready"));
            // Any error fails the probe; only EDEAD (all-ones reads) also means MMIO is dead. A
            // timeout or a bad tag table is a NOC/ARC failure on a link that otherwise works.
            if (err == TTBH_EDEAD) dead = 1;
            else if (err) failed = 1;
            if (!err) {
                if (!ttbh_telemetry_read(&w, TTBH_TAG_ASIC_TEMP, &raw)) printf("║  temp     %.1f °C\n", ttbh_temp_c(raw));
                if (!ttbh_telemetry_read(&w, TTBH_TAG_POWER, &raw))     printf("║  power    %u W\n", raw);
                if (!ttbh_telemetry_read(&w, TTBH_TAG_VCORE, &raw))     printf("║  vcore    %u mV\n", raw);
                if (!ttbh_telemetry_read(&w, TTBH_TAG_AICLK, &raw))     printf("║  aiclk    %u MHz\n", raw);
            }
        }
        IOConnectUnmapMemory64(conn, kTTBHMemoryBar0, mach_task_self(), bar0);
    } else if (bar0) {
        fprintf(stderr, "  BAR0 mapped only 0x%llx bytes — too small for TLB regs\n", size0);
    }

    // ── 5. BAR2: iATU outbound region 0 CTRL_1 ─────────────────────
    mach_vm_size_t size2 = 0;
    mach_vm_address_t bar2 = map_bar(conn, kTTBHMemoryBar2, &size2);
    if (bar2 && size2 > TTBH_BAR2_IATU_BASE + 4) {
        uint32_t ctrl1 = mmio_read32(bar2, TTBH_BAR2_IATU_BASE);
        dead |= (ctrl1 == 0xFFFFFFFFu);
        printf("║  iATU[0]  ctrl1 %08x\n", ctrl1);
        IOConnectUnmapMemory64(conn, kTTBHMemoryBar2, mach_task_self(), bar2);
    }

    if (dma && !dead && dma_loopback(conn) != 0) failed = 1;

    if (dead) printf("║  ⚠ all-ones MMIO reads: link down, memory decode off, or card in reset\n");
    printf("╚══ %s\n", dead ? "reachable config space, MMIO NOT confirmed"
                        : failed ? "MMIO works, but a NOC/ARC/DMA check FAILED (see above)" : "done");

    IOServiceClose(conn);
    return dead ? 2 : failed ? 3 : 0;
}

#ifndef TTBH_PROBE_NO_MAIN
#include <string.h>
int main(int argc, char **argv)
{
    int flags = 0;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--noc") == 0) flags |= TTBH_PROBE_NOC;
        else if (strcmp(argv[i], "--dma") == 0) flags |= TTBH_PROBE_DMA;
    }
    return ttbh_probe_run(flags);
}
#endif

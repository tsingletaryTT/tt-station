// tt-station-bh-probe — read-only smoke test for the TTStationDriver dext (spec milestone M1).
//
// What it does, in order, stopping at the first failure:
//   1. find the dext's service by name (TTBH_SERVICE_NAME) and IOServiceOpen it;
//   2. GetInfo  → ABI version, PCI IDs, and the BAR sizes the Thunderbolt bridge assigned;
//   3. CfgRead  → the command register (should show memory-space decode enabled);
//   4. map BAR0 and read TLB config register 0 (three u32s at BAR0 + 0x1FC00000);
//   5. map BAR2 and read the first iATU outbound region's CTRL_1 (BAR2 + 0x1000).
//
// Everything is a READ. Nothing here reprograms a TLB or touches the NOC — that is M2.
// A dead link shows up as 0xFFFFFFFF on MMIO reads; the probe flags it.
//
// Build: part of macos/TTStationDriver/project.yml (target tt-station-bh-probe), or
//   clang -O2 -Wall -framework IOKit -framework CoreFoundation Probe/bh_probe.c -o tt-station-bh-probe

#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <IOKit/IOKitLib.h>

#include "../Shared/TTBlackholeABI.h"

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

int main(void)
{
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
    int dead = 0;
    mach_vm_size_t size0 = 0;
    mach_vm_address_t bar0 = map_bar(conn, kTTBHMemoryBar0, &size0);
    if (bar0 && size0 > TTBH_BAR0_TLB_REGS_START + TTBH_TLB_REG_SIZE) {
        uint32_t lo  = mmio_read32(bar0, TTBH_BAR0_TLB_REGS_START + 0);
        uint32_t mid = mmio_read32(bar0, TTBH_BAR0_TLB_REGS_START + 4);
        uint32_t hi  = mmio_read32(bar0, TTBH_BAR0_TLB_REGS_START + 8);
        dead |= (lo == 0xFFFFFFFFu && mid == 0xFFFFFFFFu && hi == 0xFFFFFFFFu);
        printf("║  TLB[0]   %08x %08x %08x\n", lo, mid, hi);
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

    if (dead) printf("║  ⚠ all-ones MMIO reads: link down, memory decode off, or card in reset\n");
    printf("╚══ %s\n", dead ? "reachable config space, MMIO NOT confirmed" : "done");

    IOServiceClose(conn);
    return dead ? 2 : 0;
}

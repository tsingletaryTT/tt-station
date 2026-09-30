// broker_iokit.c — the REAL broker backend: BARs and DMA through tt-station's dext. Linked into the
// entitled host app (`TTStationDriver serve [SOCKET]`), which is the only kind of process a
// development-signed dext lets in. macOS only.
//
// DMA: the shm pages the broker created (and passed to the client) go to the dext as the >4 KiB
// structure input of kTTBHPrepareDMA, which arrives there as an IOMemoryDescriptor over exactly
// those pages. That is TinyGPU's approach (installer/Shared/server.c map_sysmem_fd), and it means the
// chip DMAs the very memory the Python client has mapped.

#include <stdio.h>
#include <string.h>
#include <IOKit/IOKitLib.h>

#include "broker.h"
#include "broker_iokit.h"
#include "../Shared/TTBlackholeABI.h"

typedef struct { io_connect_t conn; } iokit_ctx;

static int iokit_prepare(void *vctx, void *addr, uint64_t len, ttbh_dma_segment *segs, uint32_t *count, uint32_t *handle)
{
    iokit_ctx *c = vctx;
    TTBHDMASegment out[TTBH_MAX_DMA_SEGMENTS];
    size_t out_size = sizeof out;
    uint64_t scalars[2] = {0};
    uint32_t nscalars = 2;
    kern_return_t kr = IOConnectCallMethod(c->conn, kTTBHPrepareDMA, NULL, 0, addr, (size_t)len,
                                           scalars, &nscalars, out, &out_size);
    if (kr != KERN_SUCCESS) { fprintf(stderr, "ttbh broker: PrepareDMA failed 0x%08x\n", kr); return -1; }
    uint32_t n = (uint32_t)scalars[1];
    if (n > *count) {                                   // more segments than iATU regions to spend
        uint64_t id = scalars[0];
        IOConnectCallScalarMethod(c->conn, kTTBHCompleteDMA, &id, 1, NULL, NULL);
        fprintf(stderr, "ttbh broker: %u DMA segments exceed %u iATU regions\n", n, *count);
        return -1;
    }
    for (uint32_t i = 0; i < n; i++) segs[i] = (ttbh_dma_segment){ out[i].address, out[i].length };
    *count = n;
    *handle = (uint32_t)scalars[0];
    return 0;
}

static void iokit_complete(void *vctx, uint32_t handle)
{
    iokit_ctx *c = vctx;
    uint64_t id = handle;
    IOConnectCallScalarMethod(c->conn, kTTBHCompleteDMA, &id, 1, NULL, NULL);
}

int ttbh_broker_serve_iokit(const char *socket_path)
{
    io_service_t service = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceNameMatching(TTBH_SERVICE_NAME));
    if (service == IO_OBJECT_NULL) { fprintf(stderr, "no '%s' service: is the dext activated?\n", TTBH_SERVICE_NAME); return 1; }
    iokit_ctx ctx = { IO_OBJECT_NULL };
    kern_return_t kr = IOServiceOpen(service, mach_task_self(), 0, &ctx.conn);
    IOObjectRelease(service);
    if (kr != KERN_SUCCESS) { fprintf(stderr, "IOServiceOpen failed: 0x%08x\n", kr); return 1; }

    uint64_t info[kTTBHInfoCount] = {0};
    uint32_t n = kTTBHInfoCount;
    if (IOConnectCallScalarMethod(ctx.conn, kTTBHGetInfo, NULL, 0, info, &n) != KERN_SUCCESS ||
        info[kTTBHInfoABIVersion] != TTBH_ABI_VERSION) {
        fprintf(stderr, "dext ABI mismatch (want v%u)\n", TTBH_ABI_VERSION);
        IOServiceClose(ctx.conn);
        return 1;
    }

    mach_vm_address_t bar0 = 0, bar2 = 0;
    mach_vm_size_t size0 = 0, size2 = 0;
    if (IOConnectMapMemory64(ctx.conn, kTTBHMemoryBar0, mach_task_self(), &bar0, &size0, kIOMapAnywhere) != KERN_SUCCESS ||
        IOConnectMapMemory64(ctx.conn, kTTBHMemoryBar2, mach_task_self(), &bar2, &size2, kIOMapAnywhere) != KERN_SUCCESS) {
        fprintf(stderr, "mapping BAR0/BAR2 failed\n");
        IOServiceClose(ctx.conn);
        return 1;
    }

    broker_backend be = {
        .ctx = &ctx,
        .bar0 = (volatile uint8_t *)(uintptr_t)bar0, .bar0_size = size0,
        .bar2 = (volatile uint8_t *)(uintptr_t)bar2, .bar2_size = size2,
        .device_id = (uint16_t)info[kTTBHInfoDeviceID], .subsystem_id = (uint16_t)info[kTTBHInfoSubsysID],
        .prepare_dma = iokit_prepare, .complete_dma = iokit_complete,
    };
    int rc = broker_serve(&be, socket_path);

    IOConnectUnmapMemory64(ctx.conn, kTTBHMemoryBar0, mach_task_self(), bar0);
    IOConnectUnmapMemory64(ctx.conn, kTTBHMemoryBar2, mach_task_self(), bar2);
    IOServiceClose(ctx.conn);
    return rc;
}

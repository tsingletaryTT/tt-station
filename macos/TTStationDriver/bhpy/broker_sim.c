// broker_sim.c — `ttbh-broker-sim SOCKET`: the broker over libttbh's simulated Blackhole.
// Hardware-free, macOS or Linux. The Python client's tests run against this real C broker, so the
// protocol, fd passing, TLB packing and ARC/telemetry paths are exercised end to end; only the
// dext and the silicon are swapped out.
//
// Simulated DMA: prepare_dma hands back TWO fake IOVA segments for every buffer, so the iATU
// planner's "fragmented IOVA → contiguous NOC range" path is always exercised.

#include <stdio.h>
#include "broker.h"
#include "../libttbh/sim/ttbh_sim.h"

static int fake_prepare(void *ctx, void *addr, uint64_t len, ttbh_dma_segment *segs, uint32_t *count, uint32_t *handle)
{
    (void)ctx; (void)addr;
    static uint32_t next = 0;
    if (*count < 2 || len < 0x8000 || (len & 0x3FFF)) return -1;
    uint64_t half = (len / 2) & ~(uint64_t)0x3FFF;
    segs[0] = (ttbh_dma_segment){ 0x80000000ull + next * 0x10000000ull, half };
    segs[1] = (ttbh_dma_segment){ 0x70000000ull + next * 0x10000000ull, len - half };   // not adjacent
    *count = 2;
    *handle = next++;
    return 0;
}
static void fake_complete(void *ctx, uint32_t h) { (void)ctx; (void)h; }
static void retarget(void *ctx, uint32_t id) { ttbh_sim_retarget(ctx, id); }
static void sync_sim(void *ctx) { ttbh_sim_sync(ctx); }
static ttbh_window own(void *ctx) { return ttbh_sim_window(ctx, TTBH_DRIVER_TLB_INDEX); }

int main(int argc, char **argv)
{
    if (argc != 2) { fprintf(stderr, "usage: %s SOCKET\n", argv[0]); return 2; }
    ttbh_sim *sim = ttbh_sim_new(4);
    broker_backend be = {
        .ctx = sim,
        .bar0 = ttbh_sim_bar0(sim), .bar0_size = ttbh_sim_bar0_size(sim),
        .bar2 = ttbh_sim_bar2(sim), .bar2_size = ttbh_sim_bar2_size(sim),
        .device_id = 0xb140, .subsystem_id = 0x43,          // a P100A, like the owner's card
        .prepare_dma = fake_prepare, .complete_dma = fake_complete,
        .after_tlb_target = retarget, .sync = sync_sim, .own_window = own,
    };
    int rc = broker_serve(&be, argv[1]);
    ttbh_sim_free(sim);
    return rc;
}

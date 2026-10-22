// bh_probe.h — entry point of the read-only M1 probe, callable from C or (via the host app's
// bridging header) Swift. Returns 0 = all good, 1 = setup failure, 2 = MMIO looked dead,
// 3 = MMIO worked but a --noc/--dma check failed (ARC boot status error, DMA mismatch).
#ifndef bh_probe_h
#define bh_probe_h
// Flags: TTBH_PROBE_NOC = the M2 NOC read (ARC boot status + telemetry; writes one TLB register).
//        TTBH_PROBE_DMA = the M3 chip↔host DMA loopback (enables bus mastering, writes iATU regions).
#define TTBH_PROBE_NOC 1
#define TTBH_PROBE_DMA 2
int ttbh_probe_run(int flags);

// One-line JSON telemetry through the dext (for `tt-station local`). 0 = ARC answered.
int ttbh_telemetry_json(void);
#endif

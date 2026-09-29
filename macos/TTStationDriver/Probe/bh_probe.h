// bh_probe.h — entry point of the read-only M1 probe, callable from C or (via the host app's
// bridging header) Swift. Returns 0 = all good, 1 = setup failure, 2 = MMIO looked dead.
#ifndef bh_probe_h
#define bh_probe_h
// noc != 0: also do the M2 NOC read (ARC boot status + telemetry). Writes one TLB register.
int ttbh_probe_run(int noc);
#endif

// bh_probe.h — entry point of the read-only M1 probe, callable from C or (via the host app's
// bridging header) Swift. Returns 0 = all good, 1 = setup failure, 2 = MMIO looked dead.
#ifndef bh_probe_h
#define bh_probe_h
int ttbh_probe_run(void);
#endif

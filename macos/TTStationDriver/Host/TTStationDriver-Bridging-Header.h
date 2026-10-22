// Exposes the C probe (Probe/bh_probe.c, compiled with TTBH_PROBE_NO_MAIN) to main.swift.
#include "../Probe/bh_probe.h"
// The blackhole-py broker (bhpy/), served from this entitled app: `TTStationDriver serve`.
#include "../bhpy/broker_iokit.h"

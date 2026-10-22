// ttbh_sim.h — a simulated Blackhole BAR0/BAR2 for hardware-free tests (libttbh's own tests and
// the broker's `--sim` backend). Not used in any real device path.
//
// What it models:
//   * NOC memory as persistent 2 MiB pages keyed by (x, y, page base). A TLB window shows the page
//     its registers point at, decoded with an INDEPENDENT packed-bitfield struct (not libttbh's
//     encoder), so a wrong encoding shows the wrong memory rather than passing.
//   * A fake ARC firmware: boot status "ready", a message queue that answers TEST (value + 1),
//     POWER_SETTING (status 0) and ASIC_STATE0 (status 0), anything else with status 1, and only
//     after the host has triggered it (written 0 to ARC_MSI_FIFO). Plus a telemetry table
//     including the topology tags blackhole-py reads (34 = enabled Tensix columns, 36 = GDDR).
//   * BAR2 as plain memory (iATU registers just hold what's written).
//
// Memory model: every (tile, 2 MiB page) lives once, in a shared-memory backing store, and a window
// *maps* its page (mmap MAP_FIXED|MAP_SHARED). Two windows aimed at the same page are two views of
// the same bytes, as on hardware: a write through one is visible through the other at once, and
// nothing is ever copied back over it. (An earlier version copied each window's image into the
// store, so two windows on one page overwrote each other's writes; found in PR review.)

#ifndef TTBH_SIM_H
#define TTBH_SIM_H

#include "../ttbh.h"

typedef struct ttbh_sim ttbh_sim;

ttbh_sim *ttbh_sim_new(uint32_t arc_queue_entries);
void ttbh_sim_free(ttbh_sim *s);

volatile uint8_t *ttbh_sim_bar0(ttbh_sim *s);
uint64_t ttbh_sim_bar0_size(ttbh_sim *s);
volatile uint8_t *ttbh_sim_bar2(ttbh_sim *s);
uint64_t ttbh_sim_bar2_size(ttbh_sim *s);

// Call after TLB registers for `tlb_index` were written: runs a firmware step, decodes the registers
// and maps the new page into the window (or zero memory, if the settings can't reach memory).
void ttbh_sim_retarget(ttbh_sim *s, uint32_t tlb_index);
// Run a firmware step (it reads and writes the same memory the windows map).
void ttbh_sim_sync(ttbh_sim *s);

// A libttbh window over TLB `tlb_index` that retargets (and ticks the firmware) on every aim.
ttbh_window ttbh_sim_window(ttbh_sim *s, uint32_t tlb_index);

// Backing-store access for seeding/inspection. Windows showing that page see edits at once.
uint32_t *ttbh_sim_word(ttbh_sim *s, uint32_t x, uint32_t y, uint64_t addr);

// Decoded TLB register fields for `tlb_index`, via the independent decoder.
typedef struct { uint64_t addr; uint32_t x_start, y_start, x_end, y_end, noc, multicast, ordering; } ttbh_sim_tlb;
ttbh_sim_tlb ttbh_sim_decode_tlb(ttbh_sim *s, uint32_t tlb_index);

int ttbh_sim_messages_served(ttbh_sim *s);
int ttbh_sim_bad_messages(ttbh_sim *s);
uint32_t ttbh_sim_last_header(ttbh_sim *s);   // header of the most recent request served

#endif

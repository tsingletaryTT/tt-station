// broker.h — a Unix-socket broker that gives an UNENTITLED process (Python running blackhole-py)
// access to a Blackhole through tt-station's dext.
//
// Why a broker: a development-signed dext only opens for clients carrying the
// com.apple.developer.driverkit.userclient-access entitlement, and a stock `python3` can't carry
// it. So the entitled TTStationDriver.app runs this server (`TTStationDriver serve`) and the client
// talks to it over a socket, as tinygrad's TinyGPU does (installer/Shared/server.c). BAR mappings
// can't be handed across processes (task self-ports are immovable since macOS 12), so window MMIO
// is proxied as messages. Host memory IS shared: an shm fd, DMA-prepared by the server, goes to the
// client with SCM_RIGHTS, so bulk data (weights, command queues) never crosses the socket.
//
// The chip logic is libttbh's: TLB packing, telemetry walk, ARC messages and iATU, all verified in
// libttbh's tests and against tt-kmd on real silicon. This file only adds bookkeeping and framing.
//
// Backends (broker_backend): the real one maps BARs and DMA through the dext (broker_iokit.c,
// linked into the host app); the simulated one (broker_sim.c) uses in-memory BARs and a fake ARC,
// so the protocol + Python client are tested end to end without hardware.

#ifndef TTBH_BROKER_H
#define TTBH_BROKER_H

#include <stdint.h>
#include "../libttbh/ttbh.h"

#ifdef __cplusplus
extern "C" {
#endif

#define TTBH_BROKER_VERSION   1u
#define TTBH_BROKER_USER_TLBS 201u          // windows 0..200 for clients; 201 is the server's own
#define TTBH_BROKER_MAX_IO    (1u << 21)    // one window per MMIO request, at most
#define TTBH_BROKER_MAX_SYSMEM 8u

enum ttbh_broker_cmd {
    TTBH_CMD_HELLO = 0,        // → r0 = broker version, r1 = (device_id << 16) | subsystem_id
    TTBH_CMD_TLB_ALLOC = 1,    // → r0 = window id
    TTBH_CMD_TLB_FREE = 2,     // id
    TTBH_CMD_TLB_TARGET = 3,   // id; a0 = addr, a1 = start (x | y << 8), a2 = end (x | y << 8)
    TTBH_CMD_TLB_READ = 4,     // id; a0 = offset, a1 = length → payload
    TTBH_CMD_TLB_WRITE = 5,    // id; a0 = offset; payload = bytes
    TTBH_CMD_TELEMETRY = 6,    // a0 = tag → r0 = raw value
    TTBH_CMD_ARC_MSG = 7,      // payload = 8 u32 (header + 7) → payload = 8 u32 response
    TTBH_CMD_SYSMEM = 8,       // a0 = size → fd (SCM_RIGHTS), r0 = noc address, r1 = handle
    TTBH_CMD_SYSMEM_FREE = 9,  // a0 = handle
};

// Wire format (little-endian, packed). Every request gets exactly one response.
typedef struct __attribute__((packed)) ttbh_broker_req {
    uint32_t cmd, id;
    uint64_t a0, a1, a2;
    uint32_t payload_len, reserved;
} ttbh_broker_req;

typedef struct __attribute__((packed)) ttbh_broker_resp {
    int32_t status;            // TTBH_OK or a negative ttbh_err
    uint32_t payload_len;
    uint64_t r0, r1;
} ttbh_broker_resp;

// What the broker needs from its host. The DMA hooks mirror the dext's PrepareDMA/CompleteDMA.
typedef struct broker_backend {
    void *ctx;
    volatile uint8_t *bar0; uint64_t bar0_size;
    volatile uint8_t *bar2; uint64_t bar2_size;
    uint16_t device_id, subsystem_id;
    // DMA-map [addr, addr + len): fill up to *count segments, return a handle for complete().
    int (*prepare_dma)(void *ctx, void *addr, uint64_t len, ttbh_dma_segment *segs, uint32_t *count, uint32_t *handle);
    void (*complete_dma)(void *ctx, uint32_t handle);
    // Optional simulator hooks (NULL on real hardware, where the TLB itself does this work):
    // after_tlb_target runs after a window's registers change; sync runs around window I/O;
    // own_window supplies the server's window 201 (else a plain BAR0 window).
    void (*after_tlb_target)(void *ctx, uint32_t id);
    void (*sync)(void *ctx);
    ttbh_window (*own_window)(void *ctx);
} broker_backend;

// Serve clients on `socket_path`, one at a time, until a signal. Returns non-zero on setup failure.
// Sends ASIC_STATE0 at start, as tt-kmd does at device init.
int broker_serve(const broker_backend *be, const char *socket_path);

// One client, already accepted. Exposed for tests. Frees everything the client left behind.
void broker_session(const broker_backend *be, int client_fd);

#ifdef __cplusplus
}
#endif
#endif

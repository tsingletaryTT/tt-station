// broker.c — see broker.h. Portable POSIX (macOS + Linux) so the simulated build can be tested on
// either; only broker_iokit.c is macOS-specific.

#ifndef _DEFAULT_SOURCE
#define _DEFAULT_SOURCE
#endif
#define _DARWIN_C_SOURCE
#include "broker.h"

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

// ── per-session state ─────────────────────────────────────────────────────────────────────

typedef struct {
    bool used;
    void *addr; uint64_t size;
    int fd;
    uint32_t dma_handle;
    uint32_t first_region, nregions;
    uint64_t noc_base;             // chip-visible offset above TTBH_NOC_PCIE_OFFSET
} sysmem_slot;

typedef struct {
    const broker_backend *be;
    int fd;
    bool tlb_owned[TTBH_BROKER_USER_TLBS];   // windows this session allocated
    sysmem_slot sysmem[TTBH_BROKER_MAX_SYSMEM];
} session;

// Several sessions at once, like several opens of /dev/tenstorrent/N under tt-kmd. tt-flash needs
// this: it opens the card again in the same process and from forkserver workers while the parent
// still holds it (tt_flash/chip.py, flash.py). The broker is single-threaded and handles each
// request whole, so sessions interleave at request boundaries. What they share is the card's
// hardware: TLB windows and outbound iATU regions are owned broker-wide, so two sessions can never
// be handed the same window or region. What they do with the chip is theirs to coordinate, as on
// Linux.
#define TTBH_BROKER_MAX_SESSIONS 16
static session *g_sessions[TTBH_BROKER_MAX_SESSIONS];
static bool g_tlb_used[TTBH_BROKER_USER_TLBS];
static bool g_region_used[TTBH_IATU_REGIONS];
static uint8_t g_io[TTBH_BROKER_MAX_IO];       // one request at a time, so one buffer serves all

// Lowest chip-visible offset where `size` bytes fit between the session's live buffers (first fit),
// never straddling a 1 TiB boundary (an iATU region can't; see ttbh.h). Freed ranges come back:
// this used to be a bump allocator, and a long-lived session walked up the NOC address space for
// ever (seen 2026-10-22 on the card: 64 MiB, then 256 MiB, … each landing above the last though the
// previous one had been freed). At most TTBH_BROKER_MAX_SYSMEM buffers, so the O(n^2) scan is fine.
// The chip-visible range is one address space for the whole card, so the scan covers every
// session's buffers.
static const sysmem_slot *live_buffer(int i)          // i over all sessions x slots
{
    const session *s = g_sessions[i / (int)TTBH_BROKER_MAX_SYSMEM];
    const sysmem_slot *m = s ? &s->sysmem[i % (int)TTBH_BROKER_MAX_SYSMEM] : NULL;
    return m && m->used ? m : NULL;
}
static uint64_t noc_first_fit(uint64_t size)
{
    const int n = TTBH_BROKER_MAX_SESSIONS * (int)TTBH_BROKER_MAX_SYSMEM;
    uint64_t best = UINT64_MAX;
    // Candidate starts: 0 and the end of every live buffer. The answer is always one of these
    // (rounded up past a 1 TiB boundary when it would cross one).
    for (int c = -1; c < n; c++) {
        const sysmem_slot *cm = c < 0 ? NULL : live_buffer(c);
        if (c >= 0 && !cm) continue;
        uint64_t base = c < 0 ? 0 : cm->noc_base + cm->size;
        if ((base >> 40) != ((base + size - 1) >> 40)) base = (base + (1ull << 40)) & ~((1ull << 40) - 1);
        bool clear = true;
        for (int o = 0; o < n && clear; o++) {
            const sysmem_slot *m = live_buffer(o);
            if (m && base < m->noc_base + m->size && m->noc_base < base + size) clear = false;
        }
        if (clear && base < best) best = base;
    }
    return best;
}

// The server's own window (201) for telemetry and ARC messages, never handed to a client.
static ttbh_window own_window(const broker_backend *be, ttbh_bar0_ctx *ctx)
{
    if (be->own_window) return be->own_window(be->ctx);
    *ctx = (ttbh_bar0_ctx){ be->bar0, be->bar0_size, TTBH_DRIVER_TLB_INDEX };
    return ttbh_bar0_window(ctx);
}

// ── socket helpers ────────────────────────────────────────────────────────────────────────

static volatile sig_atomic_t stop_requested = 0;
static void on_signal(int sig) { (void)sig; stop_requested = 1; }

static int read_all(int fd, void *buf, size_t n)
{
    uint8_t *p = buf;
    while (n) {
        ssize_t r = read(fd, p, n);
        if (r <= 0) { if (r < 0 && errno == EINTR && !stop_requested) continue; return -1; }
        p += r; n -= (size_t)r;
    }
    return 0;
}

static int write_all(int fd, const void *buf, size_t n)
{
    const uint8_t *p = buf;
    while (n) {
        ssize_t w = write(fd, p, n);
        if (w <= 0) { if (w < 0 && errno == EINTR) continue; return -1; }
        p += w; n -= (size_t)w;
    }
    return 0;
}

static int respond(int fd, int32_t status, uint64_t r0, uint64_t r1, const void *payload, uint32_t len, int pass_fd)
{
    ttbh_broker_resp resp = { status, len, r0, r1 };
    if (pass_fd < 0) {
        if (write_all(fd, &resp, sizeof resp)) return -1;
    } else {
        // The response header travels with the fd, so the client gets both atomically.
        struct iovec iov = { &resp, sizeof resp };
        char cbuf[CMSG_SPACE(sizeof(int))];
        memset(cbuf, 0, sizeof cbuf);
        struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &pass_fd, sizeof(int));
        if (sendmsg(fd, &msg, 0) != (ssize_t)sizeof resp) return -1;
    }
    return len ? write_all(fd, payload, len) : 0;
}

// ── commands ──────────────────────────────────────────────────────────────────────────────

static bool window_ok(session *s, uint32_t id) { return id < TTBH_BROKER_USER_TLBS && s->tlb_owned[id]; }
// Is [off, off + len) inside one 2 MiB window? Written so it can't wrap: a client-supplied offset
// near UINT64_MAX would make `off + len` overflow and pass a naive `off + len > size` check.
static bool span_ok(uint64_t off, uint64_t len) { return off <= TTBH_TLB_2M_SIZE && len <= TTBH_TLB_2M_SIZE - off; }
static uint64_t window_off(uint32_t id) { return (uint64_t)id * TTBH_TLB_2M_SIZE; }

// BAR access: mapped pointer (dext, simulators) or function calls (ttsim), whichever the backend has.
static void bar_rd(const broker_backend *be, int bar, uint64_t off, void *dst, uint32_t n);
static void bar_wr(const broker_backend *be, int bar, uint64_t off, const void *src, uint32_t n);
static void bar0_wr32(void *vbe, uint32_t off, uint32_t v) { bar_wr(vbe, 0, off, &v, 4); }
static void bar2_wr32(void *vbe, uint32_t off, uint32_t v) { bar_wr(vbe, 2, off, &v, 4); }

// Program window `id` exactly as blackhole-py's ConfigureTlb does: end/start rectangle, multicast
// iff start != end, NOC 0, strict ordering (pcie.py ConfigureTlbPayload).
static int tlb_target(session *s, uint32_t id, uint64_t addr, uint64_t start, uint64_t end)
{
    if (!window_ok(s, id)) return TTBH_EINVAL;
    // Unicast (start == end) leaves the start coordinates at 0, as tt-kmd's own kernel window does.
    // blackhole-py's ConfigureTlb puts start = end = core there; silicon ignores start for unicast,
    // but ttsim rejects it ("x_start/y_start set without mcast"), and tt-kmd's convention is valid on both.
    const bool mcast = start != end;
    ttbh_tlb_config c = {
        .addr = addr,
        .x_start = mcast ? (uint8_t)(start & 0xFF) : 0, .y_start = mcast ? (uint8_t)((start >> 8) & 0xFF) : 0,
        .x_end = (uint8_t)(end & 0xFF), .y_end = (uint8_t)((end >> 8) & 0xFF),
        .multicast = mcast, .ordering = 1,
    };
    int err = ttbh_tlb2m_program_with(bar0_wr32, (void *)s->be, id, &c);
    if (err) return err;
    uint32_t flush;
    bar_rd(s->be, 0, TTBH_TLB_REGS_START + id * TTBH_TLB_REG_SIZE + 8, &flush, 4);   // flush posted writes
    if (s->be->after_tlb_target) s->be->after_tlb_target(s->be->ctx, id);
    return TTBH_OK;
}

// Copy between a window and the IO buffer with 32-bit accesses where possible: device memory
// must not see byte-wise or wider-than-requested accesses from a libc memcpy.
static void mmio_copy_from(uint8_t *dst, volatile uint8_t *src, uint64_t n)
{
    uint64_t i = 0;
    for (; i + 4 <= n; i += 4) { uint32_t v = *(volatile uint32_t *)(src + i); memcpy(dst + i, &v, 4); }
    for (; i < n; i++) dst[i] = src[i];
}
static void mmio_copy_to(volatile uint8_t *dst, const uint8_t *src, uint64_t n)
{
    uint64_t i = 0;
    for (; i + 4 <= n; i += 4) { uint32_t v; memcpy(&v, src + i, 4); *(volatile uint32_t *)(dst + i) = v; }
    for (; i < n; i++) dst[i] = src[i];
}

static void bar_rd(const broker_backend *be, int bar, uint64_t off, void *dst, uint32_t n)
{
    if (be->mmio_rd) { be->mmio_rd(be->ctx, bar, off, dst, n); return; }
    mmio_copy_from(dst, (bar == 0 ? be->bar0 : be->bar2) + off, n);
}
static void bar_wr(const broker_backend *be, int bar, uint64_t off, const void *src, uint32_t n)
{
    if (be->mmio_wr) { be->mmio_wr(be->ctx, bar, off, src, n); return; }
    mmio_copy_to((bar == 0 ? be->bar0 : be->bar2) + off, src, n);
}

// Allocate shared host memory, DMA-map it, and point iATU regions at it.
static int sysmem_create(session *s, uint64_t size, int *out_fd, uint64_t *noc, uint32_t *handle)
{
    int slot = -1;
    for (int i = 0; i < (int)TTBH_BROKER_MAX_SYSMEM; i++) if (!s->sysmem[i].used) { slot = i; break; }
    if (slot < 0 || size == 0) return TTBH_EINVAL;
    size = (size + 0x3FFFu) & ~(uint64_t)0x3FFF;          // 16 KiB: the macOS DART page

    char name[64];
    snprintf(name, sizeof name, "/ttbh.%d.%d.%d", (int)getpid(), s->fd, slot);
    shm_unlink(name);
    int fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) return TTBH_EINVAL;
    shm_unlink(name);                                     // the fds keep it alive; no name leaks
    if (ftruncate(fd, (off_t)size) != 0) { close(fd); return TTBH_EINVAL; }
    void *addr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (addr == MAP_FAILED) { close(fd); return TTBH_EINVAL; }

    ttbh_dma_segment segs[TTBH_IATU_REGIONS];
    uint32_t nseg = TTBH_IATU_REGIONS, dma = 0;
    if (s->be->prepare_dma(s->be->ctx, addr, size, segs, &nseg, &dma) != 0) goto fail;

    // Find nseg consecutive free iATU regions.
    uint32_t first = TTBH_IATU_REGIONS;
    for (uint32_t r = 0; r + nseg <= TTBH_IATU_REGIONS && first == TTBH_IATU_REGIONS; r++) {
        uint32_t k = 0;
        while (k < nseg && !g_region_used[r + k]) k++;
        if (k == nseg) first = r;
    }
    ttbh_iatu_plan_entry plan[TTBH_IATU_REGIONS];
    uint64_t base = noc_first_fit(size);
    if (first == TTBH_IATU_REGIONS || ttbh_dma_plan(segs, nseg, base, first, plan) != TTBH_OK) {
        s->be->complete_dma(s->be->ctx, dma);
        goto fail;
    }
    for (uint32_t i = 0; i < nseg; i++) {
        ttbh_iatu_regs r;
        ttbh_iatu_outbound_encode(plan[i].base, plan[i].limit, plan[i].target, &r);
        ttbh_iatu_outbound_write_with(bar2_wr32, (void *)s->be, plan[i].region, &r);
        g_region_used[plan[i].region] = true;
    }
    s->sysmem[slot] = (sysmem_slot){ true, addr, size, fd, dma, first, nseg, base };
    *out_fd = fd;
    *noc = TTBH_NOC_PCIE_OFFSET + base;
    *handle = (uint32_t)slot;
    return TTBH_OK;
fail:
    munmap(addr, size);
    close(fd);
    return TTBH_EINVAL;
}

static void sysmem_free(session *s, uint32_t h)
{
    if (h >= TTBH_BROKER_MAX_SYSMEM || !s->sysmem[h].used) return;
    sysmem_slot *m = &s->sysmem[h];
    for (uint32_t i = 0; i < m->nregions; i++) {        // disable first, then unmap the pages
        ttbh_iatu_regs off;
        ttbh_iatu_outbound_encode(0, 0, 0, &off);
        ttbh_iatu_outbound_write_with(bar2_wr32, (void *)s->be, m->first_region + i, &off);
        g_region_used[m->first_region + i] = false;
    }
    s->be->complete_dma(s->be->ctx, m->dma_handle);
    munmap(m->addr, m->size);
    close(m->fd);
    m->used = false;
}

// ── Read-only telemetry for any number of readers ────────────────────────────────────────
// Sessions drive the card (blackhole-py, tt-flash). Monitors such as tt-toplike only want to see it,
// and shouldn't need a session or its windows to do that, so the broker also listens on a second socket
// (telemetry.sock next to the session socket, or TTBH_TELEMETRY_SOCKET). A reader connects, gets
// ONE JSON line, and the broker closes the connection:
//
//   {"v":1,"device_id":45376,"subsystem_id":67,"arc_ready":true,"asic_temp_c":56.4,"power_w":38,
//    "vcore_mv":740,"current_a":51,"aiclk_mhz":800,"heartbeat":48141,"fan_rpm":null}
//
// A field is null when its read failed. Readers are served between the session's requests (the
// broker is single-threaded), through the broker's own TLB window, never a client's, so a reading
// can't disturb what the session is doing. Nothing here writes to the card.
static int tel_srv = -1;

static void json_u32(FILE *f, const char *k, int err, uint32_t v) { if (err) fprintf(f, ",\"%s\":null", k); else fprintf(f, ",\"%s\":%u", k, v); }

static void serve_telemetry(const broker_backend *be)
{
    int c = accept(tel_srv, NULL, NULL);
    if (c < 0) return;
    ttbh_bar0_ctx ctx;
    ttbh_window w = own_window(be, &ctx);
    uint32_t boot = 0, v = 0;
    int boot_err = ttbh_arc_boot_status(&w, &boot);
    char buf[512];
    FILE *f = fmemopen(buf, sizeof buf, "w");
    if (!f) { close(c); return; }
    fprintf(f, "{\"v\":1,\"device_id\":%u,\"subsystem_id\":%u,\"arc_ready\":%s",
            be->device_id, be->subsystem_id, (!boot_err && ttbh_arc_ready(boot)) ? "true" : "false");
    int e = boot_err ? boot_err : ttbh_telemetry_read(&w, TTBH_TAG_ASIC_TEMP, &v);
    if (e) fprintf(f, ",\"asic_temp_c\":null"); else fprintf(f, ",\"asic_temp_c\":%.2f", ttbh_temp_c(v));
    static const struct { const char *k; uint16_t tag; } tags[] = {
        { "power_w", TTBH_TAG_POWER }, { "vcore_mv", TTBH_TAG_VCORE }, { "current_a", TTBH_TAG_CURRENT },
        { "aiclk_mhz", TTBH_TAG_AICLK }, { "heartbeat", TTBH_TAG_TIMER_HEARTBEAT }, { "fan_rpm", TTBH_TAG_FAN_RPM },
    };
    for (size_t i = 0; i < sizeof tags / sizeof tags[0]; i++) {
        int err = boot_err ? boot_err : ttbh_telemetry_read(&w, tags[i].tag, &v);
        if (!err && tags[i].tag == TTBH_TAG_FAN_RPM && v == 0xFFFFFFFFu) err = 1;   // fan control disabled
        json_u32(f, tags[i].k, err, v);
    }
    fprintf(f, "}\n");
    long n = ftell(f);
    fclose(f);
    if (n > 0) { ssize_t wr = write(c, buf, (size_t)n); (void)wr; }
    close(c);
}


// Bind a fresh AF_UNIX listener at `path`, replacing only a stale socket (never another file).
static int listen_unix(const char *path, int backlog)
{
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    if (strlen(path) >= sizeof addr.sun_path) { fprintf(stderr, "socket path too long: %s\n", path); return -1; }
    strcpy(addr.sun_path, path);
    struct stat st;
    if (lstat(path, &st) == 0) {
        if (!S_ISSOCK(st.st_mode)) {
            fprintf(stderr, "ttbh broker: %s exists and is not a socket; refusing to replace it\n", path);
            return -1;
        }
        unlink(path);
    }
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    if (srv < 0) { perror("socket"); return -1; }
    mode_t old = umask(0077);                             // only this user may connect
    int err = bind(srv, (struct sockaddr *)&addr, sizeof addr);
    umask(old);
    if (err || listen(srv, backlog)) { perror("bind/listen"); close(srv); return -1; }
    return srv;
}

// One request from session `s`. Returns 0 to keep the session, nonzero to drop it.
static int handle_request(session *s)
{
    const broker_backend *be = s->be;
    const int fd = s->fd;
    ttbh_bar0_ctx own_ctx;
    ttbh_window own = own_window(be, &own_ctx);
    ttbh_broker_req q;
    if (be->tick) be->tick(be->ctx);                   // simulated time: let the chip catch up
    if (read_all(fd, &q, sizeof q) != 0) return -1;
    if (q.payload_len > TTBH_BROKER_MAX_IO) return -1;           // protocol violation: drop client
    if (q.payload_len && read_all(fd, g_io, q.payload_len)) return -1;
    int rc = 0;
    switch (q.cmd) {
    case TTBH_CMD_HELLO:
        rc = respond(fd, TTBH_OK, TTBH_BROKER_VERSION, ((uint64_t)be->device_id << 16) | be->subsystem_id, 0, 0, -1);
        break;
    case TTBH_CMD_TLB_ALLOC: {
        uint32_t id = 0;
        while (id < TTBH_BROKER_USER_TLBS && g_tlb_used[id]) id++;
        if (id == TTBH_BROKER_USER_TLBS) { rc = respond(fd, TTBH_EAGAIN, 0, 0, 0, 0, -1); break; }
        g_tlb_used[id] = s->tlb_owned[id] = true;
        rc = respond(fd, TTBH_OK, id, 0, 0, 0, -1);
        break;
    }
    case TTBH_CMD_TLB_FREE:
        if (window_ok(s, q.id)) g_tlb_used[q.id] = s->tlb_owned[q.id] = false;   // only your own
        rc = respond(fd, TTBH_OK, 0, 0, 0, 0, -1);
        break;
    case TTBH_CMD_TLB_TARGET:
        rc = respond(fd, tlb_target(s, q.id, q.a0, q.a1, q.a2), 0, 0, 0, 0, -1);
        break;
    case TTBH_CMD_TLB_READ: {
        if (!window_ok(s, q.id) || q.a1 > TTBH_BROKER_MAX_IO || !span_ok(q.a0, q.a1)) { rc = respond(fd, TTBH_EINVAL, 0, 0, 0, 0, -1); break; }
        if (be->sync) be->sync(be->ctx);
        bar_rd(be, 0, window_off(q.id) + q.a0, g_io, (uint32_t)q.a1);
        rc = respond(fd, TTBH_OK, 0, 0, g_io, (uint32_t)q.a1, -1);
        break;
    }
    case TTBH_CMD_TLB_WRITE: {
        if (!window_ok(s, q.id) || !span_ok(q.a0, q.payload_len)) { rc = respond(fd, TTBH_EINVAL, 0, 0, 0, 0, -1); break; }
        bar_wr(be, 0, window_off(q.id) + q.a0, g_io, q.payload_len);
        if (be->sync) be->sync(be->ctx);
        rc = respond(fd, TTBH_OK, 0, 0, 0, 0, -1);
        break;
    }
    case TTBH_CMD_TELEMETRY: {
        uint32_t v = 0;
        int err = ttbh_telemetry_read(&own, (uint16_t)q.a0, &v);
        rc = respond(fd, err, v, 0, 0, 0, -1);
        break;
    }
    case TTBH_CMD_ARC_MSG: {
        if (q.payload_len != sizeof(ttbh_arc_msg)) { rc = respond(fd, TTBH_EINVAL, 0, 0, 0, 0, -1); break; }
        ttbh_arc_msg m;
        memcpy(&m, g_io, sizeof m);
        int err = ttbh_arc_msg_send(&own, &m);
        rc = respond(fd, err, 0, 0, &m, err == TTBH_OK || err == TTBH_EREMOTE ? sizeof m : 0, -1);
        break;
    }
    case TTBH_CMD_SYSMEM: {
        int mfd = -1; uint64_t noc = 0; uint32_t h = 0;
        int err = sysmem_create(s, q.a0, &mfd, &noc, &h);
        rc = respond(fd, err, noc, h, 0, 0, err ? -1 : mfd);
        break;
    }
    case TTBH_CMD_SYSMEM_FREE:
        sysmem_free(s, (uint32_t)q.a0);
        rc = respond(fd, TTBH_OK, 0, 0, 0, 0, -1);
        break;
    default:
        rc = respond(fd, TTBH_EINVAL, 0, 0, 0, 0, -1);
    }
    return rc;
}

static int session_open(const broker_backend *be, int fd)
{
    for (int i = 0; i < TTBH_BROKER_MAX_SESSIONS; i++) {
        if (g_sessions[i]) continue;
        session *s = calloc(1, sizeof *s);
        if (!s) break;
        s->be = be;
        s->fd = fd;
        g_sessions[i] = s;
        return 0;
    }
    fprintf(stderr, "ttbh broker: %d sessions open; refusing another\n", TTBH_BROKER_MAX_SESSIONS);
    close(fd);
    return -1;
}

// Client gone: release everything it held (iATU regions + DMA mappings + pages, then windows).
static void session_close(int i)
{
    session *s = g_sessions[i];
    for (uint32_t h = 0; h < TTBH_BROKER_MAX_SYSMEM; h++) sysmem_free(s, h);
    for (uint32_t id = 0; id < TTBH_BROKER_USER_TLBS; id++) if (s->tlb_owned[id]) g_tlb_used[id] = false;
    close(s->fd);
    free(s);
    g_sessions[i] = NULL;
}


int broker_serve(const broker_backend *be, const char *path)
{
    // Replace only a stale SOCKET at the path. The path is caller-chosen (`serve [SOCKET]`), so
    // anything else there (a regular file, a directory, a symlink) is left alone and we refuse.
    int srv = listen_unix(path, 16);
    if (srv < 0) return 1;

    // The read-only telemetry socket: TTBH_TELEMETRY_SOCKET, else telemetry.sock beside `path`.
    char tel_path[sizeof(((struct sockaddr_un *)0)->sun_path)];
    const char *env = getenv("TTBH_TELEMETRY_SOCKET");
    const char *slash = strrchr(path, '/');
    if (env) snprintf(tel_path, sizeof tel_path, "%s", env);
    else snprintf(tel_path, sizeof tel_path, "%.*stelemetry.sock", slash ? (int)(slash - path + 1) : 0, path);
    tel_srv = listen_unix(tel_path, 16);
    if (tel_srv < 0) fprintf(stderr, "ttbh broker: no telemetry socket (%s); readers won't see the card\n", tel_path);

    // sigaction WITHOUT SA_RESTART: on macOS signal() restarts accept()/read() after the handler,
    // so SIGTERM set the flag but the server never noticed (the first test run hung in tearDown).
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);
    signal(SIGPIPE, SIG_IGN);

    // tt-kmd sends ASIC_STATE0 when it initialises a device; blackhole-py assumes that happened.
    ttbh_bar0_ctx ctx;
    ttbh_window own = own_window(be, &ctx);
    ttbh_arc_msg a0 = { .header = TTBH_ARC_MSG_ASIC_STATE0 };
    int a0_err = ttbh_arc_msg_send(&own, &a0);
    fprintf(stderr, "ttbh broker: listening on %s (ASIC_STATE0: %s), telemetry on %s\n",
            path, ttbh_strerror(a0_err), tel_srv >= 0 ? tel_path : "(none)");

    // One loop for everything: new sessions, telemetry readers, and a request from any open session.
    // With a tick (simulated backends) and a session open, time keeps moving while clients think;
    // with nobody connected the loop blocks, so an idle simulator doesn't spin.
    while (!stop_requested) {
        struct pollfd p[2 + TTBH_BROKER_MAX_SESSIONS];
        int idx[TTBH_BROKER_MAX_SESSIONS], n = 0, open_sessions = 0;
        p[n++] = (struct pollfd){ .fd = srv, .events = POLLIN };
        p[n++] = (struct pollfd){ .fd = tel_srv, .events = POLLIN };   // fd -1 is ignored by poll
        for (int i = 0; i < TTBH_BROKER_MAX_SESSIONS; i++) {
            if (!g_sessions[i]) continue;
            idx[n - 2] = i;
            p[n++] = (struct pollfd){ .fd = g_sessions[i]->fd, .events = POLLIN };
            open_sessions++;
        }
        const bool ticking = be->tick && open_sessions > 0;
        int r = poll(p, (nfds_t)n, ticking ? 0 : -1);
        if (r < 0) { if (errno == EINTR) continue; perror("poll"); break; }   // loop re-checks the flag
        if (ticking && r == 0) { be->tick(be->ctx); continue; }
        if (p[1].revents & POLLIN) serve_telemetry(be);
        for (int k = 2; k < n; k++)
            if ((p[k].revents & (POLLIN | POLLHUP | POLLERR)) && handle_request(g_sessions[idx[k - 2]]) != 0)
                session_close(idx[k - 2]);
        if (p[0].revents & POLLIN) {
            int c = accept(srv, NULL, NULL);
            if (c >= 0) session_open(be, c);
            else if (errno != EINTR) { perror("accept"); break; }
        }
    }
    for (int i = 0; i < TTBH_BROKER_MAX_SESSIONS; i++) if (g_sessions[i]) session_close(i);
    close(srv);
    unlink(path);
    if (tel_srv >= 0) { close(tel_srv); unlink(tel_path); tel_srv = -1; }
    return 0;
}

// broker.c — see broker.h. Portable POSIX (macOS + Linux) so the simulated build can be tested on
// either; only broker_iokit.c is macOS-specific.

#ifndef _DEFAULT_SOURCE
#define _DEFAULT_SOURCE
#endif
#define _DARWIN_C_SOURCE
#include "broker.h"

#include <errno.h>
#include <fcntl.h>
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
} sysmem_slot;

typedef struct {
    const broker_backend *be;
    bool tlb_used[TTBH_BROKER_USER_TLBS];
    sysmem_slot sysmem[TTBH_BROKER_MAX_SYSMEM];
    bool region_used[TTBH_IATU_REGIONS];
    uint64_t next_noc_base;        // bump allocator for the chip-visible sysmem range
    uint8_t io[TTBH_BROKER_MAX_IO];
} session;

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

static volatile uint8_t *user_window(session *s, uint32_t id)
{
    if (id >= TTBH_BROKER_USER_TLBS || !s->tlb_used[id]) return 0;
    return s->be->bar0 + (uint64_t)id * TTBH_TLB_2M_SIZE;
}

// Program window `id` exactly as blackhole-py's ConfigureTlb does: end/start rectangle, multicast
// iff start != end, NOC 0, strict ordering (pcie.py ConfigureTlbPayload).
static int tlb_target(session *s, uint32_t id, uint64_t addr, uint64_t start, uint64_t end)
{
    if (!user_window(s, id)) return TTBH_EINVAL;
    ttbh_tlb_config c = {
        .addr = addr,
        .x_start = (uint8_t)(start & 0xFF), .y_start = (uint8_t)((start >> 8) & 0xFF),
        .x_end = (uint8_t)(end & 0xFF), .y_end = (uint8_t)((end >> 8) & 0xFF),
        .multicast = start != end, .ordering = 1,
    };
    uint32_t w[3];
    int err = ttbh_tlb2m_encode(&c, w);
    if (err) return err;
    volatile uint32_t *regs = (volatile uint32_t *)(s->be->bar0 + TTBH_TLB_REGS_START + id * TTBH_TLB_REG_SIZE);
    regs[0] = w[0]; regs[1] = w[1]; regs[2] = w[2];
    if (id < TTBH_TLB_STRIDED_COUNT)
        *(volatile uint32_t *)(s->be->bar0 + TTBH_TLB_REGS_START + TTBH_TLB_STRIDED_REGS_OFF + id * 4u) = 0;
    (void)regs[2];   // flush the posted writes before the window is used
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

// Allocate shared host memory, DMA-map it, and point iATU regions at it.
static int sysmem_create(session *s, uint64_t size, int *out_fd, uint64_t *noc, uint32_t *handle)
{
    int slot = -1;
    for (int i = 0; i < (int)TTBH_BROKER_MAX_SYSMEM; i++) if (!s->sysmem[i].used) { slot = i; break; }
    if (slot < 0 || size == 0) return TTBH_EINVAL;
    size = (size + 0x3FFFu) & ~(uint64_t)0x3FFF;          // 16 KiB: the macOS DART page

    char name[64];
    snprintf(name, sizeof name, "/ttbh.%d.%d", (int)getpid(), slot);
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
        while (k < nseg && !s->region_used[r + k]) k++;
        if (k == nseg) first = r;
    }
    ttbh_iatu_plan_entry plan[TTBH_IATU_REGIONS];
    // Start each buffer on a fresh 1 TiB block if it would otherwise cross one (see ttbh.h).
    uint64_t base = s->next_noc_base;
    if ((base >> 40) != ((base + size - 1) >> 40)) base = (base + (1ull << 40)) & ~((1ull << 40) - 1);
    if (first == TTBH_IATU_REGIONS || ttbh_dma_plan(segs, nseg, base, first, plan) != TTBH_OK) {
        s->be->complete_dma(s->be->ctx, dma);
        goto fail;
    }
    for (uint32_t i = 0; i < nseg; i++) {
        ttbh_iatu_regs r;
        ttbh_iatu_outbound_encode(plan[i].base, plan[i].limit, plan[i].target, &r);
        ttbh_iatu_outbound_write(s->be->bar2, s->be->bar2_size, plan[i].region, &r);
        s->region_used[plan[i].region] = true;
    }
    s->next_noc_base = base + size;
    s->sysmem[slot] = (sysmem_slot){ true, addr, size, fd, dma, first, nseg };
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
        ttbh_iatu_outbound_write(s->be->bar2, s->be->bar2_size, m->first_region + i, &off);
        s->region_used[m->first_region + i] = false;
    }
    s->be->complete_dma(s->be->ctx, m->dma_handle);
    munmap(m->addr, m->size);
    close(m->fd);
    m->used = false;
}

void broker_session(const broker_backend *be, int fd)
{
    session *s = calloc(1, sizeof *s);
    if (!s) { close(fd); return; }
    s->be = be;
    ttbh_bar0_ctx own_ctx;
    ttbh_window own = own_window(be, &own_ctx);

    ttbh_broker_req q;
    while (read_all(fd, &q, sizeof q) == 0) {
        if (q.payload_len > TTBH_BROKER_MAX_IO) break;            // protocol violation: drop client
        if (q.payload_len && read_all(fd, s->io, q.payload_len)) break;
        int rc = 0;
        switch (q.cmd) {
        case TTBH_CMD_HELLO:
            rc = respond(fd, TTBH_OK, TTBH_BROKER_VERSION, ((uint64_t)be->device_id << 16) | be->subsystem_id, 0, 0, -1);
            break;
        case TTBH_CMD_TLB_ALLOC: {
            uint32_t id = 0;
            while (id < TTBH_BROKER_USER_TLBS && s->tlb_used[id]) id++;
            if (id == TTBH_BROKER_USER_TLBS) { rc = respond(fd, TTBH_EAGAIN, 0, 0, 0, 0, -1); break; }
            s->tlb_used[id] = true;
            rc = respond(fd, TTBH_OK, id, 0, 0, 0, -1);
            break;
        }
        case TTBH_CMD_TLB_FREE:
            if (q.id < TTBH_BROKER_USER_TLBS) s->tlb_used[q.id] = false;
            rc = respond(fd, TTBH_OK, 0, 0, 0, 0, -1);
            break;
        case TTBH_CMD_TLB_TARGET:
            rc = respond(fd, tlb_target(s, q.id, q.a0, q.a1, q.a2), 0, 0, 0, 0, -1);
            break;
        case TTBH_CMD_TLB_READ: {
            volatile uint8_t *win = user_window(s, q.id);
            if (!win || q.a1 > TTBH_BROKER_MAX_IO || q.a0 + q.a1 > TTBH_TLB_2M_SIZE) { rc = respond(fd, TTBH_EINVAL, 0, 0, 0, 0, -1); break; }
            if (be->sync) be->sync(be->ctx);
            mmio_copy_from(s->io, win + q.a0, q.a1);
            rc = respond(fd, TTBH_OK, 0, 0, s->io, (uint32_t)q.a1, -1);
            break;
        }
        case TTBH_CMD_TLB_WRITE: {
            volatile uint8_t *win = user_window(s, q.id);
            if (!win || q.a0 + q.payload_len > TTBH_TLB_2M_SIZE) { rc = respond(fd, TTBH_EINVAL, 0, 0, 0, 0, -1); break; }
            mmio_copy_to(win + q.a0, s->io, q.payload_len);
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
            memcpy(&m, s->io, sizeof m);
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
        if (rc) break;
    }
    // Client gone: release everything it held (iATU regions + DMA mappings + pages).
    for (uint32_t h = 0; h < TTBH_BROKER_MAX_SYSMEM; h++) sysmem_free(s, h);
    close(fd);
    free(s);
}


int broker_serve(const broker_backend *be, const char *path)
{
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    if (srv < 0) { perror("socket"); return 1; }
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    if (strlen(path) >= sizeof addr.sun_path) { fprintf(stderr, "socket path too long\n"); return 1; }
    strcpy(addr.sun_path, path);
    unlink(path);
    mode_t old = umask(0077);                             // only this user may connect
    int err = bind(srv, (struct sockaddr *)&addr, sizeof addr);
    umask(old);
    if (err || listen(srv, 1)) { perror("bind/listen"); close(srv); return 1; }

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
    fprintf(stderr, "ttbh broker: listening on %s (ASIC_STATE0: %s)\n", path, ttbh_strerror(a0_err));

    while (!stop_requested) {
        int c = accept(srv, NULL, NULL);
        if (c < 0) { if (errno == EINTR) continue; perror("accept"); break; }   // loop re-checks the flag
        broker_session(be, c);
    }
    close(srv);
    unlink(path);
    return 0;
}

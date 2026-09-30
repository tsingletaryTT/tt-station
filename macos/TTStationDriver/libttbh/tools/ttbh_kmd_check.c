// ttbh_kmd_check.c — run libttbh's ARC + telemetry logic against a REAL Blackhole on Linux, and
// compare every value with what tt-kmd itself reports through hwmon/sysfs.
//
// Why this exists: on macOS the dext will hand us raw BAR0, and ttbh_bar0_window will program TLB
// registers itself. That packing is verified in tests/test_ttbh.c. Everything ABOVE the packing
// (ARC's NOC coordinate, the scratch-register addresses, the telemetry tag-table walk, CSM bounds,
// value units) is what this tool checks on silicon, with tt-kmd doing only the window aiming
// (TENSTORRENT_IOCTL_ALLOCATE_TLB / CONFIGURE_TLB). If ours and tt-kmd's hwmon agree, the macOS
// path only has to get the register packing right, and that part is already tested.
//
// Read-only on the chip apart from TLB window configuration. Opens /dev/tenstorrent/N, so run it
// under a gozer lease:
//
//   gozer run --chips 1 --who "claude:ttbh-kmd-check" --reason "validate libttbh" -- ./build/ttbh-kmd-check
//
// It uses the first device in TT_VISIBLE_DEVICES (set by gozer), or --device N. `--dma` adds the M3
// check (iATU encoding vs tt-kmd's registers + a chip↔host loopback); see dma_check().

#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <glob.h>
#include <time.h>

#include <linux/types.h>
#include "ioctl.h"   // tt-kmd's UAPI header (GPL-2.0 WITH Linux-syscall-note), from the dkms tree

#include "../ttbh.h"

typedef struct {
    int fd;
    uint32_t tlb_id;
    volatile uint8_t *map;
} kmd_ctx;

// Aim tt-kmd's user TLB at (x, y, base) the same way the macOS backend will: unicast, strict ordering.
static volatile uint8_t *kmd_aim(void *vctx, uint32_t x, uint32_t y, uint64_t base)
{
    kmd_ctx *k = (kmd_ctx *)vctx;
    struct tenstorrent_configure_tlb cfg;
    memset(&cfg, 0, sizeof cfg);
    cfg.in.id = k->tlb_id;
    cfg.in.config.addr = base;
    cfg.in.config.x_end = (__u16)x;
    cfg.in.config.y_end = (__u16)y;
    cfg.in.config.ordering = 1;
    if (ioctl(k->fd, TENSTORRENT_IOCTL_CONFIGURE_TLB, &cfg) != 0) {
        perror("CONFIGURE_TLB");
        return NULL;
    }
    return k->map;
}

// /dev/tenstorrent/N for the first chip gozer granted us. gozer's TT_VISIBLE_DEVICES holds PCI
// BDFs ("0000:03:00.0,0000:04:00.0"), NOT device indices. The first version of this tool did
// atoi() on it, got 0, and opened a chip outside its lease. So: resolve the BDF through sysfs
// (/sys/class/tenstorrent/tenstorrent!N/device → .../0000:03:00.0) and refuse if it doesn't resolve.
// A bare integer is still accepted for manual runs. Returns -1 when nothing matches.
static int first_visible_device(void)
{
    const char *v = getenv("TT_VISIBLE_DEVICES");
    if (!v || !*v) return -1;
    char first[64];
    size_t n = strcspn(v, ",");
    if (n >= sizeof first) return -1;
    memcpy(first, v, n);
    first[n] = 0;
    if (!strchr(first, ':')) return atoi(first);          // plain index
    for (int i = 0; i < 64; i++) {
        char link[128], target[512];
        snprintf(link, sizeof link, "/sys/class/tenstorrent/tenstorrent!%d/device", i);
        ssize_t len = readlink(link, target, sizeof target - 1);
        if (len < 0) continue;
        target[len] = 0;
        const char *base = strrchr(target, '/');
        if (base && strcmp(base + 1, first) == 0) return i;
    }
    return -1;
}

// hwmon value for /dev/tenstorrent/N, or NAN. `name` like "temp1_input".
static double hwmon(int dev, const char *name)
{
    char pattern[256];
    snprintf(pattern, sizeof pattern, "/sys/class/tenstorrent/tenstorrent!%d/device/hwmon/hwmon*/%s", dev, name);
    glob_t g;
    double out = NAN;
    if (glob(pattern, 0, NULL, &g) == 0 && g.gl_pathc > 0) {
        FILE *f = fopen(g.gl_pathv[0], "r");
        long long v;
        if (f && fscanf(f, "%lld", &v) == 1) out = (double)v;
        if (f) fclose(f);
    }
    globfree(&g);
    return out;
}

static double sysfs_attr(int dev, const char *name)
{
    char path[256];
    snprintf(path, sizeof path, "/sys/class/tenstorrent/tenstorrent!%d/%s", dev, name);
    FILE *f = fopen(path, "r");
    long long v;
    double out = NAN;
    if (f && fscanf(f, "%lld", &v) == 1) out = (double)v;
    if (f) fclose(f);
    return out;
}

static int mismatches = 0;

// Compare ours vs tt-kmd's, allowing `tol` for live-moving sensors (both are separate samples).
static void compare(const char *what, double ours, double theirs, double tol, const char *unit)
{
    const char *verdict;
    if (isnan(theirs)) verdict = "(no kmd value to compare)";
    else if (fabs(ours - theirs) <= tol) verdict = "agree";
    else { verdict = "MISMATCH"; mismatches++; }
    printf("║  %-10s ours %10.3f %-4s  kmd %10.3f %-4s  %s\n", what, ours, unit, theirs, unit, verdict);
}

// ── --dma: M3 on silicon ──────────────────────────────────────────────────────────────────
// tt-kmd allocates a NOC-DMA buffer and programs an outbound iATU region for it. We then:
//   1. find the active PCIe tile ourselves (BAR0 NOC_ID, via tt-kmd's BAR0 mmap, read-only);
//   2. READ the iATU region tt-kmd wrote (BAR2 mmap, read-only) and require it to equal
//      ttbh_iatu_outbound_encode(base, limit, target), bit for bit, which verifies our encoder
//      against the real thing;
//   3. loop back through the NOC: chip → host write, then host → chip read, using OUR window path
//      to the PCIe tile at TTBH_NOC_PCIE_OFFSET + base.
// On macOS the only differences will be: the target is a DART IOVA from the dext, and we write the
// iATU region ourselves with the same (already bit-compared) encoder.

#define MMAP_RES0_UC 0ull                 // tt-kmd memory.c: MMAP_OFFSET_RESOURCE0_UC (BAR0)
#define MMAP_RES1_UC (2ull << 36)         // MMAP_OFFSET_RESOURCE1_UC (BAR2)
#define DMA_SIZE     (64u * 1024u)

static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec; }

static int dma_check(kmd_ctx *k, const ttbh_window *w)
{
    int bad = 0;
    // BAR sizes from QUERY_MAPPINGS.
    struct { struct tenstorrent_query_mappings_in in; struct tenstorrent_mapping m[8]; } q;
    memset(&q, 0, sizeof q);
    q.in.output_mapping_count = 8;
    if (ioctl(k->fd, TENSTORRENT_IOCTL_QUERY_MAPPINGS, &q) != 0) { perror("QUERY_MAPPINGS"); return 1; }
    uint64_t bar2_size = 0;
    for (int i = 0; i < 8; i++) if (q.m[i].mapping_id == TENSTORRENT_MAPPING_RESOURCE1_UC) bar2_size = q.m[i].mapping_size;

    // 1. PCIe tile x. Map just the NOC2AXI page of BAR0 and present it at its real BAR0 offset.
    const uint64_t page_off = TTBH_NOC2AXI_CFG_START, page_len = 0x10000;
    void *cfg = mmap(NULL, page_len, PROT_READ, MAP_SHARED, k->fd, (off_t)(MMAP_RES0_UC + page_off));
    if (cfg == MAP_FAILED) { perror("mmap BAR0 NOC2AXI page"); return 1; }
    uint32_t pcie_x = 0;
    int err = ttbh_pcie_noc_x((volatile uint8_t *)cfg - page_off, page_off + page_len, &pcie_x);
    printf("║  PCIe tile  x=%u y=%u  %s\n", pcie_x, TTBH_PCIE_NOC_Y, err ? ttbh_strerror(err) : "detected");
    munmap(cfg, page_len);
    if (err) return 1;

    // 2. A NOC-DMA buffer from tt-kmd.
    struct tenstorrent_allocate_dma_buf a;
    memset(&a, 0, sizeof a);
    a.in.requested_size = DMA_SIZE;
    a.in.buf_index = 0;
    a.in.flags = TENSTORRENT_ALLOCATE_DMA_BUF_NOC_DMA;
    if (ioctl(k->fd, TENSTORRENT_IOCTL_ALLOCATE_DMA_BUF, &a) != 0) { perror("ALLOCATE_DMA_BUF"); return 1; }
    volatile uint32_t *host = mmap(NULL, a.out.size, PROT_READ | PROT_WRITE, MAP_SHARED, k->fd, (off_t)a.out.mapping_offset);
    if ((void *)host == MAP_FAILED) { perror("mmap DMA buf"); return 1; }
    uint64_t base = a.out.noc_address - TTBH_NOC_PCIE_OFFSET, limit = base + a.out.size - 1;
    printf("║  DMA buf    %u B  host dma 0x%llx  noc 0x%llx (base 0x%llx)\n", a.out.size,
           (unsigned long long)a.out.physical_address, (unsigned long long)a.out.noc_address, (unsigned long long)base);

    // 3. Find tt-kmd's region for it and compare with our encoder, register by register.
    void *bar2 = mmap(NULL, bar2_size, PROT_READ, MAP_SHARED, k->fd, (off_t)MMAP_RES1_UC);
    if (bar2 == MAP_FAILED) { perror("mmap BAR2"); return 1; }
    ttbh_iatu_regs want;
    ttbh_iatu_outbound_encode(base, limit, a.out.physical_address, &want);
    int found = -1;
    for (uint32_t r = 0; r < TTBH_IATU_REGIONS; r++) {
        ttbh_iatu_regs got;
        if (ttbh_iatu_outbound_read(bar2, bar2_size, r, &got)) continue;
        uint64_t tgt = ((uint64_t)got.upper_target << 32) | got.lower_target;
        if ((got.ctrl_2 & TTBH_IATU_REGION_EN) && tgt == a.out.physical_address) {
            found = (int)r;
            const char *names[] = { "lower_base", "upper_base", "lower_target", "upper_target", "lower_limit", "upper_limit", "ctrl_1", "ctrl_2", "ctrl_3" };
            const uint32_t *g = &got.lower_base, *wv = &want.lower_base;
            int diffs = 0;
            // UPPER_LIMIT (index 5) only implements 8 bits in hardware: compare what it can hold.
            for (int i = 0; i < 9; i++)
                if ((i == 5 ? (g[i] ^ wv[i]) & TTBH_IATU_UPPER_LIMIT_IMPL_MASK : g[i] ^ wv[i]) != 0) { printf("║    iATU %-12s kmd 0x%08x  ours 0x%08x  DIFFERENT\n", names[i], g[i], wv[i]); diffs++; }
            printf("║  iATU       region %u: %s\n", r, diffs ? "differs from our encoder" : "all 9 registers match our encoder (upper_limit on its 8 implemented bits)");
            bad += diffs != 0;
            break;
        }
    }
    munmap(bar2, bar2_size);
    if (found < 0) { printf("║  iATU       no enabled region targets the buffer?\n"); bad++; }

    // 4. Loopback. Chip → host: NOC-write to the PCIe tile; host sees it in the buffer.
    host[0x40 / 4] = 0;
    const uint32_t magic = 0xC0DE1234u ^ (uint32_t)now_ns();
    err = ttbh_noc_write32(w, pcie_x, TTBH_PCIE_NOC_Y, a.out.noc_address + 0x40, magic);
    uint64_t t0 = now_ns();
    while (!err && host[0x40 / 4] != magic && now_ns() - t0 < 500000000ull) { }
    int wrote = !err && host[0x40 / 4] == magic;
    printf("║  chip→host  0x%08x  %s (%.1f µs)\n", magic, wrote ? "landed in host memory" : "DID NOT ARRIVE", (now_ns() - t0) / 1000.0);
    bad += !wrote;

    // Host → chip: host writes, chip NOC-reads it back from host memory.
    host[0x80 / 4] = ~magic;
    __sync_synchronize();
    uint32_t back = 0;
    err = ttbh_noc_read32(w, pcie_x, TTBH_PCIE_NOC_Y, a.out.noc_address + 0x80, &back);
    int read_ok = !err && back == ~magic;
    printf("║  host→chip  0x%08x  %s\n", back, read_ok ? "read back through the NOC" : "MISMATCH");
    bad += !read_ok;

    munmap((void *)host, a.out.size);   // the buffer + its iATU region are freed when the fd closes
    return bad;
}

// ── --arc: the ARC message queue on silicon ───────────────────────────────────────────────
// Locate the queue and run TEST echoes (response payload[0] = value + 1) through libttbh's ring
// code, enough of them to wrap both rings. Safe on a leased device: tt-kmd only sends its own ARC
// messages at probe/reset/fw-log setup and on user ioctls, none of which happen while we hold the
// only fd. This doesn't take tt-kmd's arc_msg_mutex, which is why it needs the lease.
static int arc_check(const ttbh_window *w)
{
    uint32_t base = 0, n = 0;
    int err = ttbh_arc_msg_locate(w, &base, &n);
    printf("║  ARC queue  base 0x%08x  %u entries  %s\n", base, n, err ? ttbh_strerror(err) : "located");
    if (err) return 1;
    int bad = 0;
    const uint32_t rounds = 4 * n;           // wraps the request and response rings twice
    for (uint32_t i = 0; i < rounds; i++) {
        uint32_t value = 0xC0DE0000u + i, echo = 0;
        err = ttbh_arc_test(w, value, &echo);
        if (err || echo != value + 1) {
            printf("║  ARC TEST   #%u: %s, echo 0x%08x (want 0x%08x)\n", i, err ? ttbh_strerror(err) : "wrong echo", echo, value + 1);
            bad++;
        }
    }
    printf("║  ARC TEST   %u/%u echoes correct (value + 1)\n", rounds - bad, rounds);
    return bad != 0;
}

int main(int argc, char **argv)
{
    int dev = first_visible_device();
    int want_dma = 0, want_arc = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--device") && i + 1 < argc) dev = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dma")) want_dma = 1;
        else if (!strcmp(argv[i], "--arc")) want_arc = 1;
    }

    if (dev < 0) {
        fprintf(stderr, "no device: run under a gozer lease (TT_VISIBLE_DEVICES) or pass --device N\n");
        return 1;
    }
    char path[64];
    snprintf(path, sizeof path, "/dev/tenstorrent/%d", dev);
    kmd_ctx k = { .fd = open(path, O_RDWR | O_CLOEXEC) };
    if (k.fd < 0) { fprintf(stderr, "open %s: %s\n", path, strerror(errno)); return 1; }

    struct tenstorrent_allocate_tlb alloc;
    memset(&alloc, 0, sizeof alloc);
    alloc.in.size = TTBH_TLB_2M_SIZE;
    if (ioctl(k.fd, TENSTORRENT_IOCTL_ALLOCATE_TLB, &alloc) != 0) { perror("ALLOCATE_TLB"); return 1; }
    k.tlb_id = alloc.out.id;
    void *map = mmap(NULL, TTBH_TLB_2M_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, k.fd, (off_t)alloc.out.mmap_offset_uc);
    if (map == MAP_FAILED) { perror("mmap TLB"); return 1; }
    k.map = map;

    ttbh_window w = { &k, kmd_aim };
    const char *vis = getenv("TT_VISIBLE_DEVICES");
    printf("╔══ ttbh-kmd-check  %s  (tt-kmd user TLB %u; TT_VISIBLE_DEVICES=%s)\n", path, k.tlb_id, vis ? vis : "unset");

    uint32_t boot = 0;
    int err = ttbh_arc_boot_status(&w, &boot);
    printf("║  ARC boot status  0x%08x  %s\n", boot, err ? ttbh_strerror(err) : (ttbh_arc_ready(boot) ? "ready" : "NOT ready"));
    if (err || !ttbh_arc_ready(boot)) mismatches++;

    struct { const char *what; uint16_t tag; } tags[] = {
        { "asic_temp", TTBH_TAG_ASIC_TEMP }, { "power", TTBH_TAG_POWER }, { "vcore", TTBH_TAG_VCORE },
        { "current", TTBH_TAG_CURRENT }, { "aiclk", TTBH_TAG_AICLK }, { "heartbeat", TTBH_TAG_TIMER_HEARTBEAT },
    };
    uint32_t raw[6] = {0};
    for (unsigned i = 0; i < 6; i++) {
        err = ttbh_telemetry_read(&w, tags[i].tag, &raw[i]);
        if (err) { printf("║  %-10s %s\n", tags[i].what, ttbh_strerror(err)); mismatches++; }
    }

    // Tolerances: two separate samples of live sensors. AICLK is quantized and stable at idle.
    compare("asic_temp", ttbh_temp_c(raw[0]), hwmon(dev, "temp1_input") / 1000.0, 2.0, "C");
    compare("power", raw[1], hwmon(dev, "power1_input") / 1e6, 3.0, "W");
    compare("vcore", raw[2], hwmon(dev, "in0_input"), 15.0, "mV");
    compare("current", raw[3], hwmon(dev, "curr1_input") / 1000.0, 3.0, "A");
    compare("aiclk", raw[4], sysfs_attr(dev, "tt_aiclk"), 0.0, "MHz");

    // Heartbeat must advance: proves we're reading live firmware state, not a stale/constant word.
    usleep(300 * 1000);
    uint32_t hb2 = 0;
    ttbh_telemetry_read(&w, TTBH_TAG_TIMER_HEARTBEAT, &hb2);
    printf("║  heartbeat  %u → %u  %s\n", raw[5], hb2, hb2 != raw[5] ? "advancing" : "STUCK");
    if (hb2 == raw[5]) mismatches++;

    if (want_dma) mismatches += dma_check(&k, &w);
    if (want_arc) mismatches += arc_check(&w);

    munmap(map, TTBH_TLB_2M_SIZE);
    struct tenstorrent_free_tlb fr;
    memset(&fr, 0, sizeof fr);
    fr.in.id = k.tlb_id;
    ioctl(k.fd, TENSTORRENT_IOCTL_FREE_TLB, &fr);
    close(k.fd);

    printf("╚══ %s\n", mismatches ? "MISMATCHES — libttbh disagrees with tt-kmd" : "libttbh agrees with tt-kmd on real silicon");
    return mismatches ? 2 : 0;
}

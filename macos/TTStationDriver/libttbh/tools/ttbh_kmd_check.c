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
// It uses the first device in TT_VISIBLE_DEVICES (set by gozer), or --device N.

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

static int first_visible_device(void)
{
    const char *v = getenv("TT_VISIBLE_DEVICES");
    return (v && *v) ? atoi(v) : 0;
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

int main(int argc, char **argv)
{
    int dev = first_visible_device();
    for (int i = 1; i < argc; i++)
        if (!strcmp(argv[i], "--device") && i + 1 < argc) dev = atoi(argv[++i]);

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
    printf("╔══ ttbh-kmd-check  %s  (tt-kmd user TLB %u)\n", path, k.tlb_id);

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

    munmap(map, TTBH_TLB_2M_SIZE);
    struct tenstorrent_free_tlb fr;
    memset(&fr, 0, sizeof fr);
    fr.in.id = k.tlb_id;
    ioctl(k.fd, TENSTORRENT_IOCTL_FREE_TLB, &fr);
    close(k.fd);

    printf("╚══ %s\n", mismatches ? "MISMATCHES — libttbh disagrees with tt-kmd" : "libttbh agrees with tt-kmd on real silicon");
    return mismatches ? 2 : 0;
}

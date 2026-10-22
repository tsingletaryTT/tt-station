# tt-umd and tt-smi on macOS

`0001-macos-tt-kmd-emulation.patch` applies to tt-umd v0.9.14 (`56bf00cf`). With it, tt-umd builds
on macOS and reaches a Blackhole through TTStationDriver, and unmodified tt-smi 6.8.0 runs on it
(`tt-smi -s` in its default UMD mode).

## What the patch changes

- **`tt-kmd-lib/src/tt_kmd_darwin.c`** (new, about 630 lines): a user-space stand-in for tt-kmd's
  character device. `open("/dev/tenstorrent/0")` opens the dext (one connection per process,
  shared by every descriptor), `ioctl` answers the `TENSTORRENT_IOCTL_*` requests, and `mmap`
  resolves the offsets it hands out into the dext's BAR0/BAR2 mappings or its own DMA buffers.
  - TLB windows are BAR0's 2 MiB windows, programmed with libttbh's encoder. There are no 4 GiB
    windows, because BAR4 isn't assigned over Thunderbolt.
  - `PIN_PAGES` and `ALLOCATE_DMA_BUF` use the dext's PrepareDMA (DART IOVAs). With `NOC_DMA`, they
    also program outbound iATU regions, planned by libttbh as tt-kmd plans them.
  - `SET_POWER_STATE` sends the ARC power message through libttbh.
  - `LOCK_CTL` is a per-process table.
  - Reset, harvesting, peer BAR, dma-buf export and the scalar NOC ioctls return `ENOTSUP`.
- **`tt-kmd-lib/src/tt_kmd_lib.c`, `tt-umd/src/pcie/pci_device.cpp`**: device `open`/`close`/
  `ioctl`/`mmap`/`munmap` go through the shim on Apple. So do sysfs attributes (`revision`,
  `iommu_group/type`), the kmd version and `/dev/tenstorrent` enumeration. The PCI slot lookup
  now returns nothing when `/sys/bus/pci/slots` is missing, where before it threw.
- **`tt-umd-utils/src/robust_mutex_darwin.cpp`** (new): macOS has no robust pthread mutexes, so
  `RobustMutex` uses `flock` on a lock file. The kernel drops the lock when its holder dies.
- Small guards: `<linux/types.h>`, `MAP_POPULATE`/`MAP_HUGETLB`, `MADV_HUGEPAGE`, `-lrt`.

The shim compiles tt-station's `libttbh/ttbh.c` and includes `Shared/TTBlackholeABI.h`, found
through the CMake cache variable `TTSTATION_DRIVER_DIR` (default
`~/code/tt-station/macos/TTStationDriver`).

## Build and run

```bash
cd ~/code/tt-umd && git checkout v0.9.14 -b macos && git am ~/code/tt-station/macos/TTStationDriver/umd/0001-*.patch
brew install hwloc
uv venv -p 3.12 ~/.venvs/ttsmi-mac && source ~/.venvs/ttsmi-mac/bin/activate
CXXFLAGS=-I/opt/homebrew/include CFLAGS=-I/opt/homebrew/include LDFLAGS=-L/opt/homebrew/lib \
  uv pip install . -C cmake.define.TT_UMD_BUILD_SIMULATION=OFF -C cmake.define.TT_UMD_BUILD_TOOLS=OFF \
  -C cmake.define.TT_UMD_ENABLE_CLANG_TIDY=OFF
uv pip install --no-deps tt-smi==6.8.0      # its tt-umd==0.9.12 pin only has Linux wheels
uv pip install distro elasticsearch pydantic psutil pyyaml requests tqdm rich textual tomli \
  'tt_tools_common~=1.6.0'
# pyluwen: the broker fork (../luwen/README.md), built into this venv with maturin
python ttsmi_darwin.py -s
```

Stop the broker first. The dext takes one client at a time, and UMD opens it directly.

`ttsmi_darwin.py` exists because tt-smi and tt-tools-common read
`/sys/module/tenstorrent/version` themselves before using UMD, and macOS has no `/sys`. It
replaces that one function with the emulated version (2.11.0) and calls tt-smi's own `main()`.
The installed tt-smi is not modified.

`TT_DARWIN_TRACE=1` logs the shim's connections, DMA mappings and power messages to stderr.

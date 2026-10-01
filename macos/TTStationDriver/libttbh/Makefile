# libttbh — see ttbh.h.
#   make test         hardware-free tests (macOS or Linux)
#   make kmd-check    Linux only: build the QuietBox cross-check against tt-kmd
#                     (needs tt-kmd's ioctl.h; KMD_INCLUDE defaults to the dkms source tree of the
#                     LOADED module. A QuietBox can have several tt-kmd versions under /usr/src;
#                     picking the first by name got 2.10.0 while 2.11.0 was loaded.)
CC      ?= cc
CFLAGS  ?= -std=c11 -O2 -Wall -Wextra -Werror -D_DEFAULT_SOURCE
KMD_VERSION ?= $(shell cat /sys/module/tenstorrent/version 2>/dev/null)
KMD_INCLUDE ?= $(if $(KMD_VERSION),/usr/src/tenstorrent-$(KMD_VERSION))

.PHONY: test kmd-check clean
test: build/test_ttbh
	./build/test_ttbh

build/test_ttbh: ttbh.c ttbh.h sim/ttbh_sim.c sim/ttbh_sim.h tests/test_ttbh.c | build
	$(CC) $(CFLAGS) ttbh.c sim/ttbh_sim.c tests/test_ttbh.c -o $@

kmd-check: build/ttbh-kmd-check
build/ttbh-kmd-check: ttbh.c ttbh.h tools/ttbh_kmd_check.c | build
	@test -n "$(KMD_INCLUDE)" || { echo "tt-kmd ioctl.h not found; set KMD_INCLUDE=<dir containing ioctl.h>"; exit 1; }
	$(CC) $(CFLAGS) -I$(KMD_INCLUDE) ttbh.c tools/ttbh_kmd_check.c -lm -o $@

build:
	mkdir -p build
clean:
	rm -rf build

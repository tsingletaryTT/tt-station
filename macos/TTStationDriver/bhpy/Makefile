# bhpy — the blackhole-py Darwin adapter: C broker + Python client.
#   make test      build the simulated broker and run the Python client tests against it
#                  (macOS or Linux; set BHPY_DIR to a blackhole-py checkout for the parity tests)
CC     ?= cc
CFLAGS ?= -std=c11 -O2 -Wall -Wextra -Werror -D_DEFAULT_SOURCE
LIB    := ../libttbh
SRCS   := broker.c $(LIB)/ttbh.c $(LIB)/sim/ttbh_sim.c

.PHONY: test clean
test: build/ttbh-broker-sim build/ttbh-broker-ttsim
	TTBH_BROKER_SIM=$(CURDIR)/build/ttbh-broker-sim python3 -m unittest discover -s tests -v

build/ttbh-broker-sim: broker_sim.c broker.h $(SRCS) | build
	$(CC) $(CFLAGS) broker_sim.c $(SRCS) -o $@

# The broker over Tenstorrent's ttsim (a simulated Blackhole that runs firmware + kernels).
# Needs a libttsim.so for Blackhole: build github.com/tenstorrent/ttsim (`./make.py :build`, then
# src/_out/release_bh/libttsim.so; native on macOS) or download a Linux release binary.
build/ttbh-broker-ttsim: broker_ttsim.c broker.h broker.c ../libttbh/ttbh.c | build
	$(CC) $(CFLAGS) broker_ttsim.c broker.c ../libttbh/ttbh.c -ldl -o $@

build:
	mkdir -p build
clean:
	rm -rf build

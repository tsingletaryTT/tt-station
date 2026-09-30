# bhpy — the blackhole-py Darwin adapter: C broker + Python client.
#   make test      build the simulated broker and run the Python client tests against it
#                  (macOS or Linux; set BHPY_DIR to a blackhole-py checkout for the parity tests)
CC     ?= cc
CFLAGS ?= -std=c11 -O2 -Wall -Wextra -Werror
LIB    := ../libttbh
SRCS   := broker.c $(LIB)/ttbh.c $(LIB)/sim/ttbh_sim.c

.PHONY: test clean
test: build/ttbh-broker-sim
	TTBH_BROKER_SIM=$(CURDIR)/build/ttbh-broker-sim python3 -m unittest discover -s tests -v

build/ttbh-broker-sim: broker_sim.c broker.h $(SRCS) | build
	$(CC) $(CFLAGS) broker_sim.c $(SRCS) -o $@

build:
	mkdir -p build
clean:
	rm -rf build

// broker_iokit.h — entry point of the real (dext-backed) broker; see broker_iokit.c.
#ifndef TTBH_BROKER_IOKIT_H
#define TTBH_BROKER_IOKIT_H
// Serve blackhole-py clients on `socket_path` until SIGINT/SIGTERM. 0 on clean exit.
int ttbh_broker_serve_iokit(const char *socket_path);
#endif

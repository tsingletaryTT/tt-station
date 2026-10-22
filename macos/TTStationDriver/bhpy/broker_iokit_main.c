// broker_iokit_main.c — `ttbh-broker-iokit [SOCKET]`: the real-hardware broker as a standalone
// binary, the same code `TTStationDriver serve` runs. Useful while developing the broker: a
// development-signed dext built from TTBlackholeDriver.Team.entitlements allows any user client,
// so this ad-hoc-signed binary can open the card without reinstalling the host app.
#include <stdio.h>
#include <stdlib.h>
#include "broker_iokit.h"

int main(int argc, char **argv)
{
    if (argc > 2) { fprintf(stderr, "usage: %s [SOCKET]\n", argv[0]); return 2; }
    char def[1024];
    const char *home = getenv("HOME");
    snprintf(def, sizeof def, "%s/Library/Application Support/TTStation/ttbh.sock", home ? home : "");
    return ttbh_broker_serve_iokit(argc == 2 ? argv[1] : def);
}

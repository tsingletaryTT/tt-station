"""tt-toplike bridge (proof of concept): serve the Mac-attached card as tt-toplike remote frames.

    python3 -m ttstation_bhpy.toplike_bridge [--port 8766] [--hz 4]
    tt-toplike --remote 127.0.0.1:8766 --mode arcade

tt-toplike's remote mode connects to ws://HOST:PORT/telemetry and parses each text frame with its
own tt-smi -s parser (tt-toplike src/backend/ws.rs), the same frames tt-station-agentd streams from a
QuietBox. This bridge produces those frames on a Mac:

  * identity (board type, PCIe location, link) comes once from `tt-station --json local`, which reads
    the IORegistry and needs no driver;
  * live readings come from the broker's read-only telemetry.sock (bhpy/broker.c), which answers any
    number of readers while blackhole-py holds the card through ttbh.sock.

Only the fields tt-toplike reads are filled in; values the broker can't read are left out. stdlib
only: the WebSocket server below implements just enough of RFC 6455 for one-way text frames.
"""
import argparse
import base64
import datetime
import hashlib
import json
import os
import platform
import socket
import subprocess
import sys
import threading
import time

TELEMETRY_SOCKET = os.path.expanduser("~/Library/Application Support/TTStation/telemetry.sock")
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def card_identity():
    """The first Tenstorrent card `tt-station local` sees (IORegistry, no driver needed)."""
    try:
        out = subprocess.run(["tt-station", "--json", "local", "--no-models"], capture_output=True,
                             text=True, timeout=30).stdout
        cards = json.loads(out).get("cards") or []
    except (OSError, ValueError, subprocess.TimeoutExpired):
        cards = []
    if not cards:
        return {"board_type": "blackhole", "bus_id": "0000:00:00.0", "gen": 0, "width": 0}
    c = cards[0]
    bus, dev, fn = (c.get("location") or "0:0:0").split(":")
    link = c.get("link") or {}
    return {"board_type": c.get("board_type") or c.get("chip") or "blackhole",
            "bus_id": f"0000:{int(bus):02x}:{int(dev):02x}.{int(fn)}",
            "gen": link.get("gen") or 0, "width": link.get("width") or 0}


def read_broker(path=TELEMETRY_SOCKET):
    """One reading from the broker's telemetry socket, or None if it isn't there."""
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(2)
        s.connect(path)
        data = b""
        while not data.endswith(b"\n"):
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        s.close()
        return json.loads(data)
    except (OSError, ValueError):
        return None


def frame(ident, t):
    """A tt-smi -s shaped snapshot for one card, from identity + one broker reading."""
    tele, smbus = {}, {}
    if t:
        if t.get("aiclk_mhz") is not None:
            tele["aiclk"] = str(t["aiclk_mhz"]); smbus["AICLK"] = hex(t["aiclk_mhz"])
        if t.get("asic_temp_c") is not None:
            tele["asic_temperature"] = f"{t['asic_temp_c']:.1f}"
            smbus["ASIC_TEMPERATURE"] = hex(int(round(t["asic_temp_c"] * 65536)))
        if t.get("power_w") is not None:
            tele["power"] = f"{t['power_w']:.1f}"; smbus["TDP"] = hex(t["power_w"])
        if t.get("current_a") is not None:
            tele["current"] = f"{t['current_a']:.1f}"; smbus["TDC"] = hex(t["current_a"])
        if t.get("vcore_mv") is not None:
            tele["voltage"] = f"{t['vcore_mv'] / 1000:.2f}"; smbus["VCORE"] = hex(t["vcore_mv"])
        if t.get("heartbeat") is not None:
            tele["heartbeat"] = str(t["heartbeat"]); smbus["TIMER_HEARTBEAT"] = hex(t["heartbeat"])
    return {
        "time": datetime.datetime.now().isoformat(),
        "host_info": {"OS": "Darwin", "Platform": platform.machine(), "Hostname": socket.gethostname(),
                      "Driver": "TTStationDriver (macOS dext)", "Distro": f"macOS {platform.mac_ver()[0]}"},
        "device_info": [{
            "board_info": {"board_type": ident["board_type"], "bus_id": ident["bus_id"],
                           "board_id": "N/A", "coords": "N/A",
                           "pcie_speed": ident["gen"], "pcie_width": str(ident["width"])},
            "telemetry": tele,
            "smbus_telem": smbus,
        }],
    }


# ── a minimal one-way WebSocket server ──────────────────────────────────────────────────────
def ws_text(payload: bytes) -> bytes:
    n = len(payload)
    head = bytes([0x81])
    if n < 126:
        head += bytes([n])
    elif n < 1 << 16:
        head += bytes([126]) + n.to_bytes(2, "big")
    else:
        head += bytes([127]) + n.to_bytes(8, "big")
    return head + payload


def serve_client(conn, ident, interval):
    try:
        req = b""
        while b"\r\n\r\n" not in req:
            chunk = conn.recv(4096)
            if not chunk:
                return
            req += chunk
        lines = req.decode("latin-1").split("\r\n")
        if not lines[0].startswith("GET /telemetry"):
            conn.sendall(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            return
        key = next((l.split(":", 1)[1].strip() for l in lines if l.lower().startswith("sec-websocket-key:")), None)
        if not key:
            conn.sendall(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            return
        accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
        conn.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                      f"Sec-WebSocket-Accept: {accept}\r\n\r\n").encode())
        while True:
            conn.sendall(ws_text(json.dumps(frame(ident, read_broker())).encode()))
            time.sleep(interval)
    except OSError:
        pass
    finally:
        conn.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=8766)
    ap.add_argument("--hz", type=float, default=4.0)
    args = ap.parse_args()
    ident = card_identity()
    print(f"toplike bridge: {ident['board_type']} at {ident['bus_id']} (Gen{ident['gen']} x{ident['width']}); "
          f"readings from {TELEMETRY_SOCKET}", file=sys.stderr)
    print(f"toplike bridge: ws://127.0.0.1:{args.port}/telemetry  →  tt-toplike --remote 127.0.0.1:{args.port}",
          file=sys.stderr)
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", args.port))                     # this Mac only
    srv.listen(8)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=serve_client, args=(conn, ident, 1.0 / args.hz), daemon=True).start()


if __name__ == "__main__":
    main()

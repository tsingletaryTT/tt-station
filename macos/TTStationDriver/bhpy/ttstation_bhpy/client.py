"""Wire client for the ttbh broker (bhpy/broker.h). Stdlib only.

Every request gets exactly one response. SYSMEM responses carry an fd (SCM_RIGHTS) alongside the
header, so the host memory the chip DMAs into is mapped into THIS process too.
"""
import array
import os
import socket
import struct

REQ = struct.Struct("<IIQQQII")      # cmd, id, a0, a1, a2, payload_len, reserved
RESP = struct.Struct("<iIQQ")        # status, payload_len, r0, r1

HELLO, TLB_ALLOC, TLB_FREE, TLB_TARGET, TLB_READ, TLB_WRITE, TELEMETRY, ARC_MSG, SYSMEM, SYSMEM_FREE = range(10)

# ttbh_err (libttbh/ttbh.h), for readable exceptions.
ERRORS = {-1: "invalid argument", -2: "could not aim a TLB window", -3: "all-ones read: link down or card gone",
          -4: "telemetry table missing or malformed", -5: "telemetry tag not present",
          -6: "unsupported telemetry version", -7: "busy/full", -8: "ARC did not answer in time",
          -9: "ARC answered with an error status", -10: "ARC not ready for messages"}

DEFAULT_SOCKET = os.path.expanduser("~/Library/Application Support/TTStation/ttbh.sock")


class BrokerError(OSError):
    def __init__(self, cmd, status):
        super().__init__(f"ttbh broker command {cmd} failed: {ERRORS.get(status, status)} ({status})")
        self.status = status


class BrokerClient:
    def __init__(self, path=None):
        self.path = path or os.environ.get("TTBH_BROKER_SOCKET", DEFAULT_SOCKET)
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        # The broker serves one client at a time (the card has one owner). A second client's connect
        # succeeds into the listen backlog but is never answered; without a timeout it hangs forever
        # (as the first version of the tests did). With one, it fails with a clear message.
        self.sock.settimeout(float(os.environ.get("TTBH_BROKER_TIMEOUT", "30")))
        try:
            self.sock.connect(self.path)
        except OSError as e:
            self.sock.close()
            raise OSError(e.errno, f"cannot reach the ttbh broker at {self.path}; start it with "
                                   f"`TTStationDriver serve` (or ttbh-broker-sim for tests)") from e

    def _recv_exact(self, n):
        buf = bytearray()
        while len(buf) < n:
            try:
                chunk = self.sock.recv(n - len(buf))
            except socket.timeout:
                raise TimeoutError(f"ttbh broker at {self.path} did not answer: busy with another client?") from None
            if not chunk:
                raise ConnectionError("ttbh broker closed the connection")
            buf += chunk
        return bytes(buf)

    def call(self, cmd, id=0, a0=0, a1=0, a2=0, payload=b"", want_fd=False):
        self.sock.sendall(REQ.pack(cmd, id, a0, a1, a2, len(payload), 0) + payload)
        fd = None
        if want_fd:
            fds = array.array("i")
            try:
                msg, anc, _flags, _addr = self.sock.recvmsg(RESP.size, socket.CMSG_SPACE(fds.itemsize))
            except socket.timeout:
                raise TimeoutError(f"ttbh broker at {self.path} did not answer: busy with another client?") from None
            for level, typ, data in anc:
                if level == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                    fds.frombytes(data[: len(data) - (len(data) % fds.itemsize)])
            if fds:
                fd = fds[0]
            header = msg if len(msg) == RESP.size else msg + self._recv_exact(RESP.size - len(msg))
        else:
            header = self._recv_exact(RESP.size)
        status, plen, r0, r1 = RESP.unpack(header)
        data = self._recv_exact(plen) if plen else b""
        if status != 0:
            if fd is not None:
                os.close(fd)
            raise BrokerError(cmd, status)
        return r0, r1, data, fd

    def close(self):
        self.sock.close()

#!/usr/bin/env python3
"""Guest-side network tool for the phase-2 bench (copied in over the agent).

The Ubuntu template ships no iperf3, so the bench's virtio-net workloads run
this stdlib-only script inside the guest. Every mode prints one JSON line
with the bytes that crossed the socket and the elapsed seconds; the host
computes the rate. One process per direction; nothing here runs on the host.

Modes:
    udp-blast DST PORT PAYLOAD SECS   sendto loop, blocking socket (device
                                      backpressure is the drain rate)
    udp-sink  PORT SECS               big-rcvbuf discard sink, counts bytes
    tcp-blast DST PORT SECS           connect, sendall of a 1 MiB buffer
    tcp-sink  PORT                    accept one connection, count until EOF
"""

import json
import socket
import sys
import time


def now():
    return time.monotonic()


def emit(kind, **fields):
    fields["type"] = kind
    print(json.dumps(fields), flush=True)


def udp_blast(dst, port, payload, secs):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    buf = b"v" * payload
    sent = 0
    end = now() + secs
    while now() < end:
        try:
            sock.sendto(buf, (dst, port))
            sent += payload
        except OSError:
            break
    emit("udp_blast", bytes=sent, secs=round(secs, 3))


def udp_sink(port, secs):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 32 * 1024 * 1024)
    sock.bind(("0.0.0.0", port))
    sock.settimeout(1.0)
    got = 0
    end = now() + secs
    while now() < end:
        try:
            data = sock.recv(70000)
            got += len(data)
        except socket.timeout:
            continue
    emit("udp_sink", bytes=got, secs=round(secs, 3))


def tcp_blast(dst, port, secs):
    sock = socket.create_connection((dst, port), timeout=30)
    buf = b"t" * (1024 * 1024)
    sent = 0
    end = now() + secs
    while now() < end:
        try:
            sock.sendall(buf)
            sent += len(buf)
        except OSError:
            break
    sock.close()
    emit("tcp_blast", bytes=sent, secs=round(secs, 3))


def tcp_sink(port):
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 32 * 1024 * 1024)
    server.bind(("0.0.0.0", port))
    server.listen(1)
    conn, _ = server.accept()
    got = 0
    while True:
        data = conn.recv(1024 * 1024)
        if not data:
            break
        got += len(data)
    conn.close()
    emit("tcp_sink", bytes=got)


def udp_blast(dst, port, payload, secs):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    buf = b"v" * payload
    sent = 0
    errors = 0
    last_error = ""
    end = now() + secs
    while now() < end:
        try:
            sock.sendto(buf, (dst, port))
            sent += payload
        except OSError as e:
            errors += 1
            last_error = f"{e.errno}: {e.strerror}"
            if errors > 100:
                break
    emit("udp_blast", bytes=sent, errors=errors, last_error=last_error,
         secs=round(secs, 3))


def main(argv):
    if len(argv) >= 2 and argv[1] == "udp-blast" and len(argv) == 6:
        udp_blast(argv[2], int(argv[3]), int(argv[4]), float(argv[5]))
    elif len(argv) >= 2 and argv[1] == "udp-sink" and len(argv) == 4:
        udp_sink(int(argv[2]), float(argv[3]))
    elif len(argv) >= 2 and argv[1] == "tcp-blast" and len(argv) == 5:
        tcp_blast(argv[2], int(argv[3]), float(argv[4]))
    elif len(argv) >= 2 and argv[1] == "tcp-sink" and len(argv) == 3:
        tcp_sink(int(argv[2]))
    else:
        sys.stderr.write(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

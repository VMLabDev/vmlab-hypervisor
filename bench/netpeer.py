"""Lifecycle for the bench's host-side network peer (bench/netpeer.c).

The peer speaks vmlab's switch framing on one unix socket (the netdev of
whichever backend connects) and a tiny JSON-line control protocol on
another. `compile_tools` builds the C pieces once per worktree; the
classes own their process and always reap it.
"""

import json
import os
import socket
import subprocess
import time

import lib

BIN_DIR = os.path.join(lib.RUN_DIR, "bin")
NETPEER_BIN = os.path.join(BIN_DIR, "netpeer")
_RANDBLKE_BIN = os.path.join(BIN_DIR, "randblke")


def compile_tools(log=None):
    """Build netpeer and randblke (cc -O2 -static) if absent. Returns the
    randblke path; raises with the compiler's output when it cannot."""
    os.makedirs(BIN_DIR, exist_ok=True)
    bench = os.path.dirname(os.path.abspath(__file__))
    for name in ("netpeer", "randblke"):
        out = os.path.join(BIN_DIR, name)
        if os.path.isfile(out):
            continue
        cmd = ["cc", "-O2", "-static", "-o", out, os.path.join(bench, f"{name}.c")]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise lib.BenchError(f"building {name}.c failed:\n{proc.stderr}")
        if log:
            log(f"built {out}")
    return _RANDBLKE_BIN


def randblke_path():
    return _RANDBLKE_BIN


class _Ctl:
    """The control connection: one JSON line out, one back."""

    def __init__(self, path, timeout=120):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.f = self.sock.makefile("rwb")

    def request(self, **msg):
        self.f.write((json.dumps(msg) + "\n").encode())
        self.f.flush()
        return json.loads(self.f.readline().decode())

    def close(self):
        try:
            self.f.close()
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass


class Peer:
    """`netpeer peer`: the switch socket for one VM plus its control line.

    Start before the VM launches (the netdev connects at process start);
    `report()` snapshots and resets the counters, so two reports bracket a
    measurement window.
    """

    def __init__(self, run_dir, guest_ip, guest_mac, udp_port, log=None):
        self.run_dir = run_dir
        self.switch_sock = os.path.join(run_dir, "switch.sock")
        self.ctl_sock = os.path.join(run_dir, "peerctl.sock")
        for path in (self.switch_sock, self.ctl_sock):
            if os.path.exists(path):
                os.remove(path)
        self.proc = subprocess.Popen(
            [
                NETPEER_BIN, "peer", self.switch_sock, self.ctl_sock,
                lib.BENCH_NET["peer_ip"], lib.BENCH_NET["peer_mac"],
                guest_ip, guest_mac, str(udp_port),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 10
        while not os.path.exists(self.switch_sock):
            if self.proc.poll() is not None:
                raise lib.BenchError(
                    f"netpeer died: {self.proc.stderr.read().decode()[-500:]}"
                )
            if time.monotonic() > deadline:
                self.kill()
                raise lib.BenchError("netpeer never bound its socket")
            time.sleep(0.05)
        self.ctl = None
        self.log = log

    def control(self):
        if self.ctl is None:
            self.ctl = _Ctl(self.ctl_sock)
        return self.ctl

    def report(self):
        """Snapshot (and reset) the peer's counters."""
        return self.control().request(cmd="report")

    def blast(self, secs, payload):
        """Fire the RX blast; returns when the peer acknowledges."""
        reply = self.control().request(cmd="blast", secs=secs, payload=payload)
        if not reply.get("ok"):
            raise lib.BenchError(f"netpeer blast refused: {reply}")

    def kill(self):
        if self.ctl is not None:
            try:
                self.ctl.request(cmd="quit")
            except Exception:
                pass
            self.ctl.close()
            self.ctl = None
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)



class Hub:
    """`netpeer hub`: the two-port segment for guest-to-guest runs."""

    def __init__(self, run_dir):
        self.switch_sock = os.path.join(run_dir, "g2g.sock")
        if os.path.exists(self.switch_sock):
            os.remove(self.switch_sock)
        self.proc = subprocess.Popen(
            [NETPEER_BIN, "hub", self.switch_sock],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 10
        while not os.path.exists(self.switch_sock):
            if self.proc.poll() is not None or time.monotonic() > deadline:
                self.kill()
                raise lib.BenchError("netpeer hub never bound its socket")
            time.sleep(0.05)

    def kill(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()

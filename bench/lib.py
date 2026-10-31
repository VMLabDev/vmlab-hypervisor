"""Shared plumbing for the D2 benchmark suite (bench/README.md).

The suite measures the three D2 workloads (boot to agent ready, sha256 over
256 MiB, `xz -9` over 32 MiB) under QEMU TCG and, once it supports foreign
archs, under `vmlab-hv` with `accel: tcg`. One VM runs at a time, always
inside a `systemd-run --user --scope` memory cap and pinned with `taskset`.

The QEMU argv below is vmlab's own argv for a linux-modern profile VM on the
`virt` machine, as captured in
`.scratch/plans/phase-3-tcg/notes/03-architecture-baseline.md` (scratch driver
`target/p3-research/g3/bench.py`). Do not drift it without re-baselining.
"""

import json
import os
import shutil
import socket
import statistics
import struct
import subprocess
import time

BENCH_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_DIR = os.path.dirname(BENCH_DIR)
HISTORY_DIR = os.path.join(BENCH_DIR, "history")
RUN_DIR = os.path.join(BENCH_DIR, "run")

TEMPLATE_ROOT = os.path.expanduser("~/.local/share/vmlab/templates")
# What vmlab's firmware::Sources::installed picks on this host (notes/03).
FIRMWARE = {
    "aarch64": (
        os.path.expanduser("~/.local/share/vmlab/guest/firmware/aarch64/AAVMF_CODE.fd"),
        os.path.expanduser("~/.local/share/vmlab/guest/firmware/aarch64/AAVMF_VARS.fd"),
    ),
    "riscv64": (
        "/usr/share/edk2/riscv64/RISCV_VIRT_CODE.fd",
        "/usr/share/edk2/riscv64/RISCV_VIRT_VARS.fd",
    ),
}
MACHINE_ARG = {"aarch64": "virt", "riscv64": "virt,acpi=off"}

# The safety cap every VM-booting command runs under (wave-4 common brief).
MEMORY_MAX = "24G"

# The three D2 workloads, in report order.
WORKLOADS = ("boot_to_agent_ready", "sha256_256M", "xz_9_32M")
UNIT = "s"

# Guest-side commands. Fixed inputs, timed host-side; identical to notes/03.
PREP_ARGV = [
    "/bin/sh",
    "-c",
    "dd if=/dev/zero of=/tmp/bench-sha.bin bs=1M count=256 status=none && "
    "yes vmlabbench | head -c 33554432 > /tmp/bench-xz.bin && sync",
]
WORKLOAD_ARGV = {
    "sha256_256M": ["/usr/bin/sha256sum", "/tmp/bench-sha.bin"],
    "xz_9_32M": ["/bin/sh", "-c", "xz -9 -T1 -c /tmp/bench-xz.bin > /tmp/bench.xz"],
}
VERIFY_ARGV = [
    "/bin/sh",
    "-c",
    "xz -dk -c /tmp/bench.xz | cmp - /tmp/bench-xz.bin && echo XZ-OK && "
    "nproc && grep -c processor /proc/cpuinfo",
]

# vmlab guest agent protocol v2 frames (guest/agent-proto): 4-byte magic,
# u32-LE length, kind, u32-LE channel, payload.
AGENT_MAGIC = b"VMLB"
AGENT_PROTO_VERSION = 2

# vmlab-hv control protocol (vmlab-hv-proto): one JSON message per line.
HV_PROTO_VERSION = 1


class BenchError(Exception):
    """A failure that belongs to the suite itself (not a VM measurement)."""


class BackendUnsupported(Exception):
    """The backend cannot run this machine yet; `reason` says why.

    Raised for the `vmlab-hv` backend until phase 3 lands foreign-arch TCG
    support. The runner records the skip and moves on.
    """

    def __init__(self, backend, reason):
        super().__init__(f"{backend} skipped: {reason}")
        self.backend = backend
        self.reason = reason


# ---------------------------------------------------------------------------
# Guest agent client (vmlab.agent.0 on the virtio-serial port)
# ---------------------------------------------------------------------------


def agent_frame(kind, channel, payload):
    return (
        AGENT_MAGIC
        + struct.pack("<I", len(payload))
        + bytes([kind])
        + struct.pack("<I", channel)
        + payload
    )


class AgentClient:
    """The readiness poller and in-guest exec channel, as vmlab drives them.

    vmlab dials fresh every second: an early persistent connection's Hello is
    eaten before the guest opens the port and never answered (notes/03 §1).
    """

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.buf = b""
        self.channels = {}

    def _recv_frame(self, timeout):
        self.sock.settimeout(timeout)
        while True:
            i = self.buf.find(AGENT_MAGIC)
            if i < 0:
                self.buf = self.buf[-3:]
            elif i > 0:
                self.buf = self.buf[i:]
            elif len(self.buf) >= 13:
                ln, kind, ch = struct.unpack("<IBI", self.buf[4:13])
                if len(self.buf) >= 13 + ln:
                    payload = self.buf[13 : 13 + ln]
                    self.buf = self.buf[13 + ln :]
                    return kind, ch, payload
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("agent socket closed")
            self.buf += chunk

    def handshake(self, timeout=3):
        msg = json.dumps(
            {"cmd": "hello", "proto_version": AGENT_PROTO_VERSION, "token": "vmlab-bench"}
        ).encode()
        self.sock.sendall(agent_frame(0, 0, msg))
        kind, ch, payload = self._recv_frame(timeout)
        if kind != 0 or ch != 0:
            raise BenchError(f"unexpected agent frame: kind={kind} channel={ch}")
        return json.loads(payload)

    def exec(self, argv, timeout, idle_timeout=None):
        cid = 1
        self.channels[cid] = {"data": b"", "stderr": b"", "code": None, "eof": False}
        msg = json.dumps({"cmd": "open_exec", "id": cid, "argv": argv}).encode()
        self.sock.sendall(agent_frame(0, 0, msg))
        deadline = time.monotonic() + timeout
        last_data = time.monotonic()
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"exec timeout: {argv[:3]}")
            idle = idle_timeout or timeout
            try:
                kind, ch, payload = self._recv_frame(min(remaining, idle))
            except socket.timeout:
                if time.monotonic() - last_data > idle:
                    raise TimeoutError(f"exec idle: {argv[:3]}") from None
                continue
            last_data = time.monotonic()
            if kind == 0:
                m = json.loads(payload)
                ev = m.get("event")
                if ev in ("window_adjust", "opened") and m.get("id") == cid:
                    continue
                if ev == "eof" and m.get("id") == cid:
                    self.channels[cid]["eof"] = True
                elif ev == "exited" and m.get("id") == cid:
                    self.channels[cid]["code"] = m["code"]
                elif ev == "error":
                    raise BenchError(f"agent error: {m}")
                chan = self.channels[cid]
                if chan["code"] is not None and chan["eof"]:
                    return chan
            elif kind == 1 and ch == cid:
                self.channels[cid]["data"] += payload
            elif kind == 2 and ch == cid:
                self.channels[cid]["stderr"] += payload

    def shutdown(self):
        """Ask the guest for an ACPI powerdown; best effort."""
        try:
            self.sock.sendall(
                agent_frame(
                    0, 0, json.dumps({"cmd": "shutdown", "mode": "powerdown"}).encode()
                )
            )
            self.sock.settimeout(30)
            while True:
                self._recv_frame(30)
        except Exception:
            pass

    def close(self):
        try:
            self.sock.close()
        except Exception:
            pass


def wait_for_agent(path, deadline_seconds, vm_exited=None):
    """Dial fresh per attempt, 1 s cadence, like vmlab's readiness poller.

    `vm_exited`, when given, returns the VM's exit code if the process died
    (a crashed VM fails the run immediately instead of running out the
    clock), and None while it lives.
    """
    end = time.monotonic() + deadline_seconds
    while time.monotonic() < end:
        if os.path.exists(path):
            cand = None
            try:
                cand = AgentClient(path)
                hello = cand.handshake(3)
                return cand, hello
            except Exception:
                if cand is not None:
                    cand.close()
        code = vm_exited() if vm_exited else None
        if code is not None:
            raise BenchError(f"VM exited (code {code}) while waiting for the agent")
        time.sleep(1)
    raise TimeoutError(f"agent never answered its handshake on {path}")


# ---------------------------------------------------------------------------
# QMP (QEMU backend)
# ---------------------------------------------------------------------------


def qmp_cont(path, timeout=60):
    """Release the vCPUs: greeting, capabilities, cont (vmlab's sequence)."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.settimeout(timeout)

    def rd():
        buf = b""
        while True:
            buf += s.recv(65536)
            try:
                return json.loads(buf.decode())
            except Exception:
                continue

    rd()  # greeting
    s.sendall(b'{"execute":"qmp_capabilities"}')
    rd()
    s.sendall(b'{"execute":"cont"}')
    rd()
    s.close()


# ---------------------------------------------------------------------------
# vmlab-hv control client
# ---------------------------------------------------------------------------


class HvControlClient:
    """The vmlab-hv control socket: handshake, then one command per request."""

    def __init__(self, path, timeout=60):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(timeout)
        self.f = self.sock.makefile("rwb")
        greeting = self._read()
        if greeting.get("type") != "hello":
            raise BenchError(f"unexpected control greeting: {greeting}")
        self.hv_version = greeting.get("hv_version", "?")
        self.f.write(
            (json.dumps({"type": "hello", "proto_version": HV_PROTO_VERSION}) + "\n").encode()
        )
        self.f.flush()
        reply = self._read()
        if reply.get("type") == "error":
            raise BenchError(f"control handshake rejected: {reply}")
        if reply.get("proto_version") != HV_PROTO_VERSION:
            raise BenchError(f"control version mismatch: {reply}")
        self.next_id = 1

    def _read(self):
        line = self.f.readline()
        if not line:
            raise ConnectionError("control socket closed")
        return json.loads(line.decode())

    def request(self, command, **kwargs):
        msg = {"type": "request", "id": self.next_id, "command": command}
        msg.update(kwargs)
        self.next_id += 1
        self.f.write((json.dumps(msg) + "\n").encode())
        self.f.flush()
        while True:
            m = self._read()
            if m.get("type") == "reply" and m.get("id") == self.next_id - 1:
                if m.get("outcome") != "ok":
                    raise BenchError(f"control {command} failed: {m}")
                return m
            # Events (e.g. run-state changes) are ignored here.

    def close(self):
        try:
            self.f.close()
        except Exception:
            pass
        try:
            self.sock.close()
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Templates, overlays, firmware
# ---------------------------------------------------------------------------


def latest_template(arch, guest):
    """The newest version dir of a template; templates are never written."""
    guest_dir = os.path.join(TEMPLATE_ROOT, arch, guest)
    if not os.path.isdir(guest_dir):
        raise BenchError(f"no template for {arch}/{guest}: {guest_dir} missing")
    versions = []
    for name in os.listdir(guest_dir):
        disk = os.path.join(guest_dir, name, "disk.qcow2")
        if os.path.isfile(disk):
            versions.append((_version_key(name), name))
    if not versions:
        raise BenchError(f"no disk.qcow2 under {guest_dir}")
    return max(versions)[1]


def _version_key(name):
    return tuple(int(p) if p.isdigit() else p for p in name.split("."))


def prepare_run_dir(run, arch, guest, version):
    """A fresh qcow2 overlay over the (never-written) template, plus vars."""
    template = os.path.join(TEMPLATE_ROOT, arch, guest, version, "disk.qcow2")
    if not os.path.isfile(template):
        raise BenchError(f"template disk missing: {template}")
    overlay = os.path.join(run, "overlay.qcow2")
    subprocess.run(
        ["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", template, overlay],
        check=True,
    )
    code, vars_template = FIRMWARE[arch]
    shutil.copyfile(vars_template, os.path.join(run, "vars.fd"))
    return overlay, code, os.path.join(run, "vars.fd")


# ---------------------------------------------------------------------------
# Noise control
# ---------------------------------------------------------------------------


def pin_cpus(vcpus, override=None):
    """The `taskset -c` window: full physical cores, top of the topology.

    On the SMT host this suite was written on (32 logical CPUs, sibling of
    cpu N is N-16), `16..` is one thread per physical core, away from cpu 0
    where kernel and daemon work concentrates. smp1 gets four cores of room,
    smp4 gets eight; every backend gets the same window for its vcpus.
    """
    if override:
        return override
    cpus = os.cpu_count() or 8
    half = cpus // 2
    width = 4 if vcpus == 1 else 8
    return f"{half}-{min(cpus - 1, half + width - 1)}"

def taskset_prefix(pin):
    return ["taskset", "-c", pin]


def systemd_run_prefix(memory_max=MEMORY_MAX):
    return [
        "systemd-run",
        "--user",
        "--scope",
        "-q",
        "-p",
        f"MemoryMax={memory_max}",
        "-p",
        "MemorySwapMax=0",
        "--",
    ]


def read_governor():
    """The scaling governor, sampled at the bottom, middle and top of the box.

    `None` when cpufreq is absent (the fact is then simply not recorded).
    """
    values = []
    n = os.cpu_count() or 2
    for cpu in {0, n // 2, n - 1}:
        try:
            with open(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor") as f:
                values.append(f.read().strip())
        except OSError:
            return None
    return ",".join(sorted(set(values)))


def read_loadavg():
    try:
        with open("/proc/loadavg") as f:
            return [float(x) for x in f.read().split()[:3]]
    except OSError:
        return None


# ---------------------------------------------------------------------------
# Statistics and history
# ---------------------------------------------------------------------------


def median_and_spread(samples):
    """Median plus max-min spread, the noise figure bench-check compares on."""
    med = statistics.median(samples)
    spread = max(samples) - min(samples)
    return round(med, 2), round(spread, 2)


def history_path(arch):
    return os.path.join(HISTORY_DIR, f"{arch}.jsonl")


def append_history(arch, record):
    os.makedirs(HISTORY_DIR, exist_ok=True)
    with open(history_path(arch), "a") as f:
        f.write(json.dumps(record) + "\n")


def read_history(arch):
    path = history_path(arch)
    records = []
    if not os.path.exists(path):
        return records
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                records.append(json.loads(line))
    return records


def latest_baseline(records, arch, guest, vcpus, workload):
    """The newest QEMU-backend record for this config, for hv history lines."""
    for rec in reversed(records):
        if (
            rec.get("backend") == "qemu"
            and rec.get("arch") == arch
            and rec.get("guest") == guest
            and rec.get("vcpus") == vcpus
            and rec.get("workload") == workload
        ):
            return {
                "median": rec.get("median"),
                "spread": rec.get("spread"),
                "commit": rec.get("commit"),
                "recorded": rec.get("recorded"),
            }
    return None


# The floor keeps tiny absolute spreads (fast workloads, or a lucky set of
# repeats) from making bench-check fail on moves that are pure scheduler
# jitter. Five percent of the previous median, and at least the spreads of
# both runs.
NOISE_FLOOR = 0.05


def regression_verdict(previous, latest):
    """('ok' | 'better' | 'worse', allowed-seconds) for latest vs previous."""
    allowed = max(
        previous.get("spread") or 0.0,
        latest.get("spread") or 0.0,
        NOISE_FLOOR * previous["median"],
    )
    delta = latest["median"] - previous["median"]
    if delta > allowed:
        return "worse", round(allowed, 2)
    if delta < -allowed:
        return "better", round(allowed, 2)
    return "ok", round(allowed, 2)

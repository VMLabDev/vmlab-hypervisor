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

import itertools
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


def project_vm_pids(proc_root="/proc"):
    """Count VMs across worktrees, excluding the owner's Android emulator."""
    pids = []
    for pid in os.listdir(proc_root):
        if not pid.isdecimal():
            continue
        try:
            with open(os.path.join(proc_root, pid, "cmdline"), "rb") as f:
                argv = f.read().split(b"\0")
        except FileNotFoundError:
            continue
        if not argv or not argv[0]:
            continue
        executable = os.path.basename(os.fsdecode(argv[0]))
        if executable == "vmlab-hv" or executable.startswith("vmlab-hv-test-") or (
            executable.startswith("qemu-system-") and b"-avd" not in argv
        ):
            pids.append(int(pid))
    return sorted(pids)


def require_vm_slots(count=1):
    """Check the host-wide VM cap immediately before every launch."""
    pids = project_vm_pids()
    limit = int(os.environ.get("VMLAB_HV_TEST_VM_LIMIT", "4"))
    if len(pids) + count > limit:
        raise BenchError(f"need {count} VM slots; running project VM PIDs: {pids}")

# ---------------------------------------------------------------------------
# x86_64 KVM (phase 2, HV-D28): templates, firmware, workloads
# ---------------------------------------------------------------------------

# The firmware binaries the two backends share. vmlab-hv names SeaBIOS
# explicitly; QEMU loads the same file from its own data directory when the
# argv carries no pflash drive (docs/hwref: BIOS vendor SeaBIOS 1.17.0).
SEABIOS = "/usr/share/qemu/bios-256k.bin"
# vmlab's bundled edk2 pair (src/qemu/firmware.rs `bundled_names`, x86_64
# plain pairing) — the highest-priority source on this host.
OVMF_CODE = os.path.expanduser(
    "~/.local/share/vmlab/guest/firmware/x86_64/OVMF_CODE_4M.fd"
)
OVMF_VARS = os.path.expanduser(
    "~/.local/share/vmlab/guest/firmware/x86_64/OVMF_VARS_4M.fd"
)
# The iPXE ROM the hv NIC carries (just firmware-ipxe-build). QEMU's NIC
# keeps its own default combo ROM; D16 compares presence and identity, not
# bytes, and neither ROM is on the measured path.
IPXE_ROM = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "ipxe", "virtio-net.rom")

# The x86_64 templates the phase-2 gate runs on (package brief): profile
# shape from vmlab's profiles/shipped.wcl + the template's own template.wcl.
X86_GUESTS = {
    "ubuntu-26.04": {
        "firmware": "seabios",
        "vcpus": 2,
        "memory_mib": 4096,
        "hyperv": False,
    },
    "windows-10": {
        "firmware": "ovmf",
        "vcpus": 4,
        "memory_mib": 8192,
        "hyperv": True,
    },
    "windows-server-2025": {
        "firmware": "ovmf",
        "vcpus": 4,
        "memory_mib": 8192,
        "hyperv": True,
    },
}

# The benchmark network: both backends attach the NIC as a stream-socket
# client of vmlab's switch framing (4-byte big-endian length + one frame).
# The bench serves that socket with bench/netpeer (README §net).
BENCH_NET = {
    "guest_ip": "192.168.50.2",
    "peer_ip": "192.168.50.1",
    "peer_mac": "52:54:00:aa:00:01",
    "guest_mac": "52:54:00:12:34:56",
    # Jumbo frames keep the Python guest tools and the framing peer off the
    # measured ceiling; both backends get the identical host_mtu deviation.
    "mtu": 65520,
    "udp_payload": 65000,
    "port": 5901,
    "g2g_a_ip": "192.168.60.2",
    "g2g_a_mac": "52:54:00:12:34:60",
    "g2g_b_ip": "192.168.60.3",
    "g2g_b_mac": "52:54:00:12:34:61",
}

# Workloads per suite, in report order. x86_64 windows boots to agent ready
# only (package brief); ubuntu carries the full phase-2 set.
TCG_WORKLOADS = ("boot_to_agent_ready", "sha256_256M", "xz_9_32M")
X86_UBUNTU_WORKLOADS = (
    "boot_to_agent_ready",
    "blk_seq_read_mbps",
    "blk_seq_write_mbps",
    "blk_rand_read_4k_iops",
    "blk_rand_write_4k_iops",
    "net_tx_udp_mbps",
    "net_rx_udp_mbps",
    "idle_cpu_pct",
)
X86_WINDOWS_WORKLOADS = ("boot_to_agent_ready",)
X86_G2G_WORKLOADS = ("net_g2g_a2b_tcp_mbps", "net_g2g_b2a_tcp_mbps")

UNIT = {
    "boot_to_agent_ready": "s",
    "sha256_256M": "s",
    "xz_9_32M": "s",
    "blk_seq_read_mbps": "MiB/s",
    "blk_seq_write_mbps": "MiB/s",
    "blk_rand_read_4k_iops": "iops",
    "blk_rand_write_4k_iops": "iops",
    "net_tx_udp_mbps": "Mbit/s",
    "net_rx_udp_mbps": "Mbit/s",
    "net_g2g_a2b_tcp_mbps": "Mbit/s",
    "net_g2g_b2a_tcp_mbps": "Mbit/s",
    "idle_cpu_pct": "%cpu",
}


def workloads(arch, guest="debian-13", suite="full"):
    """The workload list for one run, in report order."""
    if arch != "x86_64":
        return TCG_WORKLOADS
    if suite == "g2g":
        return X86_G2G_WORKLOADS
    return X86_WINDOWS_WORKLOADS if guest.startswith("windows-") else X86_UBUNTU_WORKLOADS


# HV-D28 bars for the x86_64 suite: boot no slower than QEMU (ratio <= 1.00),
# virtio-blk and virtio-net at least 80% of QEMU's throughput. Idle CPU is
# reported against QEMU but gates nothing. TCG arches keep the phase-3 bar
# (>= 1.00x) that bench-report already prints.
D28_BAR = {
    "boot_to_agent_ready": ("<=", 1.00),
    "blk_seq_read_mbps": (">=", 0.80),
    "blk_seq_write_mbps": (">=", 0.80),
    "blk_rand_read_4k_iops": (">=", 0.80),
    "blk_rand_write_4k_iops": (">=", 0.80),
    "net_tx_udp_mbps": (">=", 0.80),
    "net_rx_udp_mbps": (">=", 0.80),
    "net_g2g_a2b_tcp_mbps": (">=", 0.80),
    "net_g2g_b2a_tcp_mbps": (">=", 0.80),
    "idle_cpu_pct": None,
}

# Which way is a regression, for bench-check. Everything the TCG suite
# records is seconds; idle CPU is a percentage — both grow when worse. The
# phase-2 throughput workloads are rates that shrink when worse.
DIRECTION = {
    name: ("lower" if unit in ("s", "%cpu") else "higher")
    for name, unit in UNIT.items()
}

# The three D2 workloads, in report order (alias for the TCG suites).
WORKLOADS = TCG_WORKLOADS

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

    _channel_ids = itertools.count(1)

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

    def exec(self, argv, timeout, idle_timeout=None, stdin_data=None):
        cid = next(self._channel_ids)
        if cid > 0xffffffff:
            raise BenchError("agent channel ID space exhausted")
        self.channels[cid] = {"data": b"", "stderr": b"", "code": None, "eof": False}
        msg = json.dumps({"cmd": "open_exec", "id": cid, "argv": argv}).encode()
        self.sock.sendall(agent_frame(0, 0, msg))
        if stdin_data is not None:
            # Exec stdin rides DATA frames on the channel, closed by `eof`;
            # chunks stay under the agent's initial credit window (the tools
            # this suite copies in are a few tens of KiB).
            for off in range(0, len(stdin_data), 32768):
                self.sock.sendall(agent_frame(1, cid, stdin_data[off : off + 32768]))
            self.sock.sendall(
                agent_frame(0, 0, json.dumps({"cmd": "eof", "id": cid}).encode())
            )
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
                    return self.channels.pop(cid)
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
    stream = None
    try:
        s.connect(path)
        s.settimeout(timeout)
        stream = s.makefile("rwb")

        def rd():
            line = stream.readline()
            if not line:
                raise BenchError("QMP connection closed before its reply")
            return json.loads(line)

        if "QMP" not in rd():
            raise BenchError("QMP greeting is missing")
        for request_id, command in enumerate(
            ("qmp_capabilities", "cont", "query-status"), start=1
        ):
            request = {"execute": command, "id": request_id}
            stream.write((json.dumps(request) + "\n").encode())
            stream.flush()
            while True:
                reply = rd()
                if reply.get("id") != request_id:
                    continue
                if "error" in reply:
                    raise BenchError(f"QMP {command}: {reply['error']}")
                if "return" not in reply:
                    raise BenchError(f"QMP {command}: reply has no result")
                if command == "query-status" and not reply["return"].get("running"):
                    raise BenchError(f"QMP guest is not running: {reply['return']}")
                break
    finally:
        if stream is not None:
            stream.close()
        s.close()


# ---------------------------------------------------------------------------
# vmlab-hv control client
# ---------------------------------------------------------------------------


class HvControlClient:
    """The vmlab-hv control socket: handshake, then one command per request.

    The protocol is client-speaks-first (vmlab-hv-proto server.rs): the
    client's `hello` is answered by the server's `hello` carrying its
    version and capabilities.
    """

    def __init__(self, path, timeout=60):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(timeout)
        self.f = self.sock.makefile("rwb")
        self.f.write(
            (json.dumps({"type": "hello", "proto_version": HV_PROTO_VERSION}) + "\n").encode()
        )
        self.f.flush()
        greeting = self._read()
        if greeting.get("type") == "error":
            raise BenchError(f"control handshake rejected: {greeting}")
        if greeting.get("type") != "hello":
            raise BenchError(f"unexpected control greeting: {greeting}")
        if greeting.get("proto_version") != HV_PROTO_VERSION:
            raise BenchError(f"control version mismatch: {greeting}")
        self.hv_version = greeting.get("hv_version", "?")
        self.next_id = 1

    def _read(self):
        line = self.f.readline()
        if not line:
            raise ConnectionError("control socket closed")
        return json.loads(line.decode())

    def request(self, command, **kwargs):
        # The Command enum carries its own serde tag, so it nests:
        # {"command": {"command": "status"}} (proto/tests/wire.rs).
        inner = {"command": command, **kwargs}
        msg = {"type": "request", "id": self.next_id, "command": inner}
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


def short_backend(backend):
    return {"qemu": "q", "vmlab-hv": "hv"}[backend]


def run_dir_name(backend, arch, guest, smp, index, suite="full"):
    """A run-directory basename short enough that `<run_dir>/<socket>`
    stays under the 108-byte sun_path limit on every path this repo is
    checked out at (worktrees have long roots)."""
    stamp = time.strftime("%H%M%S")
    if arch == "x86_64":
        guest = {"ubuntu-26.04": "u2604", "windows-10": "win10"}.get(guest, guest[:8])
        tag = "g2g" if suite == "g2g" else guest
        return f"{stamp}-{short_backend(backend)}-x86_{tag}-r{index}"
    return f"{stamp}-{short_backend(backend)}-{arch}-smp{smp}-r{index}"


def _version_key(name):
    return tuple(int(p) if p.isdigit() else p for p in name.split("."))


def template_disk(arch, guest, version):
    """The template's backing disk. The windows-10 template boots its
    sysprep-generalized `standard` variant when one is present — the disk
    vmlab itself picks (labd template resolution, acceptance::template)."""
    base = os.path.join(TEMPLATE_ROOT, arch, guest, version)
    disk = os.path.join(base, "disk.qcow2")
    if guest == "windows-10":
        standard = os.path.join(base, "variants", "standard", "disk.qcow2")
        if os.path.isfile(standard):
            return standard
    if not os.path.isfile(disk):
        raise BenchError(f"template disk missing: {disk}")
    return disk


def prepare_run_dir(run, arch, guest, version):
    """A fresh qcow2 overlay over the (never-written) template, plus vars."""
    template = template_disk(arch, guest, version)
    overlay = os.path.join(run, "overlay.qcow2")
    subprocess.run(
        ["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", template, overlay],
        check=True,
    )
    if arch == "x86_64":
        # SeaBIOS carries no VARS; an OVMF repeat gets a pristine copy of
        # vmlab's blank VARS template (the template ships none of its own).
        code, vars_path = (
            (SEABIOS, None)
            if X86_GUESTS[guest]["firmware"] == "seabios"
            else (OVMF_CODE, os.path.join(run, "vars.fd"))
        )
        if vars_path:
            shutil.copyfile(OVMF_VARS, vars_path)
        return overlay, code, vars_path
    code, vars_template = FIRMWARE[arch]
    shutil.copyfile(vars_template, os.path.join(run, "vars.fd"))
    return overlay, code, os.path.join(run, "vars.fd")


def host_apic_virtualization():
    """vmlab's cmdline::host_apic_virtualization: kvm_intel's enable_apicv
    or kvm_amd's avic. Decides the hv-avic enlightenment and QEMU's
    kvm-pit lost-tick global on this host."""
    for module, param in [("kvm_intel", "enable_apicv"), ("kvm_amd", "avic")]:
        try:
            with open(f"/sys/module/{module}/parameters/{param}") as f:
                if f.read().strip() in ("Y", "y", "1"):
                    return True
        except OSError:
            continue
    return False


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


def process_cpu_seconds(pid):
    """utime+stime of a whole process (all threads) in seconds, or None."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            stat = f.read()
        fields = stat[stat.rindex(")") + 2 :].split()
        # After comm: state(0) ... utime(11) stime(12), in clock ticks.
        ticks = int(fields[11]) + int(fields[12])
        return ticks / os.sysconf("SC_CLK_TCK")
    except (OSError, ValueError, IndexError):
        return None


# ---------------------------------------------------------------------------
# Statistics and history
# ---------------------------------------------------------------------------


def median_and_spread(samples, unit="s"):
    """Median plus max-min spread, the noise figure bench-check compares on."""
    med = statistics.median(samples)
    spread = max(samples) - min(samples)
    digits = 3 if unit in ("iops", "Mbit/s", "MiB/s") else 2
    return round(med, digits), round(spread, digits)

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
    """('ok' | 'better' | 'worse', allowed) for latest vs previous.

    `allowed` is in the workload's own unit: the larger of the two runs'
    spreads, or 5 % of the previous median. Records without a `direction`
    (the TCG suites' seconds) are lower-is-better; the phase-2 throughput
    workloads record direction "higher" and a *drop* beyond the noise is
    the regression.
    """
    allowed = max(
        previous.get("spread") or 0.0,
        latest.get("spread") or 0.0,
        NOISE_FLOOR * previous["median"],
    )
    delta = latest["median"] - previous["median"]
    if latest.get("direction", "lower") == "higher":
        delta = -delta
    if delta > allowed:
        return "worse", round(allowed, 3)
    if delta < -allowed:
        return "better", round(allowed, 3)
    return "ok", round(allowed, 3)

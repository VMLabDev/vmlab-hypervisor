#!/usr/bin/env python3
"""Run the benchmark suite: backend × arch × guest × N repeats → history.

One VM at a time, each under the systemd-run memory cap and a taskset pin,
release → agent ready → in-guest workloads, all timed host-side. After the
repeats of one configuration, one JSON line per workload is appended to
bench/history/<arch>.jsonl (format in bench/README.md).

Two suites: the phase-3 TCG suites (aarch64/riscv64, seconds) and the
phase-2 x86_64 KVM suite (HV-D28: boot to agent ready, virtio-blk and
virtio-net throughput, idle host CPU; `--suite g2g` runs the guest-to-guest
net pair). The x86_64 net workloads need the framing peer built from
bench/netpeer.c; it is compiled on demand.

Exit code: 0 on success and on a skipped backend (with the reason printed);
1 on any measurement failure.
"""

import argparse
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import time

import lib
import netpeer
from backends import HvBackend, QemuBackend, X86HvBackend, X86QemuBackend

X86_BOOT_DEADLINE = {"windows-10": 1800.0, "ubuntu-26.04": 300.0}
PREP_TIMEOUT = 3600
WORKLOAD_TIMEOUT = {"sha256_256M": 3600, "xz_9_32M": 7200}
VERIFY_TIMEOUT = 3600
IDLE_TIMEOUT = 600
SHUTDOWN_TIMEOUT = 300

# x86_64 ceilings and workload sizes (phase 2). A windows sysprep first
# boot runs several minutes; everything else is minutes at most.
X86_EXEC_TIMEOUT = 180
X86_SHUTDOWN_TIMEOUT = 300
X86_MAX_RESETS = 3
NET_WINDOW = 12.0        # seconds of sustained blasting per net direction
NET_SETTLE = 1.0
BLK_SEQ_MIB = 512        # sequential read/write volume
BLK_RAND_OPS = 20000     # 4-KiB O_DIRECT ops per random workload
BLK_RAND_SPAN_MIB = 1024 # preallocated file size for random writes
IDLE_WINDOW = 60.0       # idle host CPU sampling window


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--backend", choices=("qemu", "vmlab-hv", "interpreter", "interpreter-reference", "interpreter-specialized"), default="qemu")
    p.add_argument("--arch", default="all", help="all, or x86_64,aarch64,riscv64")
    p.add_argument("--vcpus", default=None,
                   help="comma-separated vCPU counts (default: 1,4 TCG, "
                        "the template's own for x86_64)")
    p.add_argument("--repeats", type=int, default=3, help="repeats per configuration")
    p.add_argument("--guest", default="debian-13", help="template name")
    p.add_argument("--suite", choices=("full", "g2g"), default="full",
                   help="x86_64: full single-VM set, or the guest-to-guest net pair")
    p.add_argument("--cpus", default=None, help="taskset window override (e.g. 16-19)")
    p.add_argument("--hv-bin", default=os.path.join(lib.REPO_DIR, "target/release/vmlab-hv"))
    p.add_argument("--timeout-scale", type=float, default=1.0)
    p.add_argument("--keep", action="store_true", help="keep per-run scratch dirs")
    p.add_argument("--dry-run", action="store_true", help="print the plan, boot nothing")
    return p.parse_args(argv)


def resolve_arches(spec):
    known = list(lib.FIRMWARE) + ["x86_64"]
    if spec == "all":
        return ["aarch64", "riscv64"]
    arches = [a.strip() for a in spec.split(",") if a.strip()]
    for a in arches:
        if a not in known:
            raise lib.BenchError(f"unknown arch {a!r} (want x86_64, aarch64 or riscv64)")
    return arches


def git_commit():
    sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=lib.REPO_DIR, capture_output=True, text=True
    ).stdout.strip()
    dirty = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=lib.REPO_DIR,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return sha, bool(dirty)


def log(msg):
    print(f"[{datetime.datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def make_backend(args, run_dir, arch, smp, pin, overlay, code_fd, vars_fd):
    if args.backend == "qemu":
        return QemuBackend(run_dir, arch, smp, pin, overlay, code_fd, vars_fd, log)
    return HvBackend(
        run_dir, arch, smp, pin, overlay, code_fd, vars_fd, log, args.hv_bin
    )


def one_repeat(args, arch, smp, pin, version, index):
    """One VM: launch → ready → workloads → clean shutdown. Returns samples."""
    run_dir = os.path.join(
        lib.RUN_DIR, lib.run_dir_name(args.backend, arch, args.guest, smp, index)
    )
    shutil.rmtree(run_dir, ignore_errors=True)
    os.makedirs(run_dir)
    overlay, code_fd, vars_fd = lib.prepare_run_dir(run_dir, arch, args.guest, version)
    backend = make_backend(args, run_dir, arch, smp, pin, overlay, code_fd, vars_fd)

    result = {"run": run_dir, "arch": arch, "vcpus": smp, "backend": args.backend}
    session = backend.launch()
    agent = None
    try:
        backend.release_cpus()
        result["t_release"] = round(session.elapsed(), 2)
        log(f"released CPUs after {result['t_release']}s; waiting for agent")

        deadline = BOOT_DEADLINE * args.timeout_scale
        agent, hello = lib.wait_for_agent(
            backend.agent_socket(), deadline, vm_exited=session.exit_code
        )
        result["boot_to_agent_ready"] = round(session.elapsed(), 2)
        result["agent_hello"] = {
            k: hello.get(k) for k in ("agent_version", "os", "features")
        }
        log(f"agent ready after {result['boot_to_agent_ready']}s")

        def timed(name, argv, timeout):
            t = time.monotonic()
            chan = agent.exec(argv, timeout, IDLE_TIMEOUT * args.timeout_scale)
            dt = time.monotonic() - t
            result[name] = {
                "secs": round(dt, 2),
                "code": chan["code"],
                "out": chan["data"].decode(errors="replace")[-400:],
                "err": chan["stderr"].decode(errors="replace")[-200:],
            }
            if chan["code"] != 0:
                raise lib.BenchError(
                    f"{name} exited {chan['code']}: "
                    f"{chan['stderr'].decode(errors='replace')[-400:]}"
                )
            log(f"  {name}: {dt:.2f}s")
            return dt

        # Fixed inputs, untimed (notes/03), then the two timed workloads.
        timed("prep", lib.PREP_ARGV, PREP_TIMEOUT * args.timeout_scale)
        sha_secs = timed(
            "sha256_256M", lib.WORKLOAD_ARGV["sha256_256M"],
            WORKLOAD_TIMEOUT["sha256_256M"] * args.timeout_scale,
        )
        xz_secs = timed(
            "xz_9_32M", lib.WORKLOAD_ARGV["xz_9_32M"],
            WORKLOAD_TIMEOUT["xz_9_32M"] * args.timeout_scale,
        )

        # Determinism gate: the inputs are fixed, so the digest must be too,
        # and the guest must really see `smp` CPUs.
        timed("verify", lib.VERIFY_ARGV, VERIFY_TIMEOUT * args.timeout_scale)
        out = result["verify"]["out"]
        if "XZ-OK" not in out:
            raise lib.BenchError(f"verify failed: {out!r}")
        nproc = int(out.strip().splitlines()[-2])
        if nproc != smp:
            raise lib.BenchError(f"guest sees {nproc} CPUs, expected {smp}")
        digest = result["sha256_256M"]["out"].split()[0]
        result["sha256_digest"] = digest

        result["samples"] = {
            "boot_to_agent_ready": result["boot_to_agent_ready"],
            "sha256_256M": round(sha_secs, 2),
            "xz_9_32M": round(xz_secs, 2),
        }
        return result, backend, session, agent, digest
    finally:
        # Always leave nothing running. The graceful paths are best effort:
        # a failure in them must not skip the kill.
        try:
            if agent is not None:
                agent.shutdown()
                agent.close()
            if args.backend == "vmlab-hv":
                backend.quit()
            code = session.wait_exit(SHUTDOWN_TIMEOUT)
            if code is None:
                log(f"VM did not exit after powerdown within {SHUTDOWN_TIMEOUT}s")
        except Exception as e:  # noqa: BLE001 - cleanup must reach the kill
            log(f"note: graceful shutdown path failed ({e}); killing")
        session.kill()
        result["vm_exit"] = session.exit_code()
        if result["vm_exit"] != 0:
            log(f"warning: VM exit code {result['vm_exit']}")
        # Bulky scratch: the overlay grew by everything the guest wrote; the
        # serial log and result.json stay.
        for name in ("overlay.qcow2", "vars.fd"):
            path = os.path.join(run_dir, name)
            if os.path.exists(path) and not args.keep:
                os.remove(path)
        with open(os.path.join(run_dir, "result.json"), "w") as f:
            json.dump(result, f, indent=1)


def run_config(args, arch, smp, pin, version, commit, dirty, host):
    """N repeats of one configuration → one history line per workload."""
    all_samples = {w: [] for w in lib.workloads(arch, args.guest, args.suite)}
    digests = set()
    env = {
        "governor": lib.read_governor(),
        "loadavg_start": lib.read_loadavg(),
        "pin": pin,
        "memory_max": lib.MEMORY_MAX,
        "template": version,
        "host_cpus": os.cpu_count(),
    }
    for i in range(1, args.repeats + 1):
        log(f"=== {args.backend} {arch} smp={smp} {args.guest} repeat {i}/{args.repeats}")
        result, backend, session, agent, digest = one_repeat(
            args, arch, smp, pin, version, i
        )
        env["loadavg_end"] = lib.read_loadavg()
        env["backend_version"] = backend.backend_version()
        for w in lib.workloads(arch, args.guest, args.suite):
            all_samples[w].append(result["samples"][w])
        digests.add(digest)
        log(f"repeat {i}: {result['samples']}")

    if len(digests) != 1:
        raise lib.BenchError(f"sha256 digest changed across repeats: {digests}")

    return finish_config(args, arch, smp, all_samples, env, commit, dirty, host)


def finish_config(args, arch, smp, all_samples, env, commit, dirty, host):
    """Median/spread per workload → one history line each."""
    recorded = datetime.datetime.now().isoformat(timespec="seconds")
    existing = lib.read_history(arch)
    appended = []
    for w, samples in all_samples.items():
        if not samples:
            continue
        unit = lib.UNIT[w]
        median, spread = lib.median_and_spread(samples, unit)
        record = {
            "recorded": recorded,
            "commit": commit,
            "dirty": dirty,
            "host": host,
            "backend": args.backend,
            "arch": arch,
            "guest": args.guest,
            "vcpus": smp,
            "workload": w,
            "unit": unit,
            "direction": lib.DIRECTION[w],
            "repeats": args.repeats,
            "samples": samples,
            "median": median,
            "spread": spread,
            "qemu_baseline": None,
            "env": env,
        }
        if args.backend == "vmlab-hv":
            record["qemu_baseline"] = lib.latest_baseline(
                existing, arch, args.guest, smp, w
            )
            if record["qemu_baseline"] is None:
                log(f"warning: no QEMU baseline in history for {arch} smp={smp} {w}")
        lib.append_history(arch, record)
        appended.append(record)
    return appended


# ---------------------------------------------------------------------------
# x86_64 KVM suite (phase 2, HV-D28)
# ---------------------------------------------------------------------------


def sh(agent, cmd, timeout=X86_EXEC_TIMEOUT, stdin_data=None):
    """One `/bin/sh -c` exec; returns (code, stdout, stderr)."""
    chan = agent.exec(["/bin/sh", "-c", cmd], timeout, timeout, stdin_data)
    return (
        chan["code"],
        chan["data"].decode(errors="replace"),
        chan["stderr"].decode(errors="replace"),
    )


def must_sh(agent, cmd, what, timeout=X86_EXEC_TIMEOUT, stdin_data=None):
    code, out, err = sh(agent, cmd, timeout, stdin_data)
    if code != 0:
        raise lib.BenchError(f"{what} exited {code}: {err[-400:]}")
    return out


def respawn_hv(backend, run_dir):
    """vmlab-hv exits 0 on a guest reset (triple fault); vmlab restarts the
    process, so the bench does too — same machine description, same disks."""
    for name in ("control.sock", "agent.sock", "display.sock", "vnc.sock"):
        path = os.path.join(run_dir, name)
        if os.path.exists(path):
            os.remove(path)
    try:
        backend.launch()
        backend.release_cpus()
    except Exception:
        session = getattr(backend, "session", None)
        if session is not None:
            session.kill()
        raise

WINDOWS_MARKER = "C:\\\\Windows\\\\Temp\\\\vmlab-firstboot.done"

# vmlab's user-OOBE gate (the template's first-boot script): the marker
# fires before the oobeSystem pass, so setup must be idle, autologon gone,
# and the last autologon line in setupact.log the clear, not msoobe's
# enable — rebooting earlier wedges the clone.
WINDOWS_OOBE_PS = (
    "$s = Get-ItemProperty HKLM:\\SYSTEM\\Setup; "
    "if ($s.SystemSetupInProgress -ne 0 -or $s.OOBEInProgress -ne 0) { 'setup'; exit }; "
    "if ((Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon')"
    ".SystemAutoLogon) { 'autologon'; exit }; "
    "$l = Select-String -Path C:\\Windows\\Panther\\UnattendGC\\setupact.log "
    "-Pattern 'fEnableAutologon = 1|Clearing Auto-logon values' -ErrorAction SilentlyContinue "
    "| Select-Object -Last 1; "
    "if ($l -and $l.Line -notmatch 'Clearing') { 'user-oobe'; exit }; 'done'"
)


def win_exec(agent, argv, timeout=90):
    return agent.exec(argv, timeout, timeout)


def win_ps(agent, script, timeout=90):
    return win_exec(
        agent,
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
        timeout,
    )


def windows_boot_id(agent):
    """vmlab's boot identity (labd/boot_id.rs): the PrefetchParameters
    BootId counter, falling back to LastBootUpTime."""
    probes = (
        ["reg", "query",
         "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\"
         "Memory Management\\PrefetchParameters", "/v", "BootId"],
        ["wmic", "os", "get", "LastBootUpTime", "/value"],
    )
    for argv in probes:
        try:
            chan = win_exec(agent, ["cmd.exe", "/C"] + argv, 60)
        except (TimeoutError, ConnectionError, lib.BenchError, OSError):
            return None
        out = chan["data"].decode(errors="replace").strip()
        if chan["code"] == 0 and out:
            return out
    return None


class WindowsReady:
    """The windows first-boot sequence, ported from the template's
    provision script (see wait_x86_ready)."""

    def __init__(self, backend, args, run_dir, t0):
        self.backend = backend
        self.args = args
        self.run_dir = run_dir
        self.t0 = t0
        self.deadline = t0 + X86_BOOT_DEADLINE["windows-10"] * args.timeout_scale
        self.resets = 0

    def ensure_vm(self):
        """vmlab-hv exits 0 on every guest reset; vmlab restarts it, and so
        does the bench (up to X86_MAX_RESETS — sysprep's internal reboots
        plus the provision reboot)."""
        if not self.backend.session.exited():
            return
        code = self.backend.session.exit_code()
        if code != 0:
            raise lib.BenchError(
                f"VM exited (code {code}) before agent ready: "
                f"{self.backend.session.stderr_text()[-1500:]}"
            )
        self.resets += 1
        if self.resets > X86_MAX_RESETS + 3:
            raise lib.BenchError("too many guest resets during first boot")
        log(f"guest reset {self.resets}: respawning vmlab-hv")
        respawn_hv(self.backend, self.run_dir)

    def connect(self):
        """The answered agent, or None. An early persistent connection's
        Hello is never answered, so every attempt is a fresh dial."""
        self.ensure_vm()
        cand = None
        try:
            cand = lib.AgentClient(self.backend.agent_socket())
            hello = cand.handshake(3)
            return cand, hello
        except Exception:
            if cand is not None:
                cand.close()
            return None, None

    def wait_marker(self):
        """Phase 1: the specialize marker, `cmd if exist` like vmlab's
        script (powershell is not dependable this early)."""
        while time.monotonic() < self.deadline:
            agent, hello = self.connect()
            if agent is not None:
                try:
                    chan = win_exec(
                        agent,
                        ["cmd.exe", "/C",
                         f"if exist {WINDOWS_MARKER} (exit 0) else (exit 1)"],
                        60,
                    )
                    if chan["code"] == 0:
                        log("windows first boot finished; clearing marker")
                        win_exec(agent, ["cmd.exe", "/C", f"del /f /q {WINDOWS_MARKER}"], 60)
                        return agent, hello
                except (TimeoutError, ConnectionError, lib.BenchError, OSError):
                    pass
                agent.close()
            time.sleep(5)
        raise TimeoutError("windows first-boot marker never appeared")

    def wait_oobe(self, agent):
        """Phase 2: user OOBE done (best-effort ~10 min, like the script:
        a stuck OOBE costs time, never the boot). The agent blips across
        the specialize reboots, so every iteration works with whatever
        connection it can get."""
        end = min(time.monotonic() + 600, self.deadline)
        while time.monotonic() < end:
            if agent is None:
                agent, _ = self.connect()
                if agent is None:
                    time.sleep(5)
                    continue
            try:
                chan = win_ps(agent, WINDOWS_OOBE_PS)
                state = chan["data"].decode(errors="replace").strip()
                if chan["code"] == 0 and state == "done":
                    log("windows OOBE finished")
                    return agent
            except (TimeoutError, ConnectionError, lib.BenchError, OSError):
                try:
                    agent.close()
                except Exception:
                    pass
                agent = None
            time.sleep(5)
        log("windows OOBE still running after ~10 minutes; continuing")
        return agent

    def settle_appx(self, agent):
        """Phase 3: consume the staged-AppX reconciliation (best-effort,
        like the script; the removal attempt itself settles it)."""
        ps = (
            "$prov = (Get-AppxProvisionedPackage -Online).DisplayName; "
            "Get-AppxPackage -AllUsers | Where-Object { -not $_.IsFramework "
            "-and -not $_.IsResourcePackage -and $prov -notcontains $_.Name "
            "-and -not ($_.PackageUserInformation | Where-Object InstallState "
            "-eq 'Installed') } | ForEach-Object { $r = 'removed'; "
            "try { Remove-AppxPackage -Package $_.PackageFullName -AllUsers "
            "-ErrorAction Stop } catch { $r = 'settled' }; "
            "Write-Output ($r + ' ' + $_.PackageFullName) }"
        )
        try:
            win_ps(agent, ps, 600)
        except (TimeoutError, ConnectionError, lib.BenchError, OSError) as e:
            log(f"windows appx settle skipped ({e})")

    def request_reboot(self, agent, round_index):
        line = "shutdown /r /t 0 /f"
        if round_index == 1:
            line = "shutdown /a & shutdown /r /t 0 /f /d p:4:1"
        argv = (["cmd.exe", "/C", line] if round_index < 2 else
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                 "Restart-Computer -Force"])
        try:
            chan = win_exec(agent, argv, 60)
            log(f"windows reboot request {round_index + 1}: exit {chan['code']} "
                f"{(chan['data'] + chan['stderr']).decode(errors='replace')[:80]!r}")
        except (TimeoutError, ConnectionError, lib.BenchError, OSError) as e:
            log(f"windows reboot request {round_index + 1} got no reply "
                f"(reboot likely underway): {e}")

    def reboot_and_return(self, agent):
        """Phase 4: one in-guest reboot, watched by boot id, escalating and
        finally host-restarting exactly like the script's reboot_guest."""
        before = windows_boot_id(agent)
        agent.close()
        for round_index in range(3):
            agent, _ = self.connect()
            if agent is None:
                time.sleep(5)
                continue
            self.request_reboot(agent, round_index)
            end = time.monotonic() + 60
            while time.monotonic() < end:
                agent, _ = self.connect()
                if agent is not None:
                    now_id = windows_boot_id(agent)
                    if before is None or (now_id is not None and now_id != before):
                        return self.wait_post_reboot_agent(agent)
                    agent.close()
                time.sleep(5)
            log(f"windows still on the same boot ~60 s after request "
                f"{round_index + 1}")
        log("windows in-guest reboot never took; forcing a host restart")
        if self.args.backend == "qemu":
            self.backend.session.kill()
            session = self.backend.launch()
            self.backend.release_cpus()
        else:
            self.backend.session.kill()
            respawn_hv(self.backend, self.run_dir)
        agent, hello = self.wait_agent(self.deadline)
        return agent, hello

    def wait_agent(self, deadline):
        while time.monotonic() < deadline:
            agent, hello = self.connect()
            if agent is not None:
                return agent, hello
            time.sleep(2)
        raise TimeoutError("windows agent never answered after the reboot")

    def wait_post_reboot_agent(self, agent):
        """The reboot is observed; the machine can spend minutes on the way
        up, so wait out the agent blip before declaring ready."""
        agent.close()
        return self.wait_agent(min(time.monotonic() + 900, self.deadline))

    def run(self):
        agent, hello = self.wait_marker()
        agent = self.wait_oobe(agent) or agent
        self.settle_appx(agent)
        agent, hello = self.reboot_and_return(agent)
        return agent, hello, round(time.monotonic() - self.t0, 2)


def wait_x86_ready(backend, args, guest, run_dir, t0):
    """Boot to agent ready, vmlab's readiness semantics.

    Linux: the agent answers its Hello and runs an exec. Windows: the
    template's whole first-boot sequence — the specialize marker, the user
    OOBE gate, the AppX settle, and one in-guest reboot watched by boot id —
    because the agent answers long before the machine is usable and the
    reboot must not land mid-Oobe. vmlab-hv exits 0 on a guest reset and is
    respawned; QEMU reboots in-process. The clock starts at the first spawn
    and never resets.
    """
    if guest == "windows-10":
        return WindowsReady(backend, args, run_dir, t0).run()
    deadline = t0 + X86_BOOT_DEADLINE[guest] * args.timeout_scale
    while time.monotonic() < deadline:
        if backend.session.exited():
            code = backend.session.exit_code()
            raise lib.BenchError(
                f"VM exited (code {code}) before agent ready: "
                f"{backend.session.stderr_text()[-1500:]}"
            )
        cand = None
        agent = None
        try:
            cand = lib.AgentClient(backend.agent_socket())
            hello = cand.handshake(3)
            agent = cand
        except Exception:
            if cand is not None:
                cand.close()
        if agent is None:
            time.sleep(1)
            continue
        out = must_sh(agent, "printf hv-agent-ready", "ready echo")
        if "hv-agent-ready" not in out:
            raise lib.BenchError(f"guest agent echo failed: {out!r}")
        return agent, hello, round(time.monotonic() - t0, 2)
    raise TimeoutError(f"{guest} never reached agent ready")


def guest_nic(agent):
    out = must_sh(agent, "ls /sys/class/net | grep -v '^lo$' | head -1", "NIC name")
    nic = out.strip().splitlines()[0]
    if not nic:
        raise lib.BenchError("no NIC visible in the guest")
    return nic


def copy_in_tools(agent, randblke_bin):
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "nettool.py"), "rb") as f:
        nettool_src = f.read()
    must_sh(agent, "cat > /tmp/vmlab-nettool.py", "nettool copy", stdin_data=nettool_src)
    import base64
    with open(randblke_bin, "rb") as f:
        b64 = base64.b64encode(f.read())
    must_sh(
        agent,
        "base64 -d > /tmp/randblke && chmod 755 /tmp/randblke",
        "randblke copy",
        stdin_data=b64,
    )


def allocated_span(template_disk):
    out = subprocess.run(
        ["qemu-img", "map", "--output=json", template_disk],
        capture_output=True, text=True, check=True,
    ).stdout
    end = 0
    for entry in json.loads(out):
        if entry.get("data") and not entry.get("zero", False):
            end = max(end, entry["start"] + entry["length"])
    return end


def x86_one_repeat(args, guest, smp, pin, version, index):
    """One x86_64 VM through the full workload set for its guest."""
    lib.require_vm_slots()
    loadavg_start = lib.read_loadavg()
    run_dir = os.path.join(
        lib.RUN_DIR, lib.run_dir_name(args.backend, "x86_64", guest, smp, index)
    )
    shutil.rmtree(run_dir, ignore_errors=True)
    os.makedirs(run_dir)
    overlay, code_fd, vars_fd = lib.prepare_run_dir(run_dir, "x86_64", guest, version)
    net = lib.BENCH_NET
    windows = guest == "windows-10"
    # The boot-only windows guest runs vmlab's no-network shape exactly
    # (the reference lab boots this template without a NIC; a jumbo-MTU
    # NIC on it wedges the first-boot reboot).
    peer = None if windows else netpeer.Peer(
        run_dir, net["guest_ip"], net["guest_mac"], net["port"], log)
    switch_sock = None if windows else peer.switch_sock
    mtu = None if windows else net["mtu"]
    if args.backend == "qemu":
        backend = X86QemuBackend(
            run_dir, guest, smp, pin, overlay, code_fd, vars_fd,
            switch_sock, net["guest_mac"], mtu, log,
        )
    else:
        backend = X86HvBackend(
            run_dir, guest, smp, pin, overlay, code_fd, vars_fd,
            switch_sock, net["guest_mac"], mtu, log,
            args.hv_bin, lib.IPXE_ROM,
        )
    result = {"run": run_dir, "arch": "x86_64", "guest": guest,
              "vcpus": smp, "backend": args.backend,
              "loadavg_start": loadavg_start}
    agent = None
    try:
        session = backend.launch()
        backend.release_cpus()
        result["t_release"] = round(session.elapsed(), 2)
        log(f"released CPUs after {result['t_release']}s; waiting for agent")

        agent, hello, boot_secs = wait_x86_ready(backend, args, guest, run_dir, session.t0)
        result["boot_to_agent_ready"] = boot_secs
        result["agent_hello"] = {k: hello.get(k) for k in ("agent_version", "os")}
        log(f"agent ready after {boot_secs}s")

        samples = {"boot_to_agent_ready": boot_secs}
        env_tool = {}

        if not windows:
            nic = guest_nic(agent)

            def configure_nic():
                must_sh(
                    agent,
                    f"ip addr replace {net['guest_ip']}/24 dev {nic} && "
                    f"ip link set dev {nic} mtu {net['mtu']} && ip link set {nic} up",
                    "NIC setup",
                )

            # cloud-init may still be reconfiguring interfaces right after
            # the agent answers; a static address added too early can be
            # flushed again. Let it finish, then prove the segment works
            # before anything is measured on it.
            sh(agent, "timeout 90 cloud-init status --wait >/dev/null 2>&1 || true")
            configure_nic()
            copy_in_tools(agent, netpeer.randblke_path())
            for attempt in range(1, 4):
                peer.report()  # reset
                out = must_sh(
                    agent,
                    f"python3 /tmp/vmlab-nettool.py udp-blast {net['peer_ip']} "
                    f"{net['port']} 4096 1",
                    f"connectivity probe {attempt}",
                )
                time.sleep(0.3)
                if peer.report()["rx_bytes"] > 0:
                    break
                configure_nic()
            else:
                raise lib.BenchError(
                    f"guest never delivered a probe frame to the peer on {nic}"
                )
            fio = sh(agent, "command -v fio")[1].strip()
            env_tool["blk_tool"] = "fio" if fio else "randblke"
            span = min(allocated_span(lib.template_disk("x86_64", guest, version)),
                       8 * 1024**3)
            span_mib = max(span // 1024**2, 64)
            seq_mib = min(BLK_SEQ_MIB, span_mib)

            def randblke(args_str, what):
                log(f"starting {what}")
                out = must_sh(agent, f"/tmp/randblke {args_str}", what,
                              timeout=1800)
                log(f"finished {what}: {out.strip()}")
                return json.loads(out.strip().splitlines()[-1])


            # --- virtio-blk ------------------------------------------------
            # All direct I/O: reads against the template-backed overlay's
            # allocated extent, writes against a fresh file on it. (GNU dd's
            # iflag=direct buffer is refused by the virtio-blk device, so the
            # one tool covers every shape.)
            j = randblke(f"seqread /dev/vda 1048576 0 {span_mib * 1024 * 1024}",
                         "seq read")
            samples["blk_seq_read_mbps"] = round(j["bytes"] / 1048576 / j["secs"], 2)

            j = randblke(f"seqwrite /var/tmp/bench-seq.img {BLK_SEQ_MIB * 1048576}",
                         "seq write")
            samples["blk_seq_write_mbps"] = round(j["bytes"] / 1048576 / j["secs"], 2)
            sh(agent, "rm -f /var/tmp/bench-seq.img")

            j = randblke(f"randread /dev/vda {BLK_RAND_OPS} 42 0 {span}", "rand read")
            samples["blk_rand_read_4k_iops"] = round(j["iops"], 1)

            must_sh(agent, f"fallocate -l {BLK_RAND_SPAN_MIB}M /var/tmp/bench-rand.img",
                    "rand write prep")
            j = randblke(f"randwrite /var/tmp/bench-rand.img {BLK_RAND_OPS} 7",
                         "rand write")
            samples["blk_rand_write_4k_iops"] = round(j["iops"], 1)
            sh(agent, "rm -f /var/tmp/bench-rand.img")


            # --- virtio-net ------------------------------------------------
            # TX: the guest blasts UDP at the peer; the peer counts the
            # ethernet bytes that crossed the whole path (device, backend
            # netdev, framing socket). A delivery below 90% of what the
            # guest sent means the measurement is broken, not slow.
            peer.report()  # reset counters
            log("starting net tx")
            out = must_sh(
                agent,
                f"python3 /tmp/vmlab-nettool.py udp-blast {net['peer_ip']} {net['port']} "
                f"{net['udp_payload']} {NET_WINDOW}",
                "net tx", timeout=NET_WINDOW * 3,
            )
            blast = json.loads(out.strip().splitlines()[-1])
            time.sleep(NET_SETTLE)
            rx = peer.report()
            if blast["bytes"] == 0:
                raise lib.BenchError(f"net tx blasted nothing: {blast}")
            delivery = rx["rx_bytes"] / blast["bytes"]
            if delivery < 0.9:
                raise lib.BenchError(
                    f"net tx delivery {delivery:.0%}: guest sent {blast['bytes']}, "
                    f"peer saw {rx['rx_bytes']} ({blast.get('last_error')})"
                )
            samples["net_tx_udp_mbps"] = round(
                rx["rx_bytes"] * 8 / NET_WINDOW / 1e6, 2)
            log(f"finished net tx: {samples['net_tx_udp_mbps']} Mbit/s")

            # RX: the peer blasts; the rate is read from the guest's own
            # interface counter (kernel-counted, immune to userspace sink
            # starvation under the flood). The sink only keeps the port
            # open so the kernel answers nothing back.
            def nic_rx_bytes():
                out = must_sh(agent, f"cat /sys/class/net/{nic}/statistics/rx_bytes",
                              "rx counter read")
                return int(out.strip().splitlines()[-1])

            must_sh(
                agent,
                f"rm -f /tmp/sink.json && nohup python3 /tmp/vmlab-nettool.py "
                f"udp-sink {net['port']} {NET_WINDOW + 8} > /tmp/sink.json 2>&1 &",
                "net rx sink start",
            )
            time.sleep(NET_SETTLE)
            log("starting net rx")
            rx0 = nic_rx_bytes()
            peer.blast(NET_WINDOW, net["udp_payload"])
            # The ack returns at the blast's start; the window is timed
            # guest-side between the two counter reads.
            rx_mid = None
            deadline = time.monotonic() + NET_WINDOW + 30
            while time.monotonic() < deadline:
                now_rx = nic_rx_bytes()
                if rx_mid is not None and now_rx == rx_mid:
                    break  # counter stopped moving: the flood has drained
                rx_mid = now_rx
                time.sleep(2)
            rx1 = rx_mid if rx_mid is not None else nic_rx_bytes()
            # Everything the counter gained belongs to this blast's window
            # (the peer stops writing at NET_WINDOW; the tail is in-flight
            # drain), so the rate is the whole delta over the window.
            samples["net_rx_udp_mbps"] = round(
                (rx1 - rx0) * 8 / NET_WINDOW / 1e6, 2)
            log(f"finished net rx: {samples['net_rx_udp_mbps']} Mbit/s")
            sh(agent, "pkill -f vmlab-nettool || true")

            # --- idle host CPU ---------------------------------------------
            must_sh(agent, "sync", "idle sync")
            time.sleep(3)
            pid = backend.session.proc.pid
            c0 = lib.process_cpu_seconds(pid)
            time.sleep(IDLE_WINDOW)
            c1 = lib.process_cpu_seconds(pid)
            if c0 is None or c1 is None:
                raise lib.BenchError("VM process vanished during idle sampling")
            samples["idle_cpu_pct"] = round((c1 - c0) / IDLE_WINDOW * 100, 2)
            log(f"idle host CPU over {IDLE_WINDOW:.0f}s: {samples['idle_cpu_pct']}%")

        result["samples"] = samples
        result["tools"] = env_tool
        if peer is not None:
            result["peer"] = peer.report()
        return result, backend, agent
    finally:
        session = getattr(backend, "session", None)
        try:
            if agent is not None:
                agent.shutdown()
                agent.close()
            if args.backend == "vmlab-hv":
                backend.quit()
            if session is not None:
                code = session.wait_exit(X86_SHUTDOWN_TIMEOUT)
                if code is None:
                    log(f"VM did not exit within {X86_SHUTDOWN_TIMEOUT}s")
        except Exception as e:  # noqa: BLE001 - cleanup must reach the kills
            log(f"note: graceful shutdown failed ({e}); killing")
        if session is not None:
            session.kill()
            result["vm_exit"] = session.exit_code()
            if result["vm_exit"] != 0:
                log(f"warning: VM exit code {result['vm_exit']}")
        if peer is not None:
            peer.kill()
        for name in ("overlay.qcow2", "vars.fd"):
            path = os.path.join(run_dir, name)
            if os.path.exists(path) and not args.keep:
                os.remove(path)
        with open(os.path.join(run_dir, "result.json"), "w") as f:
            result["loadavg_end"] = lib.read_loadavg()
            json.dump(result, f, indent=1)


def x86_run_config(args, guest, smp, pin, version, commit, dirty, host):
    all_samples = {w: [] for w in lib.workloads("x86_64", guest, args.suite)}
    env = {
        "governor": lib.read_governor(),
        "loadavg_start": lib.read_loadavg(),
        "pin": pin,
        "memory_max": lib.MEMORY_MAX,
        "template": version,
        "host_cpus": os.cpu_count(),
        "apic_virtualization": lib.host_apic_virtualization(),
        "nic_mtu": lib.BENCH_NET["mtu"],
        "net_peer": "bench/netpeer.c framing peer",
        "tools": {},
        "repeat_loadavg": [],
    }
    for i in range(1, args.repeats + 1):
        log(f"=== {args.backend} x86_64 {guest} smp={smp} repeat {i}/{args.repeats}")
        result, backend, agent = x86_one_repeat(args, guest, smp, pin, version, i)
        env["backend_version"] = backend.backend_version()
        env["tools"] = result.get("tools", {})
        env["repeat_loadavg"].append({
            "start": result["loadavg_start"], "end": lib.read_loadavg()})
        for w, value in result["samples"].items():
            all_samples.setdefault(w, []).append(value)
        log(f"repeat {i}: {result['samples']}")
    env["loadavg_end"] = lib.read_loadavg()
    return finish_config(args, "x86_64", smp, all_samples, env, commit, dirty, host)


# --- guest-to-guest net pair (two VMs, one two-port hub) -------------------


def g2g_launch(args, tag, guest, smp, pin, hub_sock, mac, run_dir):
    overlay, code_fd, vars_fd = lib.prepare_run_dir(run_dir, "x86_64", guest,
                                                    lib.latest_template("x86_64", guest))
    if args.backend == "qemu":
        backend = X86QemuBackend(
            run_dir, guest, smp, pin, overlay, code_fd, vars_fd,
            hub_sock, mac, lib.BENCH_NET["mtu"], log,
        )
    else:
        backend = X86HvBackend(
            run_dir, guest, smp, pin, overlay, code_fd, vars_fd,
            hub_sock, mac, lib.BENCH_NET["mtu"], log,
            args.hv_bin, lib.IPXE_ROM,
        )
    backend.launch()
    backend.release_cpus()
    return backend


def g2g_teardown(backends, agents):
    for agent in agents:
        if agent is None:
            continue
        try:
            agent.shutdown()
            agent.close()
        except Exception:
            pass
    for backend in backends:
        if backend is None:
            continue
        try:
            if args_backend_quit(backend):
                backend.quit()
        except Exception:
            pass
        backend.session.kill()


def args_backend_quit(backend):
    return backend.name == "vmlab-hv"


def g2g_setup_guest(agent, ip, mac_peer=None):
    nic = guest_nic(agent)
    must_sh(
        agent,
        f"ip link set dev {nic} mtu {lib.BENCH_NET['mtu']} && "
        f"ip addr add {ip}/24 dev {nic} && ip link set {nic} up",
        "g2g NIC setup",
    )
    copy_in_tools(agent, netpeer.randblke_path())


def g2g_direction(sender, receiver, receiver_ip, port, window):
    """One TCP transfer: receiver listens guest-side, sender blasts."""
    must_sh(
        receiver,
        f"rm -f /tmp/sink.json && nohup python3 /tmp/vmlab-nettool.py tcp-sink {port} "
        f"> /tmp/sink.json 2>&1 &",
        "g2g sink start",
    )
    time.sleep(1)
    out = must_sh(
        sender,
        f"python3 /tmp/vmlab-nettool.py tcp-blast {receiver_ip} {port} {window}",
        "g2g blast", timeout=window * 4,
    )
    blast = json.loads(out.strip().splitlines()[-1])
    time.sleep(1)
    sink_out = must_sh(receiver, "cat /tmp/sink.json", "g2g sink read")
    sink = json.loads(sink_out.strip().splitlines()[-1])
    mbps = round(sink["bytes"] * 8 / window / 1e6, 2)
    return mbps, blast, sink


def g2g_one_repeat(args, guest, smp, pin_a, pin_b, version, index):
    lib.require_vm_slots(2)
    root = os.path.join(
        lib.RUN_DIR, lib.run_dir_name(args.backend, "x86_64", guest, smp, index, "g2g")
    )
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    hub = netpeer.Hub(root)
    net = lib.BENCH_NET
    a_dir = os.path.join(root, "a")
    b_dir = os.path.join(root, "b")
    os.makedirs(a_dir)
    os.makedirs(b_dir)
    a = b = None
    agent_a = agent_b = None
    result = {"run": root, "arch": "x86_64", "guest": guest, "vcpus": smp,
              "backend": args.backend, "suite": "g2g"}
    try:
        a = g2g_launch(args, "a", guest, smp, pin_a, hub.switch_sock, net["g2g_a_mac"], a_dir)
        b = g2g_launch(args, "b", guest, smp, pin_b, hub.switch_sock, net["g2g_b_mac"], b_dir)
        agent_a, _hello_a = lib.wait_for_agent(a.agent_socket(), 300.0, vm_exited=a.session.exit_code)
        agent_b, _hello_b = lib.wait_for_agent(b.agent_socket(), 300.0, vm_exited=b.session.exit_code)
        g2g_setup_guest(agent_a, net["g2g_a_ip"])
        g2g_setup_guest(agent_b, net["g2g_b_ip"])

        port = 5903
        a2b, blast, sink = g2g_direction(agent_b, agent_a, net["g2g_a_ip"], port, NET_WINDOW)
        log(f"g2g b->a: {a2b} Mbit/s (blast {blast['bytes']}B, sink {sink['bytes']}B)")
        b2a, blast, sink = g2g_direction(agent_a, agent_b, net["g2g_b_ip"], port, NET_WINDOW)
        log(f"g2g a->b: {b2a} Mbit/s (blast {blast['bytes']}B, sink {sink['bytes']}B)")
        result["samples"] = {
            "net_g2g_a2b_tcp_mbps": a2b,
            "net_g2g_b2a_tcp_mbps": b2a,
        }
        # b sent to a first: a2b is the a-side sink rate.
        return result, a
    finally:
        g2g_teardown([a, b], [agent_a, agent_b])
        hub.kill()
        if not args.keep:
            shutil.rmtree(root, ignore_errors=True)


def g2g_run_config(args, guest, smp, pin_a, pin_b, version, commit, dirty, host):
    all_samples = {w: [] for w in lib.X86_G2G_WORKLOADS}
    env = {
        "governor": lib.read_governor(),
        "loadavg_start": lib.read_loadavg(),
        "pin": f"{pin_a}+{pin_b}",
        "memory_max": lib.MEMORY_MAX,
        "template": version,
        "host_cpus": os.cpu_count(),
        "nic_mtu": lib.BENCH_NET["mtu"],
        "net_peer": "bench/netpeer.c hub (2 ports)",
        "repeat_loadavg": [],
    }
    for i in range(1, args.repeats + 1):
        log(f"=== {args.backend} x86_64 g2g {guest} repeat {i}/{args.repeats}")
        loadavg_start = lib.read_loadavg()
        result, a = g2g_one_repeat(args, guest, smp, pin_a, pin_b, version, i)
        env["repeat_loadavg"].append({
            "start": loadavg_start, "end": lib.read_loadavg()})
        env["backend_version"] = a.backend_version()
        for w, value in result["samples"].items():
            all_samples[w].append(value)
        log(f"repeat {i}: {result['samples']}")
    env["loadavg_end"] = lib.read_loadavg()
    return finish_config(args, "x86_64", smp, all_samples, env, commit, dirty, host)


# ---------------------------------------------------------------------------


def sweep_stale_vms():
    """Kill leftover VMs of this suite's own runs (their argv names a bench
    run path that exists nowhere else). A driver killed mid-run — a timeout,
    a Ctrl-C — can leave its systemd-run scope children alive, squatting on
    the pin window and poisoning every later measurement."""
    subprocess.run(
        ["pkill", "-TERM", "-f", f"{lib.RUN_DIR}/"],
        check=False, stdout=subprocess.DEVNULL)
    time.sleep(1)
    subprocess.run(
        ["pkill", "-KILL", "-f", f"{lib.RUN_DIR}/"],
        check=False, stdout=subprocess.DEVNULL)


def main(argv=None):
    args = parse_args(argv)
    if args.backend in ("interpreter", "interpreter-reference", "interpreter-specialized"):
        import micro
        commit, dirty = git_commit()
        return micro.run(args, commit, dirty, os.uname().nodename, log)
    if args.repeats < 2:
        log("note: --repeats < 2 leaves no spread; bench-check treats that as zero noise")
    arches = resolve_arches(args.arch)
    if args.suite == "g2g" and arches != ["x86_64"]:
        raise lib.BenchError("--suite g2g is an x86_64 suite")
    commit, dirty = git_commit()
    host = os.uname().nodename
    if "x86_64" in arches:
        netpeer.compile_tools(log)
        sweep_stale_vms()

    for arch in arches:
        if arch == "x86_64":
            guests = [args.guest] if args.guest != "debian-13" else ["ubuntu-26.04"]
            for guest in guests:
                if guest not in lib.X86_GUESTS:
                    raise lib.BenchError(f"unknown x86_64 guest {guest!r}")
                version = lib.latest_template(arch, guest)
                profile_smp = lib.X86_GUESTS[guest]["vcpus"]
                smp_list = ([int(v) for v in args.vcpus.split(",") if v.strip()]
                            if args.vcpus else [profile_smp])
                for smp in smp_list:
                    if args.suite == "g2g":
                        pin_a, pin_b = "16-19", "20-23"
                        if args.cpus:
                            pin_a, pin_b = args.cpus, args.cpus
                        log(f"### {args.backend} x86_64 g2g {guest}@{version} "
                            f"pins={pin_a}+{pin_b} governor={lib.read_governor()}")
                        if args.dry_run:
                            continue
                        records = g2g_run_config(args, guest, smp, pin_a, pin_b,
                                                 version, commit, dirty, host)
                    else:
                        pin = lib.pin_cpus(smp, args.cpus)
                        log(f"### {args.backend} x86_64 {guest} smp={smp} {guest}@{version} "
                            f"pin={pin} governor={lib.read_governor()} "
                            f"load={lib.read_loadavg()}")
                        if args.dry_run:
                            continue
                        records = x86_run_config(args, guest, smp, pin, version,
                                                 commit, dirty, host)
                    for r in records:
                        log(f"history: {r['workload']} median={r['median']}{r['unit']} "
                            f"spread={r['spread']}")
            continue

        version = lib.latest_template(arch, args.guest)
        vcpu_spec = args.vcpus or "1,4"
        for smp in [int(v) for v in vcpu_spec.split(",") if v.strip()]:
            pin = lib.pin_cpus(smp, args.cpus)
            log(f"### {args.backend} {arch} smp={smp} {args.guest}@{version} pin={pin} "
                f"governor={lib.read_governor()} load={lib.read_loadavg()}")
            if args.dry_run:
                continue
            try:
                records = run_config(args, arch, smp, pin, version, commit, dirty, host)
                for r in records:
                    log(f"history: {r['workload']} median={r['median']}s "
                        f"spread={r['spread']}s")
            except lib.BackendUnsupported as skip:
                log(f"SKIP {skip.backend} {arch} smp={smp}: {skip.reason}")
                log("(no history recorded for this configuration)")
    if args.dry_run:
        return 0
    log("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())

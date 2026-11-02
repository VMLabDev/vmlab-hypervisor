#!/usr/bin/env python3
"""Measure G4, G7, G9 and G10 only, interleaved across three fresh repeats.

Build vmlab-hv, iPXE, netpeer and randblke before the window. --preflight
checks these prerequisites without starting a VM. --dry-run prints the plan.
The window runs no builds, boot bars, read/random/RX/idle workloads or CI.
Results and logs go under bench/run unless --output names a new JSON file.
Exit 0 means the measurements finished; strict bar verdicts are in the JSON.
"""

import argparse
import contextlib
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import sys
import tempfile
import threading
import time

import backends
import lib
import netpeer
import run

REPEATS = 3
GUEST = "ubuntu-26.04"
SINGLE_PIN = "16-23"
PAIR_PINS = ("16-19", "20-23")
WORKLOADS = ("blk_seq_write_mbps", "net_tx_udp_mbps",
             "net_g2g_a2b_tcp_mbps", "net_g2g_b2a_tcp_mbps")


def plan():
    return [{"suite": suite, "repeat": repeat, "backend": backend}
            for suite in ("single", "g2g") for repeat in range(1, REPEATS + 1)
            for backend in (("qemu", "vmlab-hv") if repeat % 2 else ("vmlab-hv", "qemu"))]


def preflight(hv_bin):
    name = Path(hv_bin).resolve().name
    if name != "vmlab-hv" and not name.startswith("vmlab-hv-test-"):
        raise lib.BenchError("keep the binary basename vmlab-hv or vmlab-hv-test-* so host VM counting sees it")
    for command in ("systemd-run", "taskset", "qemu-img", "qemu-system-x86_64"):
        if shutil.which(command) is None:
            raise lib.BenchError(f"missing {command}; prepare dependencies before the window")
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise lib.BenchError("/dev/kvm is not readable and writable")
    version = lib.latest_template("x86_64", GUEST)
    assets = {"hv": str(Path(hv_bin).resolve()), "ipxe": lib.IPXE_ROM,
              "seabios": lib.SEABIOS, "netpeer": netpeer.NETPEER_BIN,
              "randblke": netpeer.randblke_path()}
    hashes = {}
    for name, filename in assets.items():
        path = Path(filename)
        if not path.is_file():
            raise lib.BenchError(f"missing {name}: {path}; build it before the window")
        hashes[name] = {"path": str(path.resolve()), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
    template = Path(lib.template_disk("x86_64", GUEST, version))
    stat = template.stat()
    if not set(range(16, 24)).issubset(os.sched_getaffinity(0)):
        raise lib.BenchError("the established CPU pins 16-23 are unavailable")
    return {"template": version, "assets": hashes,
            "template_image": {"path": str(template), "size": stat.st_size, "mtime_ns": stat.st_mtime_ns},
            "governor": lib.read_governor(), "host_cpus": os.cpu_count(),
            "apic_virtualization": lib.host_apic_virtualization(), "memory_max": lib.MEMORY_MAX,
            "memory_swap_max": "0", "nic_mtu": lib.BENCH_NET["mtu"]}


@contextlib.contextmanager
def admission(slot_directory=None):
    handles = []
    try:
        directory = slot_directory or Path(f"/tmp/vmlab-hv-vm-slots-{os.getuid()}")
        directory.mkdir(exist_ok=True)
        for slot in range(4):
            handle = (directory / f"{slot}.lock").open("a+")
            handles.append(handle)
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise lib.BenchError("shared VM slots are busy; the quiet window has not started") from error
        if lib.project_vm_pids():
            raise lib.BenchError("project VMs are already running; Android is excluded")
        yield
    finally:
        for handle in handles:
            handle.close()


class Monitor:
    def __init__(self, temporary_root):
        self.root = str(temporary_root)
        self.samples = []
        self.errors = []
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.watch, name="phase2-window-monitor")

    def watch(self):
        while not self.stop.is_set():
            try:
                self.capture()
            except Exception as error:
                self.errors.append(f"monitor failed: {error}")
                return
            self.stop.wait(1)

    def capture(self):
        sample = {"time": time.time(), "load": lib.read_loadavg(), "vms": []}
        for pid in lib.project_vm_pids():
            try:
                process = Path(f"/proc/{pid}")
                argv = process.joinpath("cmdline").read_bytes().split(b"\0")
                if not any(self.root.encode() in arg for arg in argv):
                    self.errors.append(f"foreign project VM {pid} appeared")
                group = process.joinpath("cgroup").read_text().strip().split("::")[-1]
                cgroup = Path("/sys/fs/cgroup") / group.lstrip("/")
                memory = cgroup.joinpath("memory.max").read_text().strip()
                swap = cgroup.joinpath("memory.swap.max").read_text().strip()
                if memory != "25769803776" or swap != "0":
                    self.errors.append(f"VM {pid} has memory.max={memory}, memory.swap.max={swap}")
                sample["vms"].append({"pid": pid, "memory_max": memory, "swap_max": swap})
            except FileNotFoundError:
                continue
            except OSError as error:
                self.errors.append(f"monitor VM {pid}: {error}")
        if len(sample["vms"]) > 4:
            self.errors.append("host-wide four-VM cap exceeded")
        self.samples.append(sample)

    def check(self):
        if self.errors:
            raise lib.BenchError("; ".join(dict.fromkeys(self.errors)))

    def close(self):
        self.stop.set()
        self.thread.join()


@contextlib.contextmanager
def vm(args, directory, pin, switch, mac, version):
    directory.mkdir()
    overlay, code, variables = lib.prepare_run_dir(str(directory), "x86_64", GUEST, version)
    common = (str(directory), GUEST, 2, pin, overlay, code, variables,
              switch, mac, lib.BENCH_NET["mtu"], run.log)
    backend = (backends.X86QemuBackend(*common) if args.backend == "qemu" else
               backends.X86HvBackend(*common, args.hv_bin, lib.IPXE_ROM))
    try:
        lib.require_vm_slots()
        backend.launch()
        backend.release_cpus()
        yield backend
    finally:
        session = getattr(backend, "session", None)
        if session is not None:
            if args.backend == "vmlab-hv":
                try:
                    backend.quit()
                    session.wait_exit(5)
                except Exception as error:
                    run.log(f"quit failed; reaping owned VM: {error}")
            session.kill()
            expected = (0, -signal.SIGTERM) if args.backend == "qemu" else (0,)
            if session.exit_code() not in expected:
                prior = sys.exc_info()[1]
                raise lib.BenchError(f"unexpected VM exit {session.exit_code()}, prior error={prior}: "
                                     f"{session.stderr_text()[-2000:]}") from prior


@contextlib.contextmanager
def ready(backend, args, directory):
    agent = None
    try:
        agent, _, _ = run.wait_x86_ready(backend, args, GUEST, str(directory), backend.session.t0)
        run.must_sh(agent, "timeout 90 cloud-init status --wait >/dev/null 2>&1 || true", "cloud-init wait")
        yield agent
    finally:
        if agent is not None:
            agent.close()


def single(args, directory, version):
    directory.mkdir()
    net = lib.BENCH_NET
    peer = netpeer.Peer(str(directory), net["guest_ip"], net["guest_mac"], net["port"], run.log)
    try:
        with vm(args, directory / "guest", SINGLE_PIN, peer.switch_sock, net["guest_mac"], version) as backend:
            with ready(backend, args, directory) as agent:
                nic = run.guest_nic(agent)
                run.must_sh(agent, f"ip addr replace {net['guest_ip']}/24 dev {nic} && "
                            f"ip link set dev {nic} mtu {net['mtu']} && ip link set {nic} up", "NIC setup")
                run.copy_in_tools(agent, netpeer.randblke_path())
                output = run.must_sh(agent,
                    f"/tmp/randblke seqwrite /var/tmp/bench-seq.img {run.BLK_SEQ_MIB * 1048576}",
                    "sequential write", timeout=1800)
                block = json.loads(output.strip().splitlines()[-1])
                if block["bytes"] != run.BLK_SEQ_MIB * 1048576 or block["secs"] <= 0:
                    raise lib.BenchError(f"invalid sequential write result: {block}")
                run.must_sh(agent, "rm /var/tmp/bench-seq.img", "remove measured file")
                peer.report()
                output = run.must_sh(agent,
                    f"python3 /tmp/vmlab-nettool.py udp-blast {net['peer_ip']} {net['port']} "
                    f"{net['udp_payload']} {run.NET_WINDOW}", "UDP TX", timeout=run.NET_WINDOW * 3)
                sent = json.loads(output.strip().splitlines()[-1])
                time.sleep(run.NET_SETTLE)
                received = peer.report()
                if not sent["bytes"] or received["rx_bytes"] / sent["bytes"] < 0.9:
                    raise lib.BenchError(f"UDP delivery below 90%: sent={sent}, received={received}")
                return {"samples": {
                    "blk_seq_write_mbps": round(block["bytes"] / 1048576 / block["secs"], 2),
                    "net_tx_udp_mbps": round(received["rx_bytes"] * 8 / run.NET_WINDOW / 1e6, 2)},
                    "block": block, "udp_sender": sent, "udp_receiver": received,
                    "backend_version": backend.backend_version()}
    finally:
        peer.kill()


def pair(args, directory, version):
    directory.mkdir()
    hub = netpeer.Hub(str(directory))
    net = lib.BENCH_NET
    try:
        with contextlib.ExitStack() as stack:
            machines = [stack.enter_context(vm(args, directory / tag, pin, hub.switch_sock,
                net[f"g2g_{tag}_mac"], version)) for tag, pin in zip(("a", "b"), PAIR_PINS)]
            agents = [stack.enter_context(ready(backend, args, directory)) for backend in machines]
            for agent, tag in zip(agents, ("a", "b")):
                run.g2g_setup_guest(agent, net[f"g2g_{tag}_ip"])
            samples, transfers = {}, {}
            for source, target, direction, ip in ((1, 0, "b2a", net["g2g_a_ip"]),
                                                   (0, 1, "a2b", net["g2g_b_ip"])):
                rate, sent, received = run.g2g_direction(agents[source], agents[target], ip, 5903, run.NET_WINDOW)
                if not sent["bytes"] or sent["bytes"] != received["bytes"] or rate <= 0:
                    raise lib.BenchError(f"TCP {direction} byte mismatch: sent={sent}, received={received}")
                samples[f"net_g2g_{direction}_tcp_mbps"] = rate
                transfers[direction] = {"sender": sent, "receiver": received}
            return {"samples": samples, "transfers": transfers, "backend_version": machines[0].backend_version()}
    finally:
        hub.kill()


def retain_logs(directory, destination):
    for path in directory.rglob("*"):
        if path.is_file() and path.suffix in (".json", ".log"):
            target = destination / path.relative_to(directory)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)


def aggregate(results, environment, commit, dirty):
    records = []
    prepared = []
    for backend in ("qemu", "vmlab-hv"):
        selected = [result for result in results if result["backend"] == backend]
        samples = {workload: [r["samples"][workload] for r in selected if workload in r["samples"]]
                   for workload in WORKLOADS}
        if any(len(values) != REPEATS for values in samples.values()):
            raise lib.BenchError("incomplete target workload samples; history is unchanged")
        args = argparse.Namespace(backend=backend, guest=GUEST, repeats=REPEATS)
        for suite, keys, pin, peer in (("single", WORKLOADS[:2], SINGLE_PIN, "bench/netpeer.c framing peer"),
                                      ("g2g", WORKLOADS[2:], "+".join(PAIR_PINS), "bench/netpeer.c hub (2 ports)")):
            repeats = [result for result in selected if result["suite"] == suite]
            env = dict(environment, repeat_loadavg=[{"start": r["load_start"], "end": r["load_end"]}
                for r in repeats], backend_version=repeats[0]["backend_version"], pin=pin, net_peer=peer)
            prepared.append((args, {key: samples[key] for key in keys}, env))
    for args, samples, env in prepared:
        records.extend(run.finish_config(args, "x86_64", 2, samples, env, commit, dirty, socket.gethostname()))
    return records


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hv-bin", default=str(Path(lib.REPO_DIR) / "target/release/vmlab-hv"))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    args.timeout_scale = 1.0
    if args.dry_run:
        print(json.dumps({"workloads": WORKLOADS, "plan": plan(), "expected_minutes": "6-8"}, indent=2))
        return 0
    try:
        environment = preflight(args.hv_bin)
        args.hv_bin = environment["assets"]["hv"]["path"]
        if args.preflight:
            print(json.dumps(environment, indent=2))
            return 0
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
        logs = Path(lib.RUN_DIR) / f"phase2-window-{stamp}"
        output = args.output or logs / "result.json"
        if output.exists():
            raise lib.BenchError(f"output already exists: {output}")
        commit, dirty = run.git_commit()
        report = {"scope": list(WORKLOADS), "commit": commit, "dirty": dirty, "environment": environment,
                  "started": time.time(), "results": [], "complete": False, "logs": str(logs)}
        logs.mkdir(parents=True)
        monitor = None
        try:
            with admission(), tempfile.TemporaryDirectory(prefix="hv-p2-window-", dir="/tmp") as temporary:
                monitor = Monitor(temporary)
                monitor.thread.start()
                try:
                    for item in plan():
                        monitor.check()
                        args.backend = item["backend"]
                        directory = Path(temporary) / f"{item['suite']}-{item['backend']}-{item['repeat']}"
                        result = dict(item, load_start=lib.read_loadavg(), started=time.time())
                        try:
                            result.update((single if item["suite"] == "single" else pair)(args, directory, environment["template"]))
                            monitor.check()
                        finally:
                            result.update(load_end=lib.read_loadavg(), finished=time.time())
                            retain_logs(directory, logs / directory.name)
                            report["results"].append(result)
                        run.log(f"{item}: {result['samples']}")
                finally:
                    monitor.close()
                monitor.check()
            report["history"] = aggregate(report["results"], environment, commit, dirty)
            medians = {(r["backend"], r["workload"]): r["median"] for r in report["history"]}
            report["bars"] = [{"workload": w, "qemu": medians["qemu", w],
                "vmlab_hv": medians["vmlab-hv", w],
                "ratio": medians["vmlab-hv", w] / medians["qemu", w],
                "passes": medians["vmlab-hv", w] / medians["qemu", w] >= lib.D28_BAR[w][1]}
                for w in WORKLOADS]
            report["complete"] = True
        except Exception as error:
            report["error"] = str(error)
        finally:
            report["finished"] = time.time()
            report["monitor"] = monitor.samples if monitor is not None else []
            output.parent.mkdir(parents=True, exist_ok=True)
            with output.open("x") as file:
                json.dump(report, file, indent=2)
        print(json.dumps({"output": str(output), "complete": report["complete"],
                          "bars": report.get("bars"), "error": report.get("error")}, indent=2))
        return 0 if report["complete"] else 1
    except (OSError, lib.BenchError) as error:
        print(f"phase2 window: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

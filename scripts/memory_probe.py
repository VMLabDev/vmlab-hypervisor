#!/usr/bin/env python3
"""Measure real-guest RSS and cgroup memory under an externally capped scope."""

import argparse
import array
import csv
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent / "hwref"))
from process_guard import VmPermit


def exact(connection, size):
    data = bytearray()
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise EOFError("display closed")
        data.extend(chunk)
    return data


def connect(path, child):
    deadline = time.monotonic() + 60
    while True:
        connection = socket.socket(socket.AF_UNIX)
        try:
            connection.connect(str(path))
            connection.settimeout(180)
            return connection
        except (FileNotFoundError, ConnectionRefusedError):
            connection.close()
            if child.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"socket unavailable: {path}; exit={child.poll()}")
            time.sleep(0.05)


def native(connection):
    connection.sendall(struct.pack("<IHHII", 16, 1, 0, 1, 1))
    header = exact(connection, 8)
    exact(connection, struct.unpack("<IHH", header)[0] - 8)
    # Receive the configure fds without releasing any frame. The producer must
    # skip frames once its three buffers are held, including across resizes.
    data, ancillary, _, _ = connection.recvmsg(65536, socket.CMSG_SPACE(64))
    for level, kind, payload in ancillary:
        if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
            fds = array.array("i")
            fds.frombytes(payload[:len(payload) - len(payload) % fds.itemsize])
            for fd in fds:
                os.close(fd)
    if not data:
        raise EOFError("native configure missing")


def rfb(connection, stop):
    assert exact(connection, 12) == b"RFB 003.008\n"
    connection.sendall(b"RFB 003.008\n")
    assert exact(connection, 2) == b"\x01\x01"
    connection.sendall(b"\x01")
    assert exact(connection, 4) == bytes(4)
    connection.sendall(b"\x01")
    init = exact(connection, 24)
    exact(connection, struct.unpack(">I", init[20:24])[0])
    connection.sendall(struct.pack(">BBHii", 2, 0, 2, 0, -223))
    while not stop.is_set():
        connection.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, 16384, 16384))
        header = exact(connection, 4)
        assert header[0] == 0
        for _ in range(struct.unpack(">H", header[2:])[0]):
            rect = exact(connection, 12)
            _, _, width, height, encoding = struct.unpack(">HHHHi", rect)
            if encoding == 0:
                exact(connection, width * height * 4)
            elif encoding != -223:
                raise ValueError(f"unexpected RFB encoding {encoding}")
        stop.wait(0.1)


def terminate(child):
    if child is None:
        return
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait()


def run(args):
    cgroup = Path("/sys/fs/cgroup") / Path("/proc/self/cgroup").read_text().strip().split("::", 1)[1].lstrip("/")
    maximum = (cgroup / "memory.max").read_text().strip()
    if maximum == "max" or int(maximum) > 48 * 1024**3:
        raise RuntimeError("run inside a scope with MemoryMax <= 48G")
    args.output.mkdir(parents=True, exist_ok=False)
    permit = VmPermit()
    child = helper = tpm_probe = fs_helper = None
    stop = threading.Event()
    workers = []
    connections = []
    with tempfile.TemporaryDirectory(prefix="hv-memory-") as temporary:
        path = Path(temporary)
        try:
            disk = path / "disk.qcow2"
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", str(args.template.resolve()), str(disk)], check=True)
            machine = {
                "name": "memory-probe", "machine": "q35", "arch": "x86_64", "accel": "kvm",
                "vcpus": 2, "memory_mib": 4096,
                "boot": {"kind": "firmware", "firmware": "seabios", "code": "/usr/share/qemu/bios-256k.bin",
                         "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
                "console_pci": {"bus": 0, "device": 1, "function": 0},
                "display": {"device": "virtio-vga" if args.scenario == "virtio" else "std-vga",
                            "pci": {"bus": 0, "device": 4, "function": 0}, "rom": None},
                "disks": [{"id": "disk0", "path": str(disk), "format": "qcow2", "read_only": False,
                           "vmstate": True, "pci": {"bus": 0, "device": 2, "function": 0}}],
                "nics": [], "serial_log": str(args.output.resolve() / "serial.log"),
                "ports": [{"name": "vmlab.agent.0", "socket": str(path / "agent.sock")}]}
            if args.scenario == "helpers":
                state = path / "tpm"
                state.mkdir()
                with (args.output / "swtpm.log").open("wb") as log:
                    helper = subprocess.Popen(["swtpm", "socket", "--tpm2", "--tpmstate", f"dir={state}",
                        "--ctrl", f"type=unixio,path={path / 'swtpm.sock'}", "--flags", "not-need-init"],
                        stdout=log, stderr=log, start_new_session=True)
                deadline = time.monotonic() + 10
                while not (path / "swtpm.sock").exists():
                    assert helper.poll() is None and time.monotonic() < deadline
                    time.sleep(0.05)
                # This branch rejects TPM machine attachment until phase 2c.
                # Exercise the implemented device/backend without claiming an
                # end-to-end VMM TPM attachment.
                with (args.output / "tpm-probe.log").open("wb") as log:
                    tpm_probe = subprocess.Popen([str(args.tpm_probe.resolve()), str(path / "swtpm.sock"), str(args.seconds + 60)], stdout=log, stderr=log, start_new_session=True)
                shared = path / "share"
                shared.mkdir()
                with (args.output / "virtiofsd.log").open("wb") as log:
                    fs_helper = subprocess.Popen(["/usr/lib/virtiofsd", "--socket-path", str(path / "fs.sock"), "--shared-dir", str(shared), "--cache", "auto", "--log-level", "warn"], stdout=log, stderr=log, start_new_session=True)
                deadline = time.monotonic() + 10
                while not (path / "fs.sock").exists():
                    assert fs_helper.poll() is None and time.monotonic() < deadline
                    time.sleep(0.05)
                machine["shares"] = [{"tag": "memory-probe", "socket": str(path / "fs.sock"), "pci": {"bus": 0, "device": 5, "function": 0}}]
            description = path / "machine.json"
            description.write_text(json.dumps(machine))
            with (args.output / "hv.log").open("wb") as log:
                child = subprocess.Popen([str(args.binary.resolve()), "--machine", str(description), "--control", str(path / "control.sock")], stdout=log, stderr=log, start_new_session=True)
            control = connect(path / "control.sock", child)
            connections.append(control)
            stream = control.makefile("rwb", buffering=0)
            stream.write(b'{"type":"hello","proto_version":1}\n')
            assert json.loads(stream.readline())["type"] == "hello"
            def call(command, **fields):
                stream.write((json.dumps({"type": "request", "id": 1, "command": {"command": command, **fields}}) + "\n").encode())
                while True:
                    reply = json.loads(stream.readline())
                    if reply.get("type") == "reply":
                        assert reply["outcome"] == "ok", reply
                        return
            started = time.monotonic()
            errors = []
            cycles = []
            def rss(pid):
                lines = Path(f"/proc/{pid}/status").read_text().splitlines()
                return int(next(line.split()[1] for line in lines if line.startswith("VmRSS:")))
            def tree_rss(pid):
                try:
                    children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
                    return rss(pid) + sum(tree_rss(int(child)) for child in children)
                except (FileNotFoundError, ProcessLookupError):
                    return 0
            def snapshots():
                try:
                    if stop.wait(60):
                        return
                    for iteration in range(20):
                        call("snapshot_save", name="probe")
                        call("snapshot_load", name="probe")
                        call("snapshot_delete", name="probe")
                        cycles.append({"cycle": iteration + 1, "seconds": time.monotonic() - started, "rss_kib": rss(child.pid)})
                        (args.output / "cycles.json").write_text(json.dumps(cycles, indent=2))
                        print(f"snapshot cycle {iteration + 1}/20", flush=True)
                        stop.wait(1)
                except Exception as error:
                    errors.append(error)
            if args.scenario in ("native", "rfb"):
                connection = connect(path / ("display.sock" if args.scenario == "native" else "vnc.sock"), child)
                connections.append(connection)
                if args.scenario == "native":
                    native(connection)
                else:
                    def viewer():
                        try:
                            rfb(connection, stop)
                        except Exception as error:
                            errors.append(error)
                    worker = threading.Thread(target=viewer)
                    worker.start()
                    workers.append(worker)
            if args.scenario == "snapshots":
                worker = threading.Thread(target=snapshots)
                worker.start()
                workers.append(worker)
            if args.scenario in ("virtio", "helpers"):
                def inspect_guest():
                    try:
                        with (args.output / "guest-driver.log").open("wb") as log:
                            subprocess.run([str(args.agent_probe.resolve()), str(path / "agent.sock"), args.scenario], stdout=log, stderr=log, check=True, timeout=180)
                    except Exception as error:
                        errors.append(error)
                worker = threading.Thread(target=inspect_guest)
                worker.start()
                workers.append(worker)
            samples = []
            with (args.output / "memory.csv").open("w") as output:
                writer = csv.writer(output)
                writer.writerow(["seconds", "rss_kib", "helper_rss_kib", "memory_current", "memory_anon", "memory_file", "fds", "threads", "tpm_probe_rss_kib", "virtiofsd_rss_kib"])
                while time.monotonic() - started < args.seconds or any(w.is_alive() for w in workers) and args.scenario == "snapshots":
                    assert child.poll() is None, f"VM exited {child.returncode}"
                    if tpm_probe:
                        assert tpm_probe.poll() is None, f"TPM probe exited {tpm_probe.returncode}"
                    if fs_helper:
                        assert fs_helper.poll() is None, f"virtiofsd exited {fs_helper.returncode}"
                    if errors:
                        raise errors[0]
                    def status(pid):
                        return dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines() if ":" in line)
                    info = status(child.pid)
                    stats = dict(line.split() for line in (cgroup / "memory.stat").read_text().splitlines())
                    elapsed = round(time.monotonic() - started, 2)
                    row = [elapsed, int(info["VmRSS"].split()[0]), int(status(helper.pid)["VmRSS"].split()[0]) if helper else 0,
                           int((cgroup / "memory.current").read_text()), int(stats["anon"]), int(stats["file"]),
                           len(list(Path(f"/proc/{child.pid}/fd").iterdir())), int(info["Threads"]), rss(tpm_probe.pid) if tpm_probe else 0, tree_rss(fs_helper.pid) if fs_helper else 0]
                    writer.writerow(row)
                    samples.append(row)
                    output.flush()
                    print(args.scenario, row, flush=True)
                    stop.wait(5)
            if errors:
                raise errors[0]
            warm = [row for row in samples if row[0] >= 60]
            summary = {"scenario": args.scenario, "seconds": time.monotonic() - started,
                       "rss_kib_first": samples[0][1], "rss_kib_last": samples[-1][1],
                       "rss_kib_peak": max(row[1] for row in samples),
                       "scope_bytes_peak": max(row[3] for row in samples),
                       "rss_kib_highwater": int(next(line.split()[1] for line in Path(f"/proc/{child.pid}/status").read_text().splitlines() if line.startswith("VmHWM:"))),
                       "warm_rss_kib_min": min(row[1] for row in warm) if warm else None,
                       "warm_rss_kib_max": max(row[1] for row in warm) if warm else None,
                       "swtpm_rss_kib_max": max(row[2] for row in samples),
                       "tpm_probe_rss_kib_max": max(row[8] for row in samples),
                       "virtiofsd_rss_kib_max": max(row[9] for row in samples),
                       "memory_events": (cgroup / "memory.events").read_text(), "cycles": len(cycles)}
            (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
            if args.scenario == "snapshots":
                assert len(cycles) == 20
                assert max(c["rss_kib"] for c in cycles) - min(c["rss_kib"] for c in cycles) < 256 * 1024, cycles
            elif warm:
                assert max(row[1] for row in warm) - min(row[1] for row in warm) < 256 * 1024, summary
                for index in (2, 8, 9):
                    assert max(row[index] for row in warm) - min(row[index] for row in warm) < 8 * 1024, summary
        finally:
            stop.set()
            for connection in connections:
                connection.close()
            terminate(child)
            terminate(tpm_probe)
            terminate(fs_helper)
            terminate(helper)
            for worker in workers:
                worker.join(timeout=10)
            permit.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario", choices=["idle", "native", "rfb", "snapshots", "virtio", "helpers"])
    parser.add_argument("--template", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/vmlab-hv"))
    parser.add_argument("--agent-probe", type=Path, default=Path("target/debug/examples/memory_agent"))
    parser.add_argument("--tpm-probe", type=Path, default=Path("target/debug/examples/memory_tpm"))
    parser.add_argument("--seconds", type=int, default=600)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args())

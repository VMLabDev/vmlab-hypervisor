"""Firmware probe capture through the vmlab-hv machine/control interfaces."""

from __future__ import annotations

import io
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time

import capture


def unsupported_firmware(stderr: str) -> bool:
    return ("unknown variant `firmware`" in stderr
            or "unknown field `machine`" in stderr
            or "firmware boot is not supported" in stderr.lower())


def resume(control: Path, process: subprocess.Popen, timeout: float = 20) -> None:
    deadline = time.monotonic() + timeout
    with socket.socket(socket.AF_UNIX) as connection:
        while True:
            try:
                connection.connect(str(control))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if process.poll() is not None:
                    raise RuntimeError(f"vmlab-hv exited {process.returncode} before control ready")
                if time.monotonic() >= deadline:
                    raise TimeoutError("vmlab-hv control socket did not become ready")
                time.sleep(0.05)
        connection.settimeout(timeout)
        with connection.makefile("rwb") as stream:
            stream.write(b'{"type":"hello","proto_version":1}\n')
            stream.flush()
            hello = json.loads(stream.readline())
            if hello.get("type") != "hello" or hello.get("proto_version") != 1:
                raise RuntimeError(f"control handshake failed: {hello}")
            stream.write(b'{"type":"request","id":1,"command":{"command":"resume"}}\n')
            stream.flush()
            while True:
                line = stream.readline()
                if not line:
                    raise RuntimeError("control closed before resume reply")
                message = json.loads(line)
                if message.get("type") == "reply" and message.get("id") == 1:
                    if message.get("outcome") != "ok":
                        raise RuntimeError(f"resume failed: {message}")
                    return


def prepare_machine(machine: dict, run: Path, output: Path, assets) -> dict:
    """Retain caller-owned device fields, including the explicit PCI layout."""
    machine = json.loads(json.dumps(machine))
    if machine.get("machine") != "q35" or machine.get("boot", {}).get("kind") != "firmware":
        raise ValueError("--machine must select q35 and Boot::Firmware (boot.kind=firmware)")
    probes = [disk for disk in machine.get("disks", []) if disk.get("id") == "hwref-probe"]
    if len(probes) != 1:
        raise ValueError("--machine must contain exactly one disk with id hwref-probe")
    boot = machine["boot"]
    is_ovmf = bool(boot.get("vars"))
    disk = run / "probe.raw"
    shutil.copy2(assets.esp if is_ovmf else assets.bios_disk, disk)
    probes[0].update(path=str(disk), format="raw", read_only=False)
    for index, other in enumerate(machine["disks"]):
        if other is probes[0] or other.get("read_only", False):
            continue
        overlay = run / f"disk{index}.qcow2"
        capture.sh([capture.QEMU_IMG, "create", "-q", "-f", "qcow2", "-F", other["format"],
                    "-b", str(Path(other["path"]).resolve()), str(overlay)])
        other.update(path=str(overlay), format="qcow2")
    machine["serial_log"] = str(output / "serial.log")
    ports = [p for p in machine.get("ports", []) if p["name"] == "vmlab.agent.0"]
    if len(ports) != 1:
        raise ValueError("--machine must have exactly one vmlab.agent.0 port")
    ports[0]["socket"] = str(run / "agent.sock")
    if is_ovmf:
        variables = run / "OVMF_VARS.fd"
        shutil.copy2(boot["vars"], variables)
        boot["vars"] = str(variables)
    for index, nic in enumerate(machine.get("nics", [])):
        nic["attachment"] = {"kind": "stream", "socket": str(run / f"nic{index}.sock")}
    return machine


def capture_hv(args, assets) -> int:
    if not args.binary.is_file():
        print(f"SKIP: vmlab-hv binary missing: {args.binary}")
        return 0
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        print("SKIP: /dev/kvm is unavailable")
        return 0
    if not args.machine or not args.output:
        print("vmlab-hv capture requires --machine and a fresh --output")
        return 2
    output = args.output.resolve()
    if output.exists() and any(output.iterdir()):
        print("vmlab-hv --output must be empty or absent")
        return 2
    machine = json.loads(args.machine.read_text())
    required = [capture.KERNEL, capture.STUB]
    required.extend(Path(machine["boot"][key]) for key in ("code", "vars") if machine.get("boot", {}).get(key))
    missing = [str(path) for path in required if not path.is_file()]
    if missing:
        print("SKIP: missing probe/firmware prerequisites: " + ", ".join(missing))
        return 0
    output.mkdir(parents=True, exist_ok=True)
    assets.ensure()
    process = None
    reader = None
    sinks = []
    with tempfile.TemporaryDirectory(prefix="hwref-hv-") as tmp:
        run = Path(tmp)
        try:
            description = prepare_machine(machine, run, output, assets)
            path = output / "machine.json"
            path.write_text(json.dumps(description, indent=2) + "\n")
            for index in range(len(description.get("nics", []))):
                sink = capture.NicSink(run / f"nic{index}.sock")
                sink.start()
                sinks.append(sink)
            argv = [str(args.binary.resolve()), "--machine", str(path),
                    "--control", str(run / "control.sock"), "--paused"]
            (output / "hv-argv.json").write_text(json.dumps(argv, indent=2) + "\n")
            with (output / "hv-stderr.log").open("wb") as log:
                process = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
                # Connect the guest stream while vCPUs are paused.
                reader = capture.ChanReader(run / "agent.sock")
                reader.start()
                deadline = time.monotonic() + 20
                while not reader.connected and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.05)
                resume(run / "control.sock", process)
                process.wait(timeout=capture.BOOT_TIMEOUT)
            reader.join(timeout=5)
            raw = bytes(reader.data)
            (output / "agent-channel.log").write_bytes(raw)
            if process.returncode:
                raise RuntimeError(f"vmlab-hv exited {process.returncode}")
            archive = capture.decode_stream(raw)
            if archive is None:
                raise RuntimeError("no valid HWREF guest archive")
            guest = output / "guest"
            guest.mkdir()
            with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as stream:
                stream.extractall(guest, filter="data")
            from diff import load_capture
            load_capture(output)
            print(f"OK vmlab-hv: {output}")
            return 0
        except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as error:
            log = output / "hv-stderr.log"
            stderr = log.read_text(errors="replace") if log.exists() else ""
            if unsupported_firmware(stderr):
                print(f"SKIP: vmlab-hv does not support q35 Boot::Firmware: {stderr.strip()}")
                (output / "SKIPPED.txt").write_text(stderr)
                return 0
            print(f"FAIL vmlab-hv: {error}\n{stderr}")
            return 1
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait()
            if reader is not None:
                reader.close()
                reader.join(timeout=2)
            for sink in sinks:
                sink.close()

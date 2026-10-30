#!/usr/bin/env python3
"""P1: boot the built NIC ROM through vmlab-hv's control socket."""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def request(stream, message):
    stream.write(json.dumps(message).encode() + b"\n")
    stream.flush()
    while True:
        reply = json.loads(stream.readline())
        if reply.get("type") != "event":
            return reply


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--rom", type=Path, default=Path(__file__).resolve().parents[2] / "target/ipxe/virtio-net.rom")
    args = parser.parse_args()
    bios = Path("/usr/share/qemu/bios-256k.bin")
    vga = Path("/usr/share/qemu/vgabios-virtio.bin")
    for path in (Path("/dev/kvm"), bios, vga, args.rom):
        if not path.exists():
            print(f"SKIP: iPXE firmware boot requires {path}")
            return
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        print("SKIP: /dev/kvm is not accessible")
        return
    with tempfile.TemporaryDirectory(prefix="hv-ipxe-") as temporary:
        root = Path(temporary)
        serial = root / "serial.log"
        control = root / "control.sock"
        machine = {
            "machine": "q35", "name": "ipxe", "arch": "x86_64", "accel": "kvm",
            "vcpus": 2, "memory_mib": 512,
            "boot": {"kind": "firmware", "firmware": "seabios", "code": str(bios)},
            "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 4, "function": 0}, "rom": str(vga)},
            "nics": [{"pci": {"bus": 0, "device": 3, "function": 0},
                      "rom": str(args.rom.resolve()), "mac": "52:54:00:12:34:56",
                      "attachment": {"kind": "stream", "socket": str(root / "switch.sock")}}],
            "ports": [], "disks": [], "serial_log": str(serial),
        }
        description = root / "machine.json"
        description.write_text(json.dumps(machine))
        with (root / "stderr.log").open("wb") as log:
            process = subprocess.Popen([str(args.binary.resolve()), "--machine", str(description),
                                        "--control", str(control), "--paused"], stderr=log,
                                       env=dict(os.environ, VMLAB_HV_DIAGNOSTICS="1"))
            try:
                deadline = time.monotonic() + 30
                with socket.socket(socket.AF_UNIX) as connection:
                    while True:
                        try:
                            connection.connect(str(control))
                            break
                        except (FileNotFoundError, ConnectionRefusedError):
                            if process.poll() is not None or time.monotonic() >= deadline:
                                raise AssertionError("control socket did not become ready")
                            time.sleep(0.02)
                    connection.settimeout(10)
                    with connection.makefile("rwb") as stream:
                        hello = request(stream, {"type": "hello", "proto_version": 1})
                        assert hello["type"] == "hello", hello
                        resumed = request(stream, {"type": "request", "id": 1, "command": {"command": "resume"}})
                        assert resumed.get("outcome") == "ok", resumed
                        deadline = time.monotonic() + 8
                        while True:
                            text = serial.read_text(errors="replace") if serial.exists() else ""
                            if "iPXE" in text:
                                break
                            if process.poll() is not None:
                                raise AssertionError("VMM exited before option ROM observation")
                            if time.monotonic() >= deadline:
                                break
                            time.sleep(0.05)
                        stopped = request(stream, {"type": "request", "id": 2, "command": {"command": "quit"}})
                        assert stopped.get("outcome") == "ok", stopped
                        assert process.wait(timeout=10) == 0
                        # The legacy ROM prefix uses BIOS text services before
                        # iPXE initializes its own serial console. The VMM's
                        # diagnostics read the actual VGA text memory at quit.
                        diagnostics = (root / "stderr.log").read_text(errors="replace")
                        vga_text = "\n".join(line for line in diagnostics.splitlines() if line.startswith("VGA text:"))
                        assert "iPXE" in text or ("iPXE" in vga_text and "00:03.0" in vga_text), diagnostics
                        print("P1: control resume -> SeaBIOS -> iPXE banner")
                        print(vga_text or text)
            except BaseException:
                print((root / "stderr.log").read_text(errors="replace"))
                print(serial.read_text(errors="replace") if serial.exists() else "No serial log")
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                process.wait()


if __name__ == "__main__":
    main()

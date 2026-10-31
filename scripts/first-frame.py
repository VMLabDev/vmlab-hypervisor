#!/usr/bin/env python3
"""Compare OVMF first non-black RFB frames after resume, using fresh VARS.

Run inside systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0.
No guest disk is attached; only firmware execution is measured. Each run owns
and reaps one VM. QEMU is used only through its executable and socket protocols.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import tempfile
import time


def read(stream, count):
    data = bytearray()
    while len(data) < count:
        part = stream.recv(count - len(data))
        if not part:
            raise EOFError("RFB closed")
        data.extend(part)
    return data


def connect(path, process):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"VM exited with {process.returncode}")
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(30)
        try:
            stream.connect(str(path))
            return stream
        except (FileNotFoundError, ConnectionRefusedError):
            stream.close()
            time.sleep(.01)
    raise TimeoutError(f"waiting for {path}")


def send(stream, value):
    stream.sendall(json.dumps(value).encode() + b"\n")


def frame(stream):
    stream.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, 65535, 65535))
    assert read(stream, 2) == b"\0\0"
    count = struct.unpack(">H", read(stream, 2))[0]
    nonblack = False
    digest = hashlib.sha256()
    width = height = 0
    for _ in range(count):
        _, _, width, height, encoding = struct.unpack(">HHHHi", read(stream, 12))
        if encoding == -223:
            continue
        assert encoding == 0, encoding
        pixels = read(stream, width * height * 4)
        nonblack |= any(pixels[0::4]) or any(pixels[1::4]) or any(pixels[2::4])
        digest.update(struct.pack(">HH", width, height))
        digest.update(pixels)
    return nonblack, digest.digest(), width, height


def first_frame(args, backend):
    with tempfile.TemporaryDirectory(prefix="hv-first-frame-") as temporary:
        root = Path(temporary)
        vars_path = root / "vars.fd"
        shutil.copyfile(args.vars, vars_path)
        rfb = root / "rfb.sock"
        control = root / "control.sock"
        if backend == "qemu":
            command = ["qemu-system-x86_64", "-S", "-nodefaults", "-machine", "q35",
                       "-accel", "kvm", "-cpu", "host", "-smp", "2", "-m", "4096",
                       "-drive", f"if=pflash,format=raw,readonly=on,file={args.code}",
                       "-drive", f"if=pflash,format=raw,file={vars_path}",
                       "-device", "virtio-vga,addr=4", "-display", "none",
                       "-vnc", f"unix:{rfb}", "-qmp", f"unix:{control},server=on,wait=off"]
        else:
            description = {
                "machine": "q35", "name": "first-frame", "arch": "x86_64", "accel": "kvm",
                "vcpus": 2, "memory_mib": 4096,
                "boot": {"kind": "firmware", "firmware": "ovmf", "code": str(args.code),
                         "vars": str(vars_path)},
                "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 4, "function": 0},
                            "socket": str(root / "display.sock"), "vnc_socket": str(rfb)},
                "disks": [], "nics": [], "ports": [], "serial_log": str(root / "serial.log"),
            }
            machine = root / "machine.json"
            machine.write_text(json.dumps(description))
            command = [str(args.binary), "--machine", str(machine), "--control", str(control), "--paused"]
        with (root / "stderr.log").open("w+") as stderr:
            process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=stderr)
            try:
                with connect(control, process) as ctrl:
                    reader = ctrl.makefile("rb")
                    if backend == "qemu":
                        assert "QMP" in json.loads(reader.readline())
                        send(ctrl, {"execute": "qmp_capabilities"})
                    else:
                        send(ctrl, {"type": "hello", "proto_version": 1})
                    hello = json.loads(reader.readline())
                    assert "error" not in hello and hello.get("type") != "error", hello
                    with connect(rfb, process) as stream:
                        assert read(stream, 12) == b"RFB 003.008\n"
                        stream.sendall(b"RFB 003.008\n")
                        assert 1 in read(stream, read(stream, 1)[0])
                        stream.sendall(b"\1")
                        assert read(stream, 4) == bytes(4)
                        stream.sendall(b"\1")
                        read(stream, 4)
                        pixel_format = read(stream, 16)
                        assert pixel_format[0] == 32 and pixel_format[2] == 0, pixel_format
                        read(stream, struct.unpack(">I", read(stream, 4))[0])
                        stream.sendall(struct.pack(">BBHii", 2, 0, 2, 0, -223))
                        # QEMU can supply a host-generated placeholder before
                        # the guest starts. It is not a firmware paint.
                        _, initial, _, _ = frame(stream)
                        start = time.monotonic()
                        send(ctrl, {"execute": "cont"} if backend == "qemu" else {
                            "type": "request", "id": 1, "command": {"command": "resume"}})
                        frames = 0
                        while time.monotonic() - start < 30:
                            nonblack, current, width, height = frame(stream)
                            frames += 1
                            if nonblack and current != initial:
                                return {"backend": backend, "seconds": round(time.monotonic() - start, 6),
                                        "frames": frames, "width": width, "height": height}
                            time.sleep(.01)
                        raise TimeoutError("no non-black frame in 30 seconds")
            except Exception:
                stderr.flush()
                stderr.seek(0)
                print(stderr.read())
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/vmlab-hv"))
    parser.add_argument("--code", type=Path, default=Path("/usr/share/edk2/x64/OVMF_CODE.4m.fd"))
    parser.add_argument("--vars", type=Path, default=Path("/usr/share/edk2/x64/OVMF_VARS.4m.fd"))
    parser.add_argument("--backend", choices=["both", "qemu", "vmlab-hv"], default="both")
    parser.add_argument("--runs", type=int, default=5)
    arguments = parser.parse_args()
    backends = ["qemu", "vmlab-hv"] if arguments.backend == "both" else [arguments.backend]
    for _ in range(arguments.runs):
        for name in backends:
            print(json.dumps(first_frame(arguments, name)), flush=True)

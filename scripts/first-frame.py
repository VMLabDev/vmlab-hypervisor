#!/usr/bin/env python3
"""Compare OVMF first non-black RFB frames after resume, using fresh VARS.

Run inside systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0.
By default no guest disk is attached and only firmware execution is measured.
--disk boots a linked clone of an image, and --vga-rom loads the display
option ROM, as vmlab does. Each run owns and reaps one VM. QEMU is used only through its executable and socket protocols.
"""

import argparse
import array
import contextlib
import hashlib
import json
import os
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


class NativeFrames:
    def __init__(self, stream):
        self.stream = stream
        self.buffers = []
        stream.sendall(struct.pack("<IHHII", 16, 1, 0, 1, 33))
        kind, flags, payload, fds = self.packet()
        assert (kind, flags, fds) == (1, 0, [])
        version, capabilities = struct.unpack("<II", payload)
        assert version == 1 and capabilities & 33 == 33

    def close(self):
        for fd in self.buffers:
            os.close(fd)
        self.buffers = []

    def packet(self):
        header, ancillary, flags, _ = self.stream.recvmsg(8, socket.CMSG_SPACE(16 * 4))
        if not header:
            raise EOFError("native display closed")
        assert not flags & socket.MSG_CTRUNC
        header += read(self.stream, 8 - len(header))
        length, kind, frame_flags = struct.unpack("<IHH", header)
        assert 8 <= length <= 16 * 1024 * 1024
        fds = array.array("i")
        for level, kind_of_control, data in ancillary:
            assert (level, kind_of_control) == (socket.SOL_SOCKET, socket.SCM_RIGHTS)
            fds.frombytes(data)
        return kind, frame_flags, read(self.stream, length - 8), list(fds)

    def frame(self):
        while True:
            kind, flags, payload, fds = self.packet()
            if kind == 2:
                _, self.width, self.height, _, self.stride, _, surface_kind, count = struct.unpack("<QIIIIQII", payload)
                assert surface_kind == 0 and count == len(fds)
                self.close()
                self.buffers = fds
            elif kind == 3:
                assert not fds
                surface, buffer, seq, _, _ = struct.unpack("<QIQQI", payload[:32])
                pixels = os.pread(self.buffers[buffer], self.stride * self.height, 0)
                assert len(pixels) == self.stride * self.height
                self.stream.sendall(struct.pack("<IHHQIQ", 28, 4, 0, surface, buffer, seq))
                nonblack = any(pixels[0::4]) or any(pixels[1::4]) or any(pixels[2::4])
                return nonblack, hashlib.sha256(pixels).digest(), self.width, self.height, bool(flags & 1)
            else:
                for fd in fds:
                    os.close(fd)
                raise ValueError(f"unexpected native display message {kind}")


def first_frame(args, backend):
    with tempfile.TemporaryDirectory(prefix="hv-first-frame-") as temporary:
        root = Path(temporary)
        vars_path = root / "vars.fd"
        shutil.copyfile(args.vars, vars_path)
        disk = None
        if args.disk:
            disk = root / "disk.qcow2"
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2",
                            "-b", str(args.disk.resolve()), str(disk)], check=True)
        rfb = root / "rfb.sock"
        control = root / "control.sock"
        if backend == "qemu":
            command = ["qemu-system-x86_64", "-S", "-nodefaults", "-machine", "q35",
                       "-accel", "kvm", "-cpu", "host", "-smp", str(args.vcpus),
                       "-m", str(args.memory),
                       "-drive", f"if=pflash,format=raw,readonly=on,file={args.code}",
                       "-drive", f"if=pflash,format=raw,file={vars_path}",
                       "-device", "virtio-vga,addr=4" + (
                           f",romfile={args.vga_rom}" if args.vga_rom else ",romfile="),
                       "-display", "none",
                       "-vnc", f"unix:{rfb}", "-qmp", f"unix:{control},server=on,wait=off"]
            if disk:
                command += ["-drive", f"if=none,id=disk0,format=qcow2,file={disk}",
                            "-device", "virtio-blk-pci,drive=disk0,addr=2"]
        else:
            description = {
                "machine": "q35", "name": "first-frame", "arch": "x86_64", "accel": "kvm",
                "vcpus": args.vcpus, "memory_mib": args.memory,
                "boot": {"kind": "firmware", "firmware": "ovmf", "code": str(args.code),
                         "vars": str(vars_path)},
                "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 4, "function": 0},
                            "rom": str(args.vga_rom) if args.vga_rom else None,
                            "socket": str(root / "display.sock"), "vnc_socket": str(rfb)},
                "disks": [], "nics": [], "ports": [], "serial_log": str(root / "serial.log"),
            }
            if disk:
                description["disks"] = [{"id": "disk0", "pci": {"bus": 0, "device": 2, "function": 0},
                                         "path": str(disk), "format": "qcow2", "read_only": False}]
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
                    with connect(rfb, process) as stream, contextlib.ExitStack() as stack:
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
                        placeholder, initial, _, _ = frame(stream)
                        native = None
                        if backend == "vmlab-hv":
                            native = NativeFrames(stack.enter_context(connect(root / "display.sock", process)))
                            stack.callback(native.close)
                            assert native.frame()[4], "paused VM did not publish a placeholder"
                        start = time.monotonic()
                        send(ctrl, {"execute": "cont"} if backend == "qemu" else {
                            "type": "request", "id": 1, "command": {"command": "resume"}})
                        frames = 0
                        while time.monotonic() - start < 30:
                            if native:
                                nonblack, current, width, height, host_frame = native.frame()
                            else:
                                nonblack, current, width, height = frame(stream)
                                host_frame = current == initial
                            frames += 1
                            if nonblack and not host_frame:
                                return {"backend": backend, "seconds": round(time.monotonic() - start, 6),
                                        "frames": frames, "width": width, "height": height,
                                        "lit_while_paused": placeholder}
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
                if args.show_stderr:
                    stderr.seek(0)
                    print(stderr.read(), end="")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/vmlab-hv"))
    parser.add_argument("--code", type=Path, default=Path("/usr/share/edk2/x64/OVMF_CODE.4m.fd"))
    parser.add_argument("--vars", type=Path, default=Path("/usr/share/edk2/x64/OVMF_VARS.4m.fd"))
    parser.add_argument("--backend", choices=["both", "qemu", "vmlab-hv"], default="both")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--vcpus", type=int, default=2)
    parser.add_argument("--memory", type=int, default=4096, help="MiB")
    parser.add_argument("--disk", type=Path, help="qcow2 image to boot through a linked clone")
    parser.add_argument("--show-stderr", action="store_true", help="print the VM's stderr after each run")
    parser.add_argument("--vga-rom", type=Path, help="display option ROM, as vmlab passes it")
    arguments = parser.parse_args()
    backends = ["qemu", "vmlab-hv"] if arguments.backend == "both" else [arguments.backend]
    for _ in range(arguments.runs):
        for name in backends:
            print(json.dumps(first_frame(arguments, name)), flush=True)

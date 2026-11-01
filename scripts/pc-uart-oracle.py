#!/usr/bin/env python3
"""Observe an i440fx UART through QEMU qtest and a real socket chardev."""

import argparse
import contextlib
import ctypes
import datetime
import fcntl
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


CHARACTER_NS = 86806


def project_vms():
    processes = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            arguments = (entry / "cmdline").read_bytes().split(b"\0")
        except (FileNotFoundError, ProcessLookupError):
            continue
        if not arguments or not arguments[0]:
            continue
        name = os.path.basename(arguments[0])
        if (name.startswith(b"qemu-system-") and b"-avd" not in arguments) or (
            name == b"vmlab-hv" or name.startswith(b"vmlab-hv-test-")
        ):
            processes.append(int(entry.name))
    return processes


@contextlib.contextmanager
def permit(timeout):
    directory = Path("/tmp") / f"vmlab-hv-vm-slots-{os.geteuid()}"
    directory.mkdir(exist_ok=True)
    deadline = time.monotonic() + timeout
    held = None
    try:
        while held is None:
            for slot in range(int(os.environ.get("VMLAB_HV_TEST_VM_LIMIT", "4"))):
                candidate = (directory / f"{slot}.lock").open("a+b")
                try:
                    fcntl.flock(candidate, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    candidate.close()
                    continue
                held = candidate
                break
            if held is None:
                if time.monotonic() >= deadline:
                    raise TimeoutError("waiting for a shared test VM permit")
                time.sleep(0.1)
        while len(project_vms()) >= int(os.environ.get("VMLAB_HV_TEST_VM_LIMIT", "4")):
            if time.monotonic() >= deadline:
                raise TimeoutError(f"waiting for host VM capacity: {project_vms()}")
            time.sleep(0.1)
        yield
    finally:
        if held is not None:
            fcntl.flock(held, fcntl.LOCK_UN)
            held.close()


def require_scope():
    entries = Path("/proc/self/cgroup").read_text().splitlines()
    relative = next((line.split(":", 2)[2] for line in entries if line.startswith("0::")), None)
    if relative is None:
        raise RuntimeError("the UART oracle requires cgroup v2 memory limits")
    directory = Path("/sys/fs/cgroup") / relative.lstrip("/")
    memory = (directory / "memory.max").read_text().strip()
    swap = (directory / "memory.swap.max").read_text().strip()
    if memory == "max" or int(memory) > 24 * 1024**3 or swap != "0":
        raise RuntimeError("run under systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0")


def connect(path, process, timeout=10):
    deadline = time.monotonic() + timeout
    while True:
        stream = socket.socket(socket.AF_UNIX)
        try:
            stream.connect(str(path))
            return stream
        except (FileNotFoundError, ConnectionRefusedError):
            stream.close()
        if process.poll() is not None:
            raise RuntimeError(f"QEMU exited before binding {path}")
        if time.monotonic() >= deadline:
            raise TimeoutError(f"waiting for {path}")
        time.sleep(0.01)


class Qtest:
    def __init__(self, stream):
        self.stream = stream
        self.stream.settimeout(5)
        self.reader = stream.makefile("rb")

    def command(self, command):
        self.stream.sendall(command.encode("ascii") + b"\n")
        while True:
            line = self.reader.readline()
            if not line:
                raise RuntimeError("qtest connection closed")
            response = line.decode("ascii").strip().split()
            if response and response[0] == "IRQ":
                continue
            if not response or response[0] != "OK":
                raise RuntimeError(f"qtest {command!r}: {line!r}")
            return int(response[1], 0) if len(response) > 1 else None

    def out(self, register, value):
        self.command(f"outb {0x3f8 + register:#x} {value:#x}")

    def read(self, register):
        return self.command(f"inb {0x3f8 + register:#x}")

    def step(self, ns=1):
        return self.command(f"clock_step {ns}")

    def configure(self, fcr=7, ier=0):
        for register, value in [(3, 0x83), (0, 1), (1, 0), (3, 3), (2, fcr), (1, ier)]:
            self.out(register, value)


def until(probe, condition, timeout=5):
    deadline = time.monotonic() + timeout
    while True:
        value = probe()
        if condition(value):
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(f"oracle condition did not become true; last value {value:#x}")
        time.sleep(0.001)


def drain(stream):
    result = bytearray()
    while True:
        try:
            chunk = stream.recv(65536)
        except BlockingIOError:
            return bytes(result)
        if not chunk:
            raise RuntimeError("UART chardev disconnected")
        result.extend(chunk)
        if len(result) > 1024 * 1024:
            raise RuntimeError("unexpected unbounded UART output")


def rx(qtest, count):
    result = bytearray()
    for _ in range(count):
        status = until(lambda: qtest.read(5), lambda value: value & 1)
        if status & 2:
            raise RuntimeError(f"receive overrun at byte {len(result)}")
        result.append(qtest.read(0))
    return bytes(result)


def observe(qtest, uart, result):
    qtest.configure(7, 0)
    uart.sendall(b"Z")
    assert rx(qtest, 1) == b"Z", "prove the real socket backend was accepted"

    payload = b"ABCDEFGHIJKLMNOPQRST"
    qtest.out(1, 2)
    before = qtest.step()
    for byte in payload:
        qtest.out(0, byte)
    immediate = drain(uart)
    result["tx_unblocked"] = {
        "input_hex": payload.hex(), "immediate_hex": immediate.hex(),
        "lsr": qtest.read(5), "iir": qtest.read(2), "virtual_ns": before,
        "samples": [],
    }
    all_output = bytearray(immediate)
    for increment in [1, 86804, 1, CHARACTER_NS * 30]:
        now = qtest.step(increment)
        output = drain(uart)
        all_output.extend(output)
        result["tx_unblocked"]["samples"].append({
            "virtual_ns": now, "output_hex": output.hex(),
            "lsr": qtest.read(5), "iir": qtest.read(2),
        })
    result["tx_unblocked"]["total_hex"] = all_output.hex()
    result["tx_unblocked"]["complete_delivery"] = all_output == payload

    qtest.configure(7, 2)
    filler = 0
    while filler < 65536:
        qtest.out(0, ord("a"))
        filler += 1
        status = qtest.read(5)
        if status & 0x60 != 0x60:
            # A timed backend may still be sending rather than blocked.
            qtest.step(CHARACTER_NS)
            status = qtest.read(5)
            if status & 0x60 != 0x60:
                break
    else:
        raise RuntimeError("could not fill QEMU's socket send buffer within 65536 bytes")
    markers = bytes(range(0xb0, 0xc1))
    blocked_status = qtest.read(5)
    for byte in markers:
        qtest.out(0, byte)
    full_status, full_iir = qtest.read(5), qtest.read(2)
    collected = bytearray(drain(uart))
    for _ in range(200):
        qtest.step(CHARACTER_NS)
        collected.extend(drain(uart))
        if qtest.read(5) & 0x60 == 0x60:
            # Give the chardev's writable callback one more main-loop turn.
            qtest.step(1)
            collected.extend(drain(uart))
            break
    else:
        raise TimeoutError("UART did not drain after client resumed reading")
    result["tx_backpressure"] = {
        "filler_written": filler, "filler_received": collected.count(ord("a")),
        "blocked_lsr": blocked_status, "full_lsr": full_status, "full_iir": full_iir,
        "markers_written_hex": markers.hex(),
        "markers_received_hex": bytes(byte for byte in collected if byte >= 0xb0).hex(),
        "final_lsr": qtest.read(5), "final_iir": qtest.read(2),
    }
    result["tx_backpressure"]["filler_preserved"] = collected.count(ord("a")) == filler

    qtest.configure(0xc7, 1)
    burst = bytes(range(32))
    uart.sendall(burst)
    until(lambda: qtest.read(5), lambda value: value & 1)
    burst_iir = qtest.read(2)
    received = rx(qtest, len(burst))
    result["rx_backpressure"] = {
        "sent_hex": burst.hex(), "received_hex": received.hex(),
        "initial_iir": burst_iir, "final_lsr": qtest.read(5),
    }
    result["rx_backpressure"]["complete_delivery"] = received == burst

    result["rx_triggers"] = []
    for fcr, threshold in [(0x47, 4), (0x87, 8), (0xc7, 14)]:
        qtest.configure(fcr, 1)
        uart.sendall(b"x" * (threshold - 1))
        until(lambda: qtest.read(5), lambda value: value & 1)
        below = qtest.read(2)
        uart.sendall(b"y")
        reached = until(lambda: qtest.read(2), lambda value: value & 15 == 4)
        received = rx(qtest, threshold)
        result["rx_triggers"].append({
            "threshold": threshold, "below_iir": below, "reached_iir": reached,
            "received_hex": received.hex(), "empty_iir": qtest.read(2),
        })
        result["rx_triggers"][-1]["complete_delivery"] = received == b"x" * (threshold - 1) + b"y"

    qtest.configure(0xc7, 1)
    uart.sendall(b"~")
    until(lambda: qtest.read(5), lambda value: value & 1)
    samples = [{"elapsed_ns": 0, "iir": qtest.read(2)}]
    qtest.step(CHARACTER_NS * 3)
    samples.append({"elapsed_ns": CHARACTER_NS * 3, "iir": qtest.read(2)})
    qtest.step(CHARACTER_NS)
    samples.append({"elapsed_ns": CHARACTER_NS * 4, "iir": qtest.read(2)})
    received = rx(qtest, 1)
    result["rx_timeout"] = {"samples": samples, "received_hex": received.hex(), "cleared_iir": qtest.read(2)}
    result["rx_timeout"]["four_character_interrupt"] = samples[-1]["iir"] & 15 == 12

    qtest.configure(0, 1)
    uart.sendall(b"1234")
    received = rx(qtest, 4)
    result["rx_nonfifo"] = {"received_hex": received.hex(), "final_lsr": qtest.read(5), "iir": qtest.read(2)}
    result["rx_nonfifo"]["complete_delivery"] = received == b"1234"


def observe_chipset_reset(qtest, result):
    def select(bdf, offset):
        qtest.command(f"outl 0xcf8 {0x80000000 | bdf << 8 | offset:#x}")

    configs = {}
    for bdf in [0, 8, 9, 11]:
        config = bytearray()
        for offset in range(0, 256, 4):
            select(bdf, offset)
            config.extend(qtest.command("inl 0xcfc").to_bytes(4, "little"))
        configs[f"{bdf // 8:02x}.{bdf % 8}"] = config.hex()
    result["chipset_reset_config_hex"] = configs
    result["piix3_command_writes"] = []
    for value in [0, 3, 7, 0xffff]:
        select(8, 4)
        qtest.command(f"outw 0xcfc {value:#x}")
        result["piix3_command_writes"].append({"written": value, "read": qtest.command("inw 0xcfc")})
    select(8, 4)
    qtest.command(f"outw 0xcfc {int.from_bytes(bytes.fromhex(configs['01.0'])[4:6], 'little'):#x}")
    result["chipset_config_write_replay"] = []
    for bdf in [0, 8, 9, 11]:
        for value in [0xffffffff, 0]:
            for offset in range(0, 256, 4):
                select(bdf, offset)
                qtest.command(f"outl 0xcfc {value:#x}")
            config = bytearray()
            for offset in range(0, 256, 4):
                select(bdf, offset)
                config.extend(qtest.command("inl 0xcfc").to_bytes(4, "little"))
            result["chipset_config_write_replay"].append({
                "bdf": f"{bdf // 8:02x}.{bdf % 8}",
                "written_dword": value, "config_hex": config.hex(),
            })


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=600, help="seconds to wait for host VM capacity")
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument("--chipset-reset", action="store_true", help="observe chipset reset config and PIIX3 command masks instead of the UART")
    parser.add_argument("--com2", action="store_true", help="include a second UART in the chipset inventory")
    parser.add_argument("--parallel", action="store_true", help="include a parallel port in the chipset inventory")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    def interrupted(number, _frame):
        raise RuntimeError(f"oracle interrupted by signal {number}")

    signal.signal(signal.SIGTERM, interrupted)
    result = {"captured_utc": datetime.datetime.now(datetime.timezone.utc).isoformat()}
    process = None
    error = None
    try:
        require_scope()
        result["qemu_version"] = subprocess.check_output([args.qemu, "--version"], text=True).strip()
        with permit(args.timeout), tempfile.TemporaryDirectory(prefix="pc-uart-", dir="/var/tmp") as temporary:
            directory = Path(temporary)
            qtest_path, uart_path = directory / "qtest.sock", directory / "uart.sock"
            argv = [args.qemu, "-machine", "pc,accel=qtest", "-nodefaults", "-display", "none", "-monitor", "none",
                    "-qtest", f"unix:{qtest_path},server=on,wait=off", "-qtest-log", "/dev/null",
                    "-chardev", f"socket,id=uart,path={uart_path},server=on,wait=off",
                    "-device", "isa-serial,chardev=uart,index=0"]
            if args.com2:
                argv.extend(["-chardev", "null,id=com2", "-device", "isa-serial,chardev=com2,index=1"])
            if args.parallel:
                argv.extend(["-parallel", "null"])
            result["argv"] = argv
            result["project_vm_pids_before_launch"] = project_vms()
            if len(result["project_vm_pids_before_launch"]) >= int(os.environ.get("VMLAB_HV_TEST_VM_LIMIT", "4")):
                raise RuntimeError("host VM capacity changed before launch")
            with (directory / "qemu.stderr").open("w+b") as stderr:
                try:
                    parent_pid = os.getpid()
                    library = ctypes.CDLL(None, use_errno=True)

                    def parent_death():
                        if library.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent_pid:
                            os._exit(127)

                    process = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=stderr,
                                               start_new_session=True, preexec_fn=parent_death)
                    with connect(qtest_path, process) as qtest_socket, connect(uart_path, process) as uart:
                        uart.setblocking(False)
                        if args.chipset_reset:
                            observe_chipset_reset(Qtest(qtest_socket), result)
                        else:
                            observe(Qtest(qtest_socket), uart, result)
                finally:
                    if process is not None:
                        if process.poll() is None:
                            os.killpg(process.pid, signal.SIGTERM)
                            try:
                                process.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
                    length = stderr.seek(0, os.SEEK_END)
                    stderr.seek(max(0, length - 8192))
                    result["qemu_stderr_tail"] = stderr.read().decode("utf-8", errors="replace")
    except Exception as exception:
        error = exception
        result["error"] = f"{type(exception).__name__}: {exception}"
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"output": str(args.output), "error": result.get("error"),
                      "unblocked_immediate_hex": result.get("tx_unblocked", {}).get("immediate_hex")}))
    if error is not None:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

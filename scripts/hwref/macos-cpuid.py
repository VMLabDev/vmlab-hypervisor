#!/usr/bin/env python3
"""Capture the macOS CPU model by executing CPUID in a Linux guest."""

import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tsc-khz", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    source = Path(__file__).with_name("capture.py")
    spec = importlib.util.spec_from_file_location("hwref_capture", source)
    capture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(capture)
    cpu = ("Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,"
           f"vmware-cpuid-freq=on,+invtsc,tsc-frequency={args.tsc_khz * 1000}")
    kernel = Path.home() / ".local/share/vmlab/guest/x86_64/vmlinuz"
    slots = Path(f"/tmp/vmlab-hv-vm-slots-{os.geteuid()}")
    slots.mkdir(exist_ok=True)
    deadline = time.monotonic() + 1800
    permit = None
    while permit is None:
        for slot in range(2):
            candidate = (slots / f"{slot}.lock").open("a+")
            try:
                fcntl.flock(candidate, fcntl.LOCK_EX | fcntl.LOCK_NB)
                permit = candidate
                break
            except BlockingIOError:
                candidate.close()
        if permit is None:
            if time.monotonic() >= deadline:
                raise TimeoutError("waiting for a shared VM permit")
            time.sleep(1)
    with permit, tempfile.TemporaryDirectory(prefix="macos-cpuid-") as tmp:
        directory = Path(tmp)
        program = directory / "init.c"
        program.write_text(r'''
#include <stdint.h>
#include <stdarg.h>
static long call(long n, long a, long b, long c, long d, long e) {
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8) : "rcx", "r11", "memory");
    return result;
}
static int console;
static void put(char c) { call(1, console, (long)&c, 1, 0, 0); }
static void printf(const char *format, ...) {
    va_list args; va_start(args, format);
    while (*format) {
        if (*format != '%') { put(*format++); continue; }
        ++format;
        unsigned width = 0;
        while (*format >= '0' && *format <= '9') width = width * 10 + *format++ - '0';
        if (*format++ != 'x') __builtin_trap();
        unsigned value = va_arg(args, unsigned);
        for (int i = width - 1; i >= 0; --i) put("0123456789abcdef"[(value >> (i * 4)) & 15]);
    }
    va_end(args);
}
#define main dump_cpuid
''' + capture.CPUID_C.replace("#include <stdio.h>", "") + r'''
#undef main
void _start(void) {
    call(83, (long)"/dev", 0755, 0, 0, 0);
    call(165, (long)"devtmpfs", (long)"/dev", (long)"devtmpfs", 0, 0);
    console = call(2, (long)"/dev/console", 1, 0, 0, 0);
    uint64_t affinity = 1;
    if (call(203, 0, sizeof(affinity), (long)&affinity, 0, 0) != 0) __builtin_trap();
    printf("CPUID-BEGIN\n"); dump_cpuid(); printf("CPUID-END\n");
    call(169, 0xfee1dead, 0x28121969, 0x4321fedc, 0, 0);
    for (;;) call(34, 0, 0, 0, 0, 0);
}
''')
        subprocess.run(["gcc", "-nostdlib", "-static", "-fno-builtin", "-fno-stack-protector",
                        "-march=x86-64", "-O2", "-o", str(directory / "init"), str(program)], check=True)
        with (directory / "initrd").open("wb") as archive:
            subprocess.run(["cpio", "-o", "-H", "newc"], input=b"init\n", cwd=directory,
                           stdout=archive, check=True)
        while True:
            count = 0
            for process in Path("/proc").glob("[0-9]*/cmdline"):
                try:
                    argv = process.read_bytes().split(b"\0")
                except (FileNotFoundError, ProcessLookupError):
                    continue
                name = os.path.basename(argv[0])
                count += (name.startswith(b"qemu-system-") and b"-avd" not in argv
                          or name == b"vmlab-hv" or name.startswith(b"vmlab-hv-test-"))
            if count < 2:
                break
            if time.monotonic() >= deadline:
                raise TimeoutError("waiting for fewer than two project VMs")
            time.sleep(1)
        print(f"Booting CPUID probe; {count} other project VM(s)", flush=True)
        command = ["systemd-run", "--user", "--scope", "-q", "-p", "MemoryMax=24G",
                   "-p", "MemorySwapMax=0", "--", "qemu-system-x86_64", "-machine", "q35",
                   "-accel", "kvm", "-cpu", cpu, "-smp", "4", "-m", "256M", "-nodefaults",
                   "-display", "none", "-serial", "stdio", "-no-reboot", "-kernel", str(kernel),
                   "-initrd", str(directory / "initrd"), "-append", "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init"]
        try:
            result = subprocess.run(command, capture_output=True, text=True, timeout=90)
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(f"CPUID probe timed out:\n{(error.stdout or b'')[-8000:].decode(errors='replace')}") from None
        if result.returncode or "CPUID-END" not in result.stdout:
            raise RuntimeError(f"CPUID probe failed: {result.stderr}\n{result.stdout[-4000:]}")
        payload = result.stdout.split("CPUID-BEGIN", 1)[1].split("CPUID-END", 1)[0]
        leaves = []
        for line in payload.splitlines():
            match = re.fullmatch(r"(0x[0-9a-f]+) (0x[0-9a-f]+): eax=(0x[0-9a-f]+) ebx=(0x[0-9a-f]+) ecx=(0x[0-9a-f]+) edx=(0x[0-9a-f]+)", line)
            if match:
                leaves.append(dict(zip(("function", "index", "eax", "ebx", "ecx", "edx"),
                                       (int(value, 16) for value in match.groups()))))
        if not leaves:
            raise RuntimeError("empty CPUID inventory")
        args.output.write_text(json.dumps({"cpu": cpu, "vcpus": 4, "tsc_khz": args.tsc_khz,
                                          "qemu": subprocess.check_output(["qemu-system-x86_64", "--version"], text=True).splitlines()[0],
                                          "kernel_sha256": hashlib.sha256(kernel.read_bytes()).hexdigest(),
                                          "leaves": leaves}, indent=2) + "\n")
        print(f"Captured {len(leaves)} CPUID entries to {args.output}")


if __name__ == "__main__":
    main()

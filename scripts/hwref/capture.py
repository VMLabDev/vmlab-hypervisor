#!/usr/bin/env python3
"""hwref — black-box reference capture of QEMU's guest-visible hardware.

Boots a small Alpine probe guest under /usr/bin/qemu-system-x86_64 with the
exact device sets vmlab generates for its shipped profiles (the argv mirrors
vmlab's src/qemu/cmdline.rs `build_args`, argument order included), runs a
collection script inside the guest, and writes the dumps under
docs/hwref/<config>/.

Clean room: QEMU is only ever run as a black box; nothing here reads QEMU
sources. vmlab's own sources are read — they are ours.

Usage:
    python3 scripts/hwref/capture.py                 # build assets + capture all
    python3 scripts/hwref/capture.py q35-seabios-linux-generic
    python3 scripts/hwref/capture.py --list
    python3 scripts/hwref/capture.py --summarize     # regenerate docs/hwref/tables.md

Host requirements: qemu-system-x86_64, qemu-img, /dev/kvm, gcc (static),
cpio, xorriso, mtools (mformat/mmd/mcopy), sfdisk, objcopy (binutils) plus
systemd's linuxx64.efi.stub for the OVMF UKI, swtpm, and network access to
dl-cdn.alpinelinux.org on first run (assets cache under .cache/).
"""

from __future__ import annotations

import base64
import gzip
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
CACHE = HERE / ".cache"
DOCS = HERE.parent.parent / "docs" / "hwref"

VMLAB = Path("/home/wil/dev/vmlab")
QEMU = "/usr/bin/qemu-system-x86_64"
QEMU_IMG = "/usr/bin/qemu-img"
KERNEL = VMLAB / "guest/dist/x86_64/vmlinuz"
FIRMWARE = VMLAB / "guest/dist/firmware/x86_64"
STUB = Path("/usr/lib/systemd/boot/efi/linuxx64.efi.stub")

# Alpine 3.22 — the same base as vmlab's shipped guest asset
# (guest/dist/x86_64/VERSION: "alpine=3.22 kernel=6.12.114-0-virt").
ALPINE_BASE = "https://dl-cdn.alpinelinux.org/alpine/v3.22"
DOWNLOADS = [
    f"{ALPINE_BASE}/releases/x86_64/alpine-minirootfs-3.22.7-x86_64.tar.gz",
    f"{ALPINE_BASE}/main/x86_64/pciutils-3.13.0-r1.apk",
    f"{ALPINE_BASE}/main/x86_64/pciutils-libs-3.13.0-r1.apk",
    f"{ALPINE_BASE}/main/x86_64/dmidecode-3.6-r0.apk",
    f"{ALPINE_BASE}/main/x86_64/iasl-20230628-r0.apk",
    f"{ALPINE_BASE}/main/x86_64/linux-virt-6.12.114-r0.apk",
]
KVER = "6.12.114-0-virt"

# Kernel modules shipped in the initramfs (deps pulled in via modules.dep;
# names are the .ko file names, which use dashes). linux-virt has no
# pcnet32/floppy/tpm drivers at all — those devices are captured through
# config space, ACPI and the PNP tree instead.
MODULES = [
    "qemu_fw_cfg",    # /sys/firmware/qemu_fw_cfg
    "efivarfs",       # /sys/firmware/efi/efivars (OVMF)
    "virtio_blk", "virtio_net",
    "e1000",
    "drm_kms_helper", "cirrus",
    "xhci-pci", "ehci-pci", "uhci-hcd", "usbhid", "usbmouse", "hid-generic",
    "psmouse", "evdev",
]

# Hyper-V enlightenments vmlab adds for hyperv=true profiles under KVM
# (cmdline.rs HYPERV_FLAGS + HYPERV_APICV_FLAG; this host's KVM virtualises
# the APIC — kvm_amd avic=Y — so hv-avic and the PIT global ride along,
# cmdline.rs:493-495).
CPU_HOST = "host,vmx=off,svm=off"
HYPERV = ("hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,"
          "hv-time,hv-synic,hv-stimer,hv-frequencies,hv-reset,hv-tlbflush,"
          "hv-ipi")
CPU_WIN11 = f"{CPU_HOST},{HYPERV},hv-avic"

BOOT_TIMEOUT = 420  # seconds for a whole VM lifecycle


# --------------------------------------------------------------------------
# Guest pieces
# --------------------------------------------------------------------------

INIT_SH = r"""#!/bin/sh
# hwref guest collection — runs as PID 1 from the initramfs.
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mkdir -p /proc /sys /dev /out
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mkdir -p /dev/pts
mount -t devpts devpts /dev/pts 2>/dev/null

if [ -f /etc/hwref-modules ]; then
  while IFS= read -r m; do
    [ -n "$m" ] || continue
    modprobe "$m" >>/out/modprobe.log 2>&1
  done < /etc/hwref-modules
fi
[ -d /sys/firmware/efi ] && mount -t efivarfs efivarfs /sys/firmware/efi/efivars 2>>/out/modprobe.log
sleep 2

cd /out
{ uname -a; cat /proc/version; cat /proc/cmdline; } > uname.txt 2>&1
cat /proc/cpuinfo > cpuinfo.txt 2>&1

# PCI: full config space, numeric IDs, topology
echo 'hwref: PCI' >/dev/console
lspci -nnvvvxxxx > lspci-nnnvvvxxxx.txt 2>&1
lspci -nn > lspci-nn.txt 2>&1
lspci -tv > lspci-tv.txt 2>&1
cat /proc/bus/pci/devices > proc-pci-devices.txt 2>&1

# ACPI tables: raw binaries, then iasl disassembly where the table carries AML
echo 'hwref: ACPI' >/dev/console
mkdir -p acpi
for t in /sys/firmware/acpi/tables/*; do
  [ -f "$t" ] && cp "$t" acpi/ 2>>acpi/copy-errors.log
done
for t in /sys/firmware/acpi/tables/dynamic/*; do
  [ -f "$t" ] && cp "$t" "acpi/dynamic-${t##*/}" 2>>acpi/copy-errors.log
done
mkdir -p acpi-dsl
for t in acpi/*; do
  [ -f "$t" ] || continue
  (cd acpi-dsl && iasl -d "../$t") >>acpi-dsl/iasl.log 2>&1 \
    || echo "iasl refused: $t" >>acpi-dsl/iasl.log
done

# SMBIOS
echo 'hwref: SMBIOS' >/dev/console
dmidecode > dmidecode.txt 2>&1
dmidecode --dump-bin smbios.bin >>dmidecode.txt 2>&1
mkdir -p dmi-tables
cp /sys/firmware/dmi/tables/* dmi-tables/ 2>/dev/null
{ for f in /sys/class/dmi/id/*; do
    printf '== %s ==\n' "$(basename "$f")"
    cat "$f" 2>/dev/null
  done
} > dmi-sysfs.txt 2>&1

# CPUID (tiny static dumper; Alpine does not package `cpuid`)
echo 'hwref: CPUID' >/dev/console
hwref-cpuid > cpuid.txt 2>&1 || echo "hwref-cpuid failed" > cpuid.txt

# Memory map, IRQ/MSI routing
dmesg > dmesg.txt 2>&1
grep -i e820 dmesg.txt > e820-dmesg.txt
{ for m in /sys/firmware/memmap/*; do
    printf '%s %s %s %s\n' "${m##*/}" "$(cat "$m/start" 2>/dev/null)" \
      "$(cat "$m/end" 2>/dev/null)" "$(cat "$m/type" 2>/dev/null)"
  done
} > e820-memmap.txt 2>&1
cat /proc/interrupts > interrupts.txt 2>&1
cat /proc/iomem > iomem.txt 2>&1
cat /proc/ioports > ioports.txt 2>&1

# fw_cfg file list (+ small files' contents)
echo 'hwref: fw_cfg' >/dev/console
mkdir -p fwcfg
{ cat /sys/firmware/qemu_fw_cfg/rev 2>/dev/null; echo
  ls -la /sys/firmware/qemu_fw_cfg/by_name/
} > fwcfg/listing.txt 2>&1
find /sys/firmware/qemu_fw_cfg/by_name -type l > fwcfg/files.txt
for entry in /sys/firmware/qemu_fw_cfg/by_key/*; do
  [ -f "$entry/name" ] || continue
  name=$(cat "$entry/name")
  size=$(cat "$entry/size")
  echo "$name $size" >>fwcfg/sizes.txt
  # Inventory every blob, but avoid port-I/O copies of large boot assets.
  [ "$size" -le 65536 ] || continue
  mkdir -p "fwcfg/blobs/$(dirname "$name")"
  timeout 5 cat "$entry/raw" >"fwcfg/blobs/$name" 2>>fwcfg/read-errors.log
done

# PNP/ISA devices
echo 'hwref: PNP' >/dev/console
{ for d in /sys/bus/pnp/devices/*; do
    echo "== ${d##*/} =="
    cat "$d/id" 2>/dev/null
    cat "$d/resources" 2>/dev/null
    cat "$d/options" 2>/dev/null
  done
} > pnp.txt 2>&1

# USB devices
{ lsusb 2>&1
  for u in /sys/bus/usb/devices/*; do
    [ -f "$u/idVendor" ] || continue
    echo "== $u $(cat "$u/manufacturer" 2>/dev/null) $(cat "$u/product" 2>/dev/null) [$(cat "$u/idVendor" 2>/dev/null):$(cat "$u/idProduct" 2>/dev/null)]"
  done
} > usb.txt 2>&1

# 16550 ports
cat /proc/tty/driver/serial > serial16550.txt 2>&1

# efivars (OVMF): listing + small variables
if [ -d /sys/firmware/efi/efivars ]; then
  mkdir -p efivars
  for v in /sys/firmware/efi/efivars/*; do
    n="${v##*/}"
    size=$(stat -c%s "$v" 2>/dev/null || echo x)
    echo "$n $size" >>efivars/sizes.txt
    case "$size" in ''|*[!0-9]*) continue;; esac
    [ "$size" -lt 2048 ] && od -An -tx1 "$v" >"efivars/$n.hex" 2>/dev/null
  done
fi

cd /
echo 'hwref: archive' >/dev/console
tar czf /out.tgz -C /out .
sync

OUT=$(sed -n 's/.*hwref.out=\([^ ]*\).*/\1/p' /proc/cmdline)
[ -n "$OUT" ] || OUT=/dev/console
dmesg -n 1 2>/dev/null
{ echo "===HWREF-BEGIN $(sha256sum /out.tgz 2>/dev/null | cut -d' ' -f1) $(stat -c%s /out.tgz)==="
  base64 /out.tgz
  echo "===HWREF-END==="
} >"$OUT" 2>/dev/null
sync
sleep 2
poweroff -f
sleep 60  # if poweroff failed, idle rather than panic
exec /bin/sh
"""

CPUID_C = r"""/* hwref-cpuid — raw CPUID leaf dump (stand-in for `cpuid -r`, which Alpine
 * does not package). Enumerates basic, hypervisor and extended leaves,
 * iterating subleaves for the ones defined to take ecx. */
#include <stdint.h>
#include <stdio.h>

static void cpuid(uint32_t leaf, uint32_t sub, uint32_t r[4]) {
    __asm__ volatile("cpuid"
                     : "=a"(r[0]), "=b"(r[1]), "=c"(r[2]), "=d"(r[3])
                     : "a"(leaf), "c"(sub));
}

static void emit(uint32_t leaf, uint32_t sub) {
    uint32_t r[4];
    cpuid(leaf, sub, r);
    printf("0x%08x 0x%02x: eax=0x%08x ebx=0x%08x ecx=0x%08x edx=0x%08x\n",
           leaf, sub, r[0], r[1], r[2], r[3]);
}

static void emit_sub(uint32_t leaf, int mode) {
    /* mode: 0 = until (eax & 0x1f) == 0 (cache enumerations)
             1 = subleaf 0's eax gives the maximum subleaf
             2 = until ebx == 0 (x2apic topology)                      */
    uint32_t maxsub = 63;
    for (uint32_t sub = 0; sub < 64; sub++) {
        uint32_t r[4];
        cpuid(leaf, sub, r);
        emit(leaf, sub);
        if (mode == 1 && sub == 0)
            maxsub = r[0];
        if ((mode == 0 && (r[0] & 0x1f) == 0) ||
            (mode == 1 && sub >= maxsub) ||
            (mode == 2 && r[1] == 0))
            break;
    }
}

static void emit_xsave(uint32_t leaf) {
    uint32_t r0[4], r1[4];
    cpuid(leaf, 0, r0);
    cpuid(leaf, 1, r1);
    emit(leaf, 0);
    emit(leaf, 1);
    uint64_t mask = ((uint64_t)(r0[3] | r1[3]) << 32) | r0[0] | r1[2];
    for (int b = 2; b < 64; b++)
        if (mask & (UINT64_C(1) << b))
            emit(leaf, b);
}

int main(void) {
    uint32_t r[4], maxb, maxe = 0, maxh = 0;

    printf("CPU 0 (hwref-cpuid raw dump)\n");
    cpuid(0, 0, r);
    maxb = r[0];
    cpuid(0x40000000, 0, r);
    if (r[0] >= 0x40000000 && r[0] < 0x50000000)
        maxh = r[0];
    cpuid(0x80000000, 0, r);
    if (r[0] >= 0x80000000)
        maxe = r[0];

    for (uint32_t leaf = 0; leaf <= maxb; leaf++) {
        switch (leaf) {
        case 0x04:
            emit_sub(leaf, 0); break;
        case 0x07: case 0x14: case 0x17: case 0x18:
            emit_sub(leaf, 1); break;
        case 0x0b: case 0x1f:
            emit_sub(leaf, 2); break;
        case 0x12:
            emit(leaf, 0);
            emit(leaf, 1);
            for (uint32_t sub = 2; sub < 64; sub++) {
                cpuid(leaf, sub, r);
                emit(leaf, sub);
                if ((r[0] & 0xf) == 0)
                    break;
            }
            break;
        case 0x0d:
            emit_xsave(leaf); break;
        default:
            emit(leaf, 0);
        }
    }
    for (uint32_t leaf = 0x40000000; maxh && leaf <= maxh; leaf++)
        emit(leaf, 0);
    for (uint32_t leaf = 0x80000000; maxe && leaf <= maxe; leaf++) {
        if (leaf == 0x8000001d) { emit_sub(leaf, 0); continue; }
        emit(leaf, 0);
    }
    return 0;
}
"""


# --------------------------------------------------------------------------
# Asset build (cached under scripts/hwref/.cache)
# --------------------------------------------------------------------------

def sh(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw)


def stamp_of(*paths: Path) -> str:
    h = hashlib.sha256()
    for p in paths:
        h.update(str(p).encode())
        h.update(str(p.stat().st_mtime_ns).encode() if p.exists() else b"-")
    return h.hexdigest()


class Assets:
    def __init__(self) -> None:
        self.dl = CACHE / "downloads"
        self.rootfs = CACHE / "rootfs"
        self.kmod = CACHE / "kmod"
        self.bin = CACHE / "bin"
        self.initrd = CACHE / "initramfs.cpio.gz"
        self.uki = CACHE / "bootx64.efi"
        self.esp = CACHE / "esp.raw"
        self.iso = CACHE / "cd0.iso"
        self.floppy = CACHE / "floppy.img"

    def ensure(self) -> None:
        if not STUB.exists():
            sys.exit(f"missing {STUB} (systemd-boot) — needed for the OVMF UKI")
        self.dl.mkdir(parents=True, exist_ok=True)
        self.bin.mkdir(parents=True, exist_ok=True)
        (CACHE / "run").mkdir(parents=True, exist_ok=True)
        for url in DOWNLOADS:
            dest = self.dl / url.rsplit("/", 1)[1]
            if not dest.exists():
                print(f"fetch {url}")
                subprocess.run(["curl", "-sSfLo", str(dest), url], check=True)
        stamp = CACHE / "rootfs.stamp"
        want = stamp_of(*sorted(self.dl.iterdir()), Path(__file__))
        if not (self.rootfs.exists() and stamp.exists() and stamp.read_text() == want):
            self.build_rootfs()
            stamp.write_text(want)
        self.build_cpuid()
        initrd_stamp = CACHE / "initrd.stamp"
        if (not self.initrd.exists()
                or not initrd_stamp.exists()
                or initrd_stamp.read_text() != stamp_of(self.rootfs)):
            self.build_initrd()
        uki_stamp = CACHE / "uki.stamp"
        if (not self.uki.exists() or not uki_stamp.exists()
                or uki_stamp.read_text() != stamp_of(self.initrd, KERNEL, STUB)):
            self.build_uki()
            uki_stamp.write_text(stamp_of(self.initrd, KERNEL, STUB))
        self.build_esp()
        if not self.iso.exists():
            self.build_iso()
        if not self.floppy.exists():
            self.build_floppy()

    def build_rootfs(self) -> None:
        print("building guest rootfs ...")
        if self.rootfs.exists():
            shutil.rmtree(self.rootfs)
        self.rootfs.mkdir(parents=True)
        for f in sorted(self.dl.iterdir()):
            if f.name.startswith("linux-virt"):
                continue
            subprocess.run(["tar", "xzf", str(f), "-C", str(self.rootfs)], check=True)
        if not self.kmod.exists():
            self.kmod.mkdir(parents=True)
            apk = self.dl / f"linux-virt-{KVER.replace('-0-virt', '-r0')}.apk"
            subprocess.run(["tar", "xzf", str(apk), "-C", str(self.kmod)], check=True)
        self._curate_modules()

    def _curate_modules(self) -> None:
        """Copy the wanted modules (+deps) into the rootfs, decompressed, with
        a rewritten modules.dep so busybox modprobe resolves them."""
        mdir = self.kmod / "lib" / "modules" / KVER
        dep: dict[str, list[str]] = {}
        for line in (mdir / "modules.dep").read_text().splitlines():
            if ":" not in line:
                continue
            path, _, deps = line.partition(":")

            def strip_gz(s: str, _p=path) -> str:
                return s[:-3] if s.endswith(".gz") else s
            dep[strip_gz(path.strip())] = [strip_gz(d) for d in deps.split()]

        def path_of(name: str) -> str | None:
            want = f"/{name}.ko"
            for p in dep:
                if p.endswith(want):
                    return p
            return None

        chosen: dict[str, None] = {}  # ordered set
        stack: list[str] = list(MODULES)
        while stack:
            item = stack.pop(0)
            if item.endswith(".ko"):      # a dependency path from modules.dep
                p = item
            else:                          # a module name we asked for
                want = f"/{item}.ko"
                p = next((q for q in dep if q.endswith(want)), None)
            if p is None:
                print(f"  module not found in linux-virt: {item}")
                continue
            if p in chosen:
                continue
            chosen[p] = None
            stack = dep.get(p, []) + stack

        out_mod = self.rootfs / "lib" / "modules" / KVER
        out_dep = []
        for p in chosen:
            src = mdir / (p + ".gz")
            dst = out_mod / p
            dst.parent.mkdir(parents=True, exist_ok=True)
            with gzip.open(src, "rb") as f:
                dst.write_bytes(f.read())
            out_dep.append(f"{p}: {' '.join(dep.get(p, []))}".rstrip())
        (out_mod / "modules.dep").write_text("\n".join(out_dep) + "\n")

    def build_cpuid(self) -> None:
        cpuid = self.bin / "hwref-cpuid"
        src = CACHE / "src"
        src.mkdir(parents=True, exist_ok=True)
        c = src / "hwref-cpuid.c"
        if not c.exists() or c.read_text() != CPUID_C:
            c.write_text(CPUID_C)
            if cpuid.exists():
                cpuid.unlink()
        if not cpuid.exists():
            print("building hwref-cpuid ...")
            sh(["gcc", "-O2", "-static", "-o", str(cpuid), str(c)])

    def build_initrd(self) -> None:
        print("building initramfs ...")
        shutil.copy2(self.bin / "hwref-cpuid", self.rootfs / "usr" / "bin" / "hwref-cpuid")
        (self.rootfs / "etc").mkdir(exist_ok=True)
        (self.rootfs / "etc" / "hwref-modules").write_text("\n".join(MODULES) + "\n")
        init = self.rootfs / "init"
        init.write_text(INIT_SH)
        init.chmod(0o755)
        proc = subprocess.run(
            f"cd {shlex.quote(str(self.rootfs))} && find . -print0"
            f" | cpio --null -o -H newc 2>/dev/null | gzip -9",
            shell=True, capture_output=True, check=True)
        self.initrd.write_bytes(proc.stdout)
        (CACHE / "initrd.stamp").write_text(stamp_of(self.rootfs))

    def build_uki(self) -> None:
        """UKI for the OVMF configs: systemd stub + kernel + initrd + cmdline
        (the manual objcopy layout from systemd's UKI docs)."""
        print("building UKI ...")
        cmdline = b"console=ttyS0 loglevel=7 panic=-1 reboot=t hwref.out=/dev/vport0p1\n"
        tmp = CACHE / "src"
        tmp.mkdir(parents=True, exist_ok=True)
        (tmp / "cmdline").write_bytes(cmdline)
        pe = STUB.read_bytes()
        pe_offset = struct.unpack_from("<I", pe, 0x3c)[0]
        image_base = struct.unpack_from("<Q", pe, pe_offset + 24 + 24)[0]
        sh(["objcopy",
            "--add-section", f".cmdline={tmp / 'cmdline'}",
            "--change-section-vma", f".cmdline={image_base + 0x30000:#x}",
            "--add-section", f".linux={KERNEL}",
            "--change-section-vma", f".linux={image_base + 0x2000000:#x}",
            "--add-section", f".initrd={self.initrd}",
            "--change-section-vma", f".initrd={image_base + 0x3000000:#x}",
            str(STUB), str(self.uki)])

    def build_esp(self) -> None:
        """Boot disk for the OVMF configs: MBR + one FAT32 partition holding
        EFI/BOOT/BOOTX64.EFI (the UKI). OVMF boots it through the standard
        removable-media fallback path, the way a vmlab VM boots its own ESP —
        no -kernel, so fw_cfg carries no extra boot files."""
        print("building ESP disk ...")
        size = 96 * 1024 * 1024
        with open(self.esp, "wb") as f:
            f.truncate(size)
        sh(["sfdisk", "--quiet", str(self.esp)], input="label: dos\nstart=2048, type=0x0c\n")
        img = f"{self.esp}@@1M"
        sh(["mformat", "-i", img, "-F", "-v", "HWREF", "::"])
        sh(["mmd", "-i", img, "::/EFI"])
        sh(["mmd", "-i", img, "::/EFI/BOOT"])
        sh(["mcopy", "-i", img, str(self.uki), "::/EFI/BOOT/BOOTX64.EFI"])

    def build_iso(self) -> None:
        """Non-bootable filler ISO for the legacy config's IDE CD-ROM."""
        print("building filler ISO ...")
        tmp = CACHE / "src" / "iso"
        tmp.mkdir(parents=True, exist_ok=True)
        (tmp / "hwref.txt").write_text("hwref filler CD-ROM (non-bootable)\n")
        sh(["xorriso", "-as", "mkisofs", "-quiet", "-volid", "HWREF",
            "-o", str(self.iso), str(tmp)])

    def build_floppy(self) -> None:
        print("building blank floppy ...")
        with open(self.floppy, "wb") as f:
            f.truncate(1440 * 1024)


# --------------------------------------------------------------------------
# QEMU argv builders — mirror vmlab src/qemu/cmdline.rs build_args(), order
# included. Probe-only additions (kernel/initrd for the SeaBIOS configs) are
# appended after -S, where vmlab appends qemu_args, and recorded as such.
# --------------------------------------------------------------------------

def arg(a: list[str], flag: str, value: str | None = None) -> None:
    a.append(f"-{flag}")
    if value is not None:
        a.append(value)


class RunPaths:
    def __init__(self, run: Path, out: Path, nics: int) -> None:
        self.run = run
        self.out = out
        self.agent_sock = run / "agent.sock"
        self.qmp_sock = run / "qmp.sock"
        self.vnc_sock = run / "vnc.sock"
        self.nic_sock = [run / f"nic{i}.sock" for i in range(nics)]
        self.swtpm_dir = run / "swtpm"
        self.swtpm_sock = run / "swtpm.sock"


def build_argv(cfg: dict, p: RunPaths, assets: Assets, run: Path, cfg_name: str) -> list[str]:
    a: list[str] = []
    arg(a, "name", f"vmlab:hwref/{cfg_name}")                    # cmdline.rs:439
    arg(a, "machine", cfg["machine"])                            # :448
    arg(a, "accel", "kvm")                                       # :471
    arg(a, "cpu", cfg["cpu"])                                    # :483
    arg(a, "smp", str(cfg["smp"]))                               # :484
    arg(a, "m", f"{cfg['mem']}M")                                # :485
    if cfg.get("rtc"):
        arg(a, "rtc", f"base={cfg['rtc']}")                      # :489-491
    if cfg.get("pit_global"):
        a.extend(["-global", "kvm-pit.lost_tick_policy=discard"])  # :493-495
    arg(a, "display", "none")                                    # :513
    arg(a, "vnc", f"unix:{p.vnc_sock},share=ignore")             # :523-527
    arg(a, "qmp", f"unix:{p.qmp_sock},server=on,wait=off")       # :529-533
    a.extend(["-monitor", "none"])                               # :534-535

    if cfg["agent"] == "isa":
        # serial log on COM2, agent channel on COM1 (:542-549, :607-613)
        arg(a, "chardev", f"file,id=serlog,path={p.out / 'serial.log'}")
        arg(a, "device", "isa-serial,chardev=serlog,index=1")
        arg(a, "chardev", f"socket,id=vagent0,path={p.agent_sock},server=on,wait=off")
        arg(a, "device", "isa-serial,chardev=vagent0,index=0")
    else:
        # serial log on COM1, agent channel on a virtio-serial port (:550, :599-605)
        arg(a, "serial", f"file:{p.out / 'serial.log'}")
        arg(a, "chardev", f"socket,id=vagent0,path={p.agent_sock},server=on,wait=off")
        arg(a, "device", "virtio-serial-pci")
        arg(a, "device", "virtserialport,chardev=vagent0,name=vmlab.agent.0")

    if "firmware" in cfg:                                        # :555-579
        code, vars_name, secure = cfg["firmware"]
        vars_path = run / "OVMF_VARS.fd"
        shutil.copy2(FIRMWARE / vars_name, vars_path)
        arg(a, "drive", f"if=pflash,format=raw,readonly=on,file={FIRMWARE / code}")
        arg(a, "drive", f"if=pflash,format=raw,file={vars_path}")
        if secure:
            a.extend(["-global", "driver=cfi.pflash01,property=secure,value=on"])

    # disks (:632-661): explicit blockdev node names, addressed slots
    disk0 = run / "disk0.qcow2"
    if cfg.get("ide_disk"):                                      # pc: (bus, unit) = (slot/2, slot%2)
        sh([QEMU_IMG, "create", "-q", "-f", "qcow2", str(disk0), "64M"])
        arg(a, "blockdev", f"driver=qcow2,node-name=disk0,file.driver=file,file.filename={disk0}")
        arg(a, "device", "ide-hd,drive=disk0,bus=ide.0,unit=0,bootindex=50")
    elif cfg["boot"] == "esp":                                   # OVMF: no bootindex
        sh([QEMU_IMG, "convert", "-q", "-O", "qcow2", str(assets.esp), str(disk0)])
        arg(a, "blockdev", f"driver=qcow2,node-name=disk0,file.driver=file,file.filename={disk0}")
        arg(a, "device", "virtio-blk-pci,drive=disk0")
    else:                                                        # SeaBIOS: bootindex 50+
        sh([QEMU_IMG, "create", "-q", "-f", "qcow2", str(disk0), "64M"])
        arg(a, "blockdev", f"driver=qcow2,node-name=disk0,file.driver=file,file.filename={disk0}")
        arg(a, "device", "virtio-blk-pci,drive=disk0,bootindex=50")

    if cfg.get("cdrom"):                                         # CD continues the slots (:680-707)
        cd = run / "cd0.iso"
        shutil.copy2(assets.iso, cd)
        arg(a, "blockdev", f"driver=raw,node-name=cd0,read-only=on,file.driver=file,file.filename={cd}")
        # Direct-kernel boot reserves index 0 on pc in QEMU 11.1.
        arg(a, "device", "ide-cd,drive=cd0,bus=ide.0,unit=1,bootindex=1,id=install-cd")

    if cfg.get("floppy"):                                        # :709-715
        fl = run / "floppy.img"
        shutil.copy2(assets.floppy, fl)
        arg(a, "drive", f"if=floppy,format=raw,file={fl}")

    for i, (model, mac) in enumerate(cfg["nics"]):               # :719-745
        arg(a, "netdev", f"stream,id=net{i},server=off,addr.type=unix,addr.path={p.nic_sock[i]}")
        arg(a, "device", f"{model},netdev=net{i},mac={mac}")
    if not cfg["nics"]:
        a.extend(["-nic", "none"])

    if cfg.get("tpm"):                                           # :753-766
        arg(a, "chardev", f"socket,id=chrtpm,path={p.swtpm_sock}")
        arg(a, "tpmdev", "emulator,id=tpm0,chardev=chrtpm")
        arg(a, "device", "tpm-tis,tpmdev=tpm0")

    arg(a, "device", cfg["display"])                             # :815-818
    a.extend(["-usb"])                                           # :859-865 (default input)
    if cfg.get("usb"):
        arg(a, "device", "qemu-xhci,id=usbx")
    arg(a, "device", "usb-tablet")

    a.extend(["-S"])                                             # :884

    if cfg["boot"] == "kernel":                                  # probe-only, appended last
        out = "/dev/ttyS1" if cfg["stream"] == "serial2" else "/dev/vport0p1"
        a.extend(["-kernel", str(KERNEL),
                  "-initrd", str(assets.initrd),
                  "-append", f"console=ttyS0 loglevel=7 panic=-1 reboot=t hwref.out={out}"])
    return a


CONFIGS: dict[str, dict] = {
    # (a) linux-generic: q35 + SeaBIOS + virtio — vmlab's conservative Linux
    # profile. Default input devices: `-usb` (the ICH9 UHCI/EHCI set on q35)
    # plus usb-tablet. The requested capture also sets usb=true for xHCI.
    "q35-seabios-linux-generic": {
        "title": "q35 + SeaBIOS, linux-generic profile devices",
        "profile": "linux-generic with usb = true",
        "machine": "q35", "smp": 2, "mem": 2048, "cpu": CPU_HOST,
        "agent": "virtio",
        "nics": [("virtio-net-pci", "52:54:00:00:00:01")],
        "display": "VGA",
        "usb": True,
        "boot": "kernel",
        "stream": "agent",
    },
    # (b) Windows 11 device set without secure boot (the windows-server
    # pairing): q35 + OVMF + swtpm tpm-tis + virtio + virtio-vga + Hyper-V
    # enlightenments (host KVM has AVIC, so hv-avic and the PIT global ride).
    "q35-ovmf-win11": {
        "title": "q35 + OVMF, Windows 11 profile devices, TPM (no secure boot)",
        "profile": "windows-server (the windows-11 device set, secure boot off)",
        "machine": "q35", "smp": 4, "mem": 8192, "cpu": CPU_WIN11,
        "pit_global": True, "agent": "virtio",
        "firmware": ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd", False),
        "tpm": True,
        "nics": [("virtio-net-pci", "52:54:00:00:00:01")],
        "display": "virtio-vga",
        "boot": "esp",
        "stream": "agent",
    },
    # (c) The same with secure boot: secboot CODE pflash. Booted with the
    # blank VARS (Pairing::Setup) because the enforcing .ms VARS refuses
    # every unsigned image — see this config's README for the deviation.
    "q35-ovmf-win11-secureboot": {
        "title": "q35 + OVMF secure boot, Windows 11 profile devices, TPM",
        "profile": "windows-11",
        "machine": "q35", "smp": 4, "mem": 8192, "cpu": CPU_WIN11,
        "pit_global": True, "agent": "virtio",
        "firmware": ("OVMF_CODE_4M.secboot.fd", "OVMF_VARS_4M.fd", True),
        "tpm": True,
        "nics": [("virtio-net-pci", "52:54:00:00:00:01")],
        "display": "virtio-vga",
        "boot": "esp",
        "stream": "agent",
    },
    # (d) Legacy PC superset (windows-9x base plus the e1000/floppy of its
    # siblings): i440fx + SeaBIOS, IDE disk + IDE CD, floppy, e1000 + pcnet,
    # cirrus-vga, isa-serial agent on COM1 and serial log on COM2, RTC at
    # local time.
    "pc-seabios-legacy": {
        "title": "pc (i440fx) + SeaBIOS, legacy profile device superset",
        "profile": "windows-9x + windows-xp/windows-legacy superset",
        "machine": "pc", "smp": 1, "mem": 256, "cpu": CPU_HOST,
        "rtc": "localtime", "agent": "isa",
        "nics": [("e1000", "52:54:00:00:00:01"), ("pcnet", "52:54:00:00:00:02")],
        "display": "cirrus-vga",
        "ide_disk": True, "cdrom": True, "floppy": True,
        "boot": "kernel",
        "stream": "serial2",
    },
}


# --------------------------------------------------------------------------
# Runtime helpers
# --------------------------------------------------------------------------

class NicSink(threading.Thread):
    """Stands in for vmlab's switch: listens where the stream netdevs connect."""

    def __init__(self, path: Path) -> None:
        super().__init__(daemon=True)
        self.path = path
        if path.exists():
            path.unlink()
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(str(path))
        self.srv.listen(4)
        self.stop = False

    def run(self) -> None:
        self.srv.settimeout(0.5)
        while not self.stop:
            try:
                conn, _ = self.srv.accept()
            except (socket.timeout, OSError):
                continue
            conn.settimeout(0.5)
            try:
                while not self.stop:
                    if not conn.recv(65536):
                        break
            except OSError:
                pass
            conn.close()

    def close(self) -> None:
        self.stop = True
        self.srv.close()


class ChanReader(threading.Thread):
    """Reads QEMU's agent chardev socket (server=on,wait=off) from before
    `cont` until the guest powers off, so nothing written to the port drops."""

    def __init__(self, path: Path) -> None:
        super().__init__(daemon=True)
        self.path = path
        self.data = bytearray()
        self.connected = False

    def run(self) -> None:
        deadline = time.time() + 30
        s = None
        while time.time() < deadline:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(2)
                s.connect(str(self.path))
                break
            except OSError:
                time.sleep(0.1)
        if s is None:
            return
        self.connected = True
        s.settimeout(1.0)
        while True:
            try:
                chunk = s.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            self.data += chunk


def qmp_cont(sock: Path, log) -> None:
    """The two commands vmlab's daemon sends at startup: capabilities + cont."""
    deadline = time.time() + 20
    last: Exception | None = None
    while time.time() < deadline:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(2)
            s.connect(str(sock))
            break
        except OSError as e:
            last = e
            time.sleep(0.1)
    else:
        raise RuntimeError(f"QMP socket never came up: {last}")
    s.settimeout(10)
    f = s.makefile("rwb")
    log.write(f.readline())  # greeting
    for cmd in ("qmp_capabilities", "cont"):
        f.write(json.dumps({"execute": cmd}).encode() + b"\n")
        f.flush()
        while True:
            line = f.readline()
            if not line:
                raise RuntimeError(f"QMP closed during {cmd}")
            log.write(line)
            msg = json.loads(line)
            if "return" in msg or "error" in msg:
                if "error" in msg:
                    raise RuntimeError(f"QMP {cmd}: {msg['error']}")
                break
    s.close()


def decode_stream(raw: bytes) -> bytes | None:
    text = raw.decode("utf-8", "replace").replace("\r\n", "\n")
    m = re.search(r"===HWREF-BEGIN ([0-9a-f]+) (\d+)===\n(.*?)===HWREF-END===", text, re.S)
    if not m:
        return None
    sha, size, body = m.group(1), int(m.group(2)), m.group(3)
    keep = [l.strip() for l in body.splitlines()
            if re.fullmatch(r"[A-Za-z0-9+/]+={0,2}", l.strip())]
    try:
        data = base64.b64decode("".join(keep))
    except Exception:
        return None
    if len(data) != size or hashlib.sha256(data).hexdigest() != sha:
        return None
    return data


def run_capture(cfg_name: str, cfg: dict, assets: Assets) -> bool:
    out_dir = DOCS / cfg_name
    out_dir.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="hwref-"))
    p = RunPaths(run, out_dir, len(cfg["nics"]))
    print(f"\n=== {cfg_name}: {cfg['title']} ===")
    ok = False
    reason = ""
    swtpm_proc = None
    sinks: list[NicSink] = []
    reader = None
    proc = None
    try:
        argv = build_argv(cfg, p, assets, run, cfg_name)
        probe_note = (
            "# -kernel/-initrd/-append are this capture's probe boot; vmlab\n"
            "# appends qemu_args in this position and boots a real guest disk.\n"
            if cfg["boot"] == "kernel" else
            "# boots from disk0.qcow2 (a FAT32 ESP carrying an unsigned UKI\n"
            "# at \\EFI\\BOOT\\BOOTX64.EFI) — no probe additions to the argv.\n")
        (out_dir / "qemu-argv.txt").write_text(
            "# QEMU argv (argv[0] then one argument per line), a mirror of\n"
            "# vmlab's src/qemu/cmdline.rs build_args() for this profile shape.\n"
            + "\n".join([QEMU] + argv) + "\n\n" + probe_note
            + "\n# one line:\n" + shlex.join([QEMU] + argv) + "\n")
        sinks = [NicSink(s) for s in p.nic_sock]
        for s in sinks:
            s.start()
        if cfg.get("tpm"):
            p.swtpm_dir.mkdir(parents=True, exist_ok=True)
            swtpm_argv = ["swtpm", "socket", "--tpm2",
                          f"--tpmstate=dir={p.swtpm_dir}",
                          f"--ctrl=type=unixio,path={p.swtpm_sock}",
                          "--terminate"]
            swtpm_log = open(out_dir / "swtpm.log", "wb")
            swtpm_proc = subprocess.Popen(swtpm_argv, stdout=swtpm_log,
                                          stderr=subprocess.STDOUT)
            (out_dir / "swtpm-argv.txt").write_text(shlex.join(swtpm_argv) + "\n")
            time.sleep(1.5)  # let it bind the control socket
        qemu_log = open(out_dir / "qemu-stderr.log", "wb")
        proc = subprocess.Popen([QEMU] + argv, stdout=qemu_log, stderr=subprocess.STDOUT)
        reader = ChanReader(p.agent_sock)
        reader.start()
        with open(out_dir / "qmp.log", "wb") as qmp_log:
            qmp_cont(p.qmp_sock, qmp_log)
        deadline = time.time() + BOOT_TIMEOUT
        while time.time() < deadline and proc.poll() is None:
            time.sleep(0.5)
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(10)
            except subprocess.TimeoutExpired:
                proc.kill()
            reason = f"timed out after {BOOT_TIMEOUT}s"
        elif proc.returncode != 0:
            reason = f"qemu exited {proc.returncode}"
        reader.join(timeout=15)
        (out_dir / "agent-channel.log").write_bytes(bytes(reader.data))

        raw = ((out_dir / "serial.log").read_bytes()
               if cfg["stream"] == "serial2" and (out_dir / "serial.log").exists()
               else bytes(reader.data))
        data = decode_stream(raw)
        if data is None:
            reason = reason or "no valid HWREF stream (marker missing or checksum bad)"
        else:
            guest = out_dir / "guest"
            if guest.exists():
                shutil.rmtree(guest)
            guest.mkdir()
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as t:
                t.extractall(guest, filter="data")
            if not parse_pci(guest):
                raise RuntimeError("guest lspci did not report any PCI functions")
            if not (guest / "smbios.bin").stat().st_size:
                raise RuntimeError("guest SMBIOS dump is empty")
            if not (guest / "fwcfg/sizes.txt").exists():
                raise RuntimeError("guest fw_cfg inventory is missing")
            ok = not reason

        (out_dir / "qemu-version.txt").write_text(
            subprocess.run([QEMU, "--version"], capture_output=True, text=True).stdout)
        env = [f"host kernel: {os.uname().release}",
               "host cpu: " + next(l.split(":", 1)[1].strip()
                                   for l in Path("/proc/cpuinfo").read_text().splitlines()
                                   if l.startswith("model name")),
               "kvm_amd avic: " + Path("/sys/module/kvm_amd/parameters/avic").read_text().strip(),
               "kvm_amd nested: " + Path("/sys/module/kvm_amd/parameters/nested").read_text().strip()]
        for asset in [KERNEL, assets.initrd, STUB, *sorted(assets.dl.iterdir())]:
            env.append(f"asset: {asset.name} (sha256 {hashlib.sha256(asset.read_bytes()).hexdigest()})")
        for source in [VMLAB / "src/qemu/cmdline.rs", VMLAB / "src/profiles/shipped.wcl"]:
            env.append(f"vmlab source: {source} (sha256 {hashlib.sha256(source.read_bytes()).hexdigest()})")
        if "firmware" in cfg:
            code, vars_name, _ = cfg["firmware"]
            fw = FIRMWARE / code
            env.append(f"firmware CODE: {code} (sha256 {hashlib.sha256(fw.read_bytes()).hexdigest()})")
            env.append(f"firmware VARS: {vars_name} (sha256 "
                       f"{hashlib.sha256((FIRMWARE / vars_name).read_bytes()).hexdigest()})")
            if cfg_name.endswith("secureboot"):
                ms = FIRMWARE / "OVMF_VARS_4M.ms.fd"
                env.append(f"enforcing VARS (NOT booted, refuses unsigned probes): "
                           f"OVMF_VARS_4M.ms.fd (sha256 {hashlib.sha256(ms.read_bytes()).hexdigest()})")
        (out_dir / "environment.txt").write_text("\n".join(env) + "\n")
    except Exception as e:  # noqa: BLE001 — capture failures are reported, not raised
        reason = reason or f"{type(e).__name__}: {e}"
        import traceback
        traceback.print_exc()
    finally:
        if proc and proc.poll() is None:
            proc.kill()
            proc.wait()
        for s in sinks:
            s.close()
        if swtpm_proc and swtpm_proc.poll() is None:
            swtpm_proc.terminate()
        shutil.rmtree(run, ignore_errors=True)
    write_config_readme(cfg_name, cfg, ok, reason)
    print(f"{'OK  ' if ok else 'FAIL'} {cfg_name}" + ("" if ok else f" — {reason}"))
    return ok


def write_config_readme(cfg_name: str, cfg: dict, ok: bool, reason: str) -> None:
    notes = [f"# {cfg_name}", "", f"{cfg['title']}.", "",
             f"Base profile: `{cfg['profile']}`. The exact QEMU argv is in "
             "`qemu-argv.txt`; host facts (QEMU version, CPU, KVM parameters, "
             "firmware hashes) in `environment.txt`.", ""]
    notes += [
        "## Contents", "",
        "`guest/` — collected inside the guest:",
        "- `lspci-nnnvvvxxxx.txt` — every PCI function, full config space",
        "- `lspci-nn.txt`, `lspci-tv.txt`, `proc-pci-devices.txt` — PCI IDs and topology",
        "- `acpi/` — every ACPI table from /sys/firmware/acpi/tables (raw,",
        "  `dynamic-`-prefixed ones from `tables/dynamic/`), plus iasl output;",
        "  `acpi-dsl/iasl.log` records disassembly diagnostics",
        "- `dmidecode.txt`, `smbios.bin`, `dmi-tables/`, `dmi-sysfs.txt` — SMBIOS",
        "- `cpuid.txt` — raw CPUID leaves (hwref-cpuid; Alpine packages no `cpuid`)",
        "- `dmesg.txt`, `e820-dmesg.txt`, `e820-memmap.txt` — boot log and e820 map",
        "- `interrupts.txt`, `iomem.txt`, `ioports.txt` — IRQ/MSI and address maps",
        "- `fwcfg/` — fw_cfg revision and by_name file list (+ small files)",
        "- `pnp.txt` — PNP/ISA devices from /sys/bus/pnp",
        "- `usb.txt`, `serial16550.txt`, `cpuinfo.txt`, `uname.txt`, `modprobe.log`",
    ]
    if "firmware" in cfg:
        notes.append("- `efivars/` — UEFI variable listing and small variables")
    notes += [
        "",
        "Boot/console artefacts: `serial.log` (COM1 console: kernel under SeaBIOS,",
        "OVMF's own serial console under UEFI), `agent-channel.log` (the stream on",
        "the `vmlab.agent.0` chardev — the tarball rode it where marked),",
        "`qmp.log`, `qemu-argv.txt`, `qemu-version.txt`, `environment.txt`,",
        "`qemu-stderr.log`" + (", `swtpm-argv.txt`/`swtpm.log`" if cfg.get("tpm") else "") + ".",
        "",
        "## Probe deviations", "",
    ]
    if cfg["boot"] == "kernel":
        notes += [
            "- Boots vmlab's own container kernel (`vmlinuz`, Alpine linux-virt",
            "  6.12.114) with an initramfs built from the Alpine minirootfs plus",
            "  pciutils/dmidecode/iasl, via `-kernel`. vmlab boots a real guest disk;",
            "  the device set is identical, but fw_cfg gains the `etc/boot-kernel`,",
            "  `etc/boot-initrd`, `etc/boot-cmdline-size` and `etc/boot-cmdline` files",
            "  (see `guest/fwcfg/sizes.txt`). The blank guest disk and the filler CD",
            "  preserve disk bootindex 50. The legacy CD uses index 1 instead of",
            "  vmlab's 0 because QEMU direct-kernel boot reserves index 0.",
        ]
    else:
        notes += [
            "- Boots as a real UEFI disk boot: disk0.qcow2 carries an MBR + FAT32",
            "  partition with an unsigned UKI (systemd stub + the same kernel and",
            "  initramfs) at `\\EFI\\BOOT\\BOOTX64.EFI`. No `-kernel`, so the fw_cfg",
            "  file list has no direct-kernel probe additions; the guest is Linux,",
            "  not Windows.",
        ]
    if cfg_name == "q35-ovmf-win11-secureboot":
        notes += [
            "- **Secure boot is captured in setup mode, not enforcing.** The booted",
            "  VARS is the blank `OVMF_VARS_4M.fd` (vmlab's Pairing::Setup); the",
            "  enforcing `OVMF_VARS_4M.ms.fd` (Pairing::Enrolled — what",
            "  `secure_boot = true` actually boots) refuses every unsigned image,",
            "  including this probe. An enforcing capture needs a signed probe. The",
            "  secboot CODE pflash, the `cfi.pflash01,secure=on` global, SMM and the",
            "  firmware configuration are recorded here; enforcement parity is not",
            "  proven (`SecureBoot` is 0 here, 1 under the ms VARS).",
            "  The ms VARS sha256 is in `environment.txt`.",
        ]
    if cfg_name == "pc-seabios-legacy":
        notes += [
            "- The device set is a superset across the legacy profiles (windows-9x",
            "  cirrus/pcnet/IDE/isa-serial/RTC-localtime plus windows-xp/legacy's",
            "  e1000 and the answer-file floppy). Extra devices change PCI slots;",
            "  this is a superset reference, not each profile's exact topology.",
            "- linux-virt has no pcnet32, floppy or TPM drivers, so those devices",
            "  appear without a bound driver (no /dev/fd0, no pcnet in",
            "  /proc/interrupts). Their guest-visible shape — PCI config space,",
            "  DSDT nodes and the PNP0700/0501 PNP devices — is fully captured.",
        ]
    notes.append("")
    if not ok:
        tail = ""
        sl = DOCS / cfg_name / "serial.log"
        if sl.exists():
            tail = sl.read_text(errors="replace")[-2000:]
        notes += [f"**CAPTURE FAILED**: {reason}", "", "Captured console tail:", "```",
                  tail or "(no serial output)", "```"]
    (DOCS / cfg_name / "README.md").write_text("\n".join(notes) + "\n")


# --------------------------------------------------------------------------
# Summary tables (docs/hwref/tables.md, included by docs/hwref/README.md)
# --------------------------------------------------------------------------

def parse_pci(cfg_dir: Path) -> list[dict]:
    rows = []
    address = r"(?:[0-9a-f]{4}:)?[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]"
    nn = (cfg_dir / "lspci-nn.txt").read_text().splitlines()
    vv = (cfg_dir / "lspci-nnnvvvxxxx.txt").read_text()
    for line in nn:
        m = re.match(rf"^({address}) (.+) \[([0-9a-f]{{4}}):([0-9a-f]{{4}})\]", line)
        if not m:
            continue
        bdf, name, vend, dev = m.groups()
        cls = re.search(r"\[([0-9a-f]{4})\]", name)
        block = re.search(rf"^{re.escape(bdf)} [^\n]*(?:\n|\Z).*?(?=^{address} |\Z)", vv, re.M | re.S)
        sub, bars = "—", "none"
        if block:
            b = block.group(0)
            s = re.search(r"Subsystem: (.+?)(?: \[([0-9a-f:]+)\])?$", b, re.M)
            if s:
                sub = (f"{s.group(2)} " if s.group(2) else "") + (s.group(1) or "")
            barlist = re.findall(r"^[ \t]*(?:Region \d+: )?(?:\[virtual\] )?((?:Memory at |I/O ports at |Expansion ROM at )[^\n]+)", b, re.M)
            bars = "; ".join(re.sub(r"\s+", " ", x).strip() for x in barlist) or "none"
        clean = re.sub(r"\s*\[[0-9a-f:]+\]", "", name).replace(" :", ":").strip(" :")
        rows.append({"bdf": bdf, "id": f"{vend}:{dev}",
                     "class": cls.group(1) if cls else "?", "name": clean,
                     "sub": sub, "bars": bars})
    return rows


def parse_acpi(cfg_dir: Path) -> list[dict]:
    rows = []
    directory = cfg_dir / "acpi"
    if not directory.exists():
        return rows
    for f in sorted(directory.iterdir()):
        if not f.is_file() or not re.fullmatch(r"(?:dynamic-)?[A-Z0-9_]{4}\d*", f.name):
            continue
        raw = f.read_bytes()
        if raw[:4] == b"FACS":
            if len(raw) < 64:
                raise ValueError(f"{f}: truncated FACS")
            length = struct.unpack_from("<I", raw, 4)[0]
            if length < 64 or length > len(raw):
                raise ValueError(f"{f}: invalid FACS length {length}")
            rows.append({"name": f.name, "tblid": "—", "len": length,
                         "rev": raw[32], "oem": "—", "oemrev": "—",
                         "creator": "—", "crev": "—"})
            continue
        if len(raw) < 36:
            raise ValueError(f"{f}: truncated ACPI header")
        sig, length, rev, _csum, oem_id, oem_tbl, oem_rev, creator, creator_rev = \
            struct.unpack("<4sIBB6s8sI4sI", raw[:36])
        if not re.fullmatch(rb"[A-Z0-9_]{4}", sig):
            raise ValueError(f"{f}: invalid ACPI signature {sig!r}")
        if length < 36 or length > len(raw):
            raise ValueError(f"{f}: invalid ACPI length {length}")
        rows.append({"name": f.name, "tblid": oem_tbl.decode("ascii", "replace").rstrip(),
                     "len": length, "rev": rev,
                     "oem": oem_id.decode("ascii", "replace"),
                     "oemrev": oem_rev,
                     "creator": creator.decode("ascii", "replace"),
                     "crev": creator_rev})
    return rows


def smbios_str(table: bytes, off: int, idx: int) -> str:
    if idx == 0:
        return ""
    pos, cur = off, 1
    while pos < len(table):
        end = table.find(b"\x00", pos)
        if end < 0:
            raise ValueError("unterminated SMBIOS string")
        if cur == idx:
            return table[pos:end].decode("utf-8", "replace")
        if end == pos:  # empty string = end of the string set
            break
        pos = end + 1
        cur += 1
    return ""


def parse_smbios(cfg_dir: Path) -> dict:
    """Parse the structure table from dmi-tables/DMI (what dmidecode reads)."""
    f = cfg_dir / "dmi-tables" / "DMI"
    if not f.exists():
        return {}
    table = f.read_bytes()
    out: dict = {}
    pos = 0
    while pos < len(table):
        if pos + 4 > len(table):
            raise ValueError(f"{f}: truncated SMBIOS header at {pos}")
        typ, length = table[pos], table[pos + 1]
        end = pos + length
        if length < 4 or end > len(table):
            raise ValueError(f"{f}: invalid SMBIOS length {length} at {pos}")
        strings_end = table.find(b"\x00\x00", end)
        if strings_end < 0:
            raise ValueError(f"{f}: unterminated SMBIOS string set at {pos}")
        strings = table[end:strings_end + 2]
        if typ == 0:
            if length < 9:
                raise ValueError(f"{f}: short SMBIOS type 0 at {pos}")
            out["bios_vendor"] = smbios_str(strings, 0, table[pos + 4])
            out["bios_version"] = smbios_str(strings, 0, table[pos + 5])
            out["bios_date"] = smbios_str(strings, 0, table[pos + 8])
        elif typ == 1:
            if length < 8:
                raise ValueError(f"{f}: short SMBIOS type 1 at {pos}")
            for key, offset in (("manufacturer", 4), ("product", 5),
                                ("version", 6), ("serial", 7),
                                ("sku", 25), ("family", 26)):
                if offset < length:
                    out[key] = smbios_str(strings, 0, table[pos + offset])
            if length >= 24:
                out["uuid"] = str(uuid.UUID(bytes_le=table[pos + 8:pos + 24]))
        if typ == 127:
            break
        pos = strings_end + 2
    return out


def summarize() -> None:
    lines = ["<!-- Generated by scripts/hwref/capture.py --summarize; do not "
             "edit by hand. -->", ""]
    for name, cfg in CONFIGS.items():
        d = DOCS / name / "guest"
        if not (d / "lspci-nn.txt").exists():
            continue
        lines += [f"## {name}", "", cfg["title"], "",
                  "### PCI functions", "",
                  "| BDF | vendor:device | class | device | subsystem | BARs |",
                  "|-----|--------------|-------|--------|-----------|------|"]
        for r in parse_pci(d):
            lines.append(f"| {r['bdf']} | `{r['id']}` | {r['class']} | {r['name']} "
                         f"| {r['sub']} | {r['bars']} |")
        lines += ["", "### ACPI tables", "",
                  "| table | OEM table ID | bytes | rev | OEM ID | OEM rev | creator | creator rev |",
                  "|-------|--------------|-------|-----|--------|---------|---------|-------------|"]
        for t in parse_acpi(d):
            lines.append(f"| {t['name']} | {t['tblid']} | {t['len']} | {t['rev']} | "
                         f"{t['oem']} | {t['oemrev']} | {t['creator']} | {t['crev']} |")
        sm = parse_smbios(d)
        if sm:
            lines += ["", "### SMBIOS type 0/1", "",
                      f"- BIOS: vendor `{sm.get('bios_vendor')}`, version "
                      f"`{sm.get('bios_version')}`, date `{sm.get('bios_date')}`",
                      f"- System: manufacturer `{sm.get('manufacturer')}`, product "
                      f"`{sm.get('product')}`, version `{sm.get('version')}`, serial "
                      f"`{sm.get('serial')}`, SKU `{sm.get('sku')}`, family `{sm.get('family')}`",
                      f"- UUID: `{sm.get('uuid')}`"]
        lines.append("")
    DOCS.mkdir(parents=True, exist_ok=True)
    (DOCS / "tables.md").write_text("\n".join(lines))
    versions = sorted({p.read_text().splitlines()[0]
                       for p in DOCS.glob("*/qemu-version.txt")})
    intro = """# QEMU hardware references

Black-box guest observations for D30 hardware parity and the D26 differential suite.
QEMU source code is not used. The argv follows vmlab's `src/qemu/cmdline.rs`
and `src/profiles/shipped.wcl` at capture time.

## Recapture

Run `uv run scripts/hwref/capture.py` from the repository root.
Pass a configuration name to recapture one; `--list` lists configurations.
Run `uv run scripts/hwref/capture.py --summarize` to regenerate these tables.
Requirements and asset preparation are in `scripts/hwref/README.md`.
Existing results for the selected configuration are replaced.

## Capture contents and limits

Each configuration directory records exact argv, version, host CPU and firmware
hashes, console logs, and a `guest/` directory. The guest records full PCI config
space and topology, raw ACPI tables and iasl output, SMBIOS text and binaries,
CPUID leaves, e820, IRQs, fw_cfg inventory, and PNP resources.
Per-configuration READMEs describe probe deviations and capture failures.

The Linux capture uses `linux-generic` with `usb = true` to include xHCI.
The legacy capture combines devices from several profiles; its slots do not
represent every individual profile. Secure-boot OVMF uses blank VARS in setup
mode because the probe is unsigned. Enforcing secure boot remains uncaptured.
CPUID depends on the host, and IRQ counts, timestamps and dynamic tables vary.
These are Linux-initialized observations, not reset-state device registers.
This package provides QEMU reference data; it does not change S1 or S2 behaviour.

## QEMU version

"""
    (DOCS / "README.md").write_text(intro + "\n".join(versions) + "\n\n" + "\n".join(lines))
    print(f"wrote {DOCS / 'tables.md'}")


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "--list":
        for name, cfg in CONFIGS.items():
            print(f"{name:32} {cfg['title']}")
        return 0
    if args and args[0] == "--summarize":
        summarize()
        return 0
    assets = Assets()
    assets.ensure()
    wanted = args or list(CONFIGS)
    bad = [w for w in wanted if w not in CONFIGS]
    if bad:
        print(f"unknown config(s): {bad}", file=sys.stderr)
        return 2
    failed = [c for c in wanted if not run_capture(c, CONFIGS[c], assets)]
    summarize()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

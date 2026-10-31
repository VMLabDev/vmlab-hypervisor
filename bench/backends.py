"""The two D2 backends: `qemu` (vmlab's exact argv, notes/03) and `vmlab-hv`.

Both build a fresh-overlay machine from the local vmlab templates, launch one
process under the systemd-run memory cap and a `taskset` pin, release the
vCPUs the way vmlab does, and expose the guest agent on a unix socket. The
suite times the same three workloads on either.

The `vmlab-hv` backend is wired end to end, but until phase 3 lands
foreign-arch `accel: tcg` support, `vmlab-hv` exits at launch with "is not
supported" (builder::validate); the backend reports that as a skip with the
error text as the reason, and the runner records it instead of results.
"""

import json
import os
import signal
import subprocess
import time

import lib


class VmSession:
    """One launched VM process plus the discipline that reaps it.

    Every VM runs as its own process-group leader inside the memory-cap
    scope, so a group kill reaps the whole tree and nothing outlives a run.
    """

    def __init__(self, vm_argv, run_dir, log):
        self.run_dir = run_dir
        self.log = log
        self.stderr_path = os.path.join(run_dir, "stderr.log")
        self.stderr_file = open(self.stderr_path, "wb")
        argv = lib.systemd_run_prefix() + vm_argv
        self.log(f"launching: {' '.join(argv)}")
        self.t0 = time.monotonic()
        self.proc = subprocess.Popen(
            argv,
            stdout=subprocess.DEVNULL,
            stderr=self.stderr_file,
            start_new_session=True,
        )

    def exited(self):
        return self.proc.poll() is not None

    def exit_code(self):
        return self.proc.returncode

    def stderr_text(self):
        try:
            self.stderr_file.flush()
            with open(self.stderr_path, "r", errors="replace") as f:
                return f.read()
        except OSError:
            return ""

    def wait_exit(self, timeout=180):
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            pass
        return self.proc.returncode

    def kill(self):
        """Group-terminate the VM tree, then sweep by the unique run path."""
        if self.proc.poll() is None:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(self.proc.pid, sig)
                except (ProcessLookupError, PermissionError):
                    pass
                try:
                    self.proc.wait(timeout=15)
                    break
                except subprocess.TimeoutExpired:
                    continue
        # Belt and braces: the run dir path appears in every VM argv, and in
        # nothing else, so this cannot hit an unrelated process.
        subprocess.run(
            ["pkill", "-f", self.run_dir], check=False, stdout=subprocess.DEVNULL
        )
        self.stderr_file.close()

    def elapsed(self):
        return time.monotonic() - self.t0


def _wait_socket(path, session, deadline, what):
    """Wait for a socket to appear; a dead VM here is a launch failure."""
    end = time.monotonic() + deadline
    while time.monotonic() < end:
        if os.path.exists(path):
            return
        if session.exited():
            _raise_launch_failure(session, what)
        time.sleep(0.1)
    raise lib.BenchError(f"{what} socket never appeared: {path}")


def _raise_launch_failure(session, what):
    err = session.stderr_text().strip()
    if "is not supported" in err:
        # vmlab-hv's builder::validate wording; the honest skip reason.
        reason = err.splitlines()[-1] if err else "is not supported"
        raise lib.BackendUnsupported("vmlab-hv", reason)
    raise lib.BenchError(
        f"VM exited (code {session.exit_code()}) before {what}: {err[-2000:]}"
    )


# ---------------------------------------------------------------------------
# QEMU backend — vmlab's exact argv for a linux-modern VM on `virt`
# ---------------------------------------------------------------------------


def qemu_vm_argv(arch, smp, overlay, code_fd, vars_fd, serial, agent_sock, qmp_sock, vnc_sock):
    """The QEMU TCG argv exactly as notes/03 captured from vmlab's builder.

    Deviations from vmlab's real VM, both forced by the harness (no network
    segment): `-nic none`, and no TPM/GPU. The `-name` label is ours.
    """
    return [
        f"qemu-system-{arch}",
        "-name",
        f"vmlab:bench/{arch}",
        "-machine",
        lib.MACHINE_ARG[arch],
        "-accel",
        "tcg",
        "-cpu",
        "max",
        "-smp",
        str(smp),
        "-m",
        "2048M",
        "-display",
        "none",
        "-vnc",
        f"unix:{vnc_sock},share=ignore",
        "-qmp",
        f"unix:{qmp_sock},server=on,wait=off",
        "-monitor",
        "none",
        "-serial",
        f"file:{serial}",
        "-drive",
        f"if=pflash,format=raw,readonly=on,file={code_fd}",
        "-drive",
        f"if=pflash,format=raw,file={vars_fd}",
        "-chardev",
        f"socket,id=vagent0,path={agent_sock},server=on,wait=off",
        "-device",
        "virtio-serial-pci",
        "-device",
        "virtserialport,chardev=vagent0,name=vmlab.agent.0",
        "-blockdev",
        f"driver=qcow2,node-name=disk0,file.driver=file,file.filename={overlay}",
        "-device",
        "virtio-blk-pci,drive=disk0",
        "-nic",
        "none",
        "-device",
        "qemu-xhci",
        "-device",
        "usb-tablet",
        "-S",
    ]


class QemuBackend:
    name = "qemu"

    def __init__(self, run_dir, arch, smp, pin, overlay, code_fd, vars_fd, log):
        self.run_dir = run_dir
        self.arch = arch
        self.smp = smp
        self.pin = pin
        self.serial = os.path.join(run_dir, "serial.log")
        self.agent_sock = os.path.join(run_dir, "agent.sock")
        self.qmp_sock = os.path.join(run_dir, "qmp.sock")
        self.vnc_sock = os.path.join(run_dir, "vnc.sock")
        self.log = log
        self.vm_argv = lib.taskset_prefix(pin) + qemu_vm_argv(
            arch,
            smp,
            overlay,
            code_fd,
            vars_fd,
            self.serial,
            self.agent_sock,
            self.qmp_sock,
            self.vnc_sock,
        )

    def launch(self):
        self.session = VmSession(self.vm_argv, self.run_dir, self.log)
        return self.session

    def release_cpus(self):
        """QMP cont, like vmlab releases the paused VM right after connect."""
        _wait_socket(self.qmp_sock, self.session, 120, "qmp")
        for _ in range(20):
            try:
                lib.qmp_cont(self.qmp_sock)
                return
            except Exception:
                if self.session.exited():
                    _raise_launch_failure(self.session, "qmp")
                time.sleep(0.5)
        raise lib.BenchError(f"QMP cont never succeeded: {self.qmp_sock}")

    def agent_socket(self):
        return self.agent_sock

    def backend_version(self):
        out = subprocess.run(
            [f"qemu-system-{self.arch}", "--version"], capture_output=True, text=True
        ).stdout
        return out.splitlines()[0] if out else "?"


# ---------------------------------------------------------------------------
# vmlab-hv backend — the machine description replaces the QEMU argv
# ---------------------------------------------------------------------------


def hv_machine_description(run_dir, arch, smp, overlay, code_fd, vars_fd):
    """The MachineDescription JSON (vmlab-hv-proto) for the same machine.

    The guest matches the QEMU backend: `virt` hardware, 2 GiB, one qcow2
    overlay on virtio-blk, edk2 firmware pair, the agent port named
    `vmlab.agent.0`. `accel` is `tcg` — the point of the comparison. The
    `machine` field stays at its default until phase 3b/3d adds the `virt`
    machine kind; until then vmlab-hv itself rejects the description, which
    the backend reports as a skip.
    """
    return {
        "name": f"vmlab:bench/{arch}",
        "arch": arch,
        "accel": "tcg",
        "vcpus": smp,
        "memory_mib": 2048,
        "boot": {
            "kind": "firmware",
            "firmware": "ovmf",
            "code": code_fd,
            "vars": vars_fd,
            "secure_boot": False,
        },
        "disks": [
            {
                "id": "disk0",
                "path": overlay,
                "format": "qcow2",
                "read_only": False,
                "vmstate": False,
            }
        ],
        "nics": [],
        "serial_log": os.path.join(run_dir, "serial.log"),
        "ports": [
            {"name": "vmlab.agent.0", "socket": os.path.join(run_dir, "agent.sock")}
        ],
    }


class HvBackend:
    name = "vmlab-hv"

    def __init__(self, run_dir, arch, smp, pin, overlay, code_fd, vars_fd, log, hv_bin):
        if not os.path.isfile(hv_bin):
            raise lib.BenchError(
                f"{hv_bin} not built; run `cargo build --release -p vmlab-hv`"
            )
        self.run_dir = run_dir
        self.arch = arch
        self.pin = pin
        self.control_sock = os.path.join(run_dir, "control.sock")
        self.machine_path = os.path.join(run_dir, "machine.json")
        self.hv_bin = hv_bin
        self.log = log
        desc = hv_machine_description(run_dir, arch, smp, overlay, code_fd, vars_fd)
        with open(self.machine_path, "w") as f:
            json.dump(desc, f, indent=1)
        self.vm_argv = lib.taskset_prefix(pin) + [
            hv_bin,
            "--machine",
            self.machine_path,
            "--control",
            self.control_sock,
            "--paused",
        ]

    def launch(self):
        self.session = VmSession(self.vm_argv, self.run_dir, self.log)
        return self.session

    def release_cpus(self):
        """Handshake + `resume` on the control socket (S1, vmlab's path)."""
        end = time.monotonic() + 120
        while time.monotonic() < end:
            if os.path.exists(self.control_sock):
                break
            if self.session.exited():
                _raise_launch_failure(self.session, "control")
            time.sleep(0.1)
        else:
            raise lib.BenchError(f"control socket never appeared: {self.control_sock}")
        self.control = lib.HvControlClient(self.control_sock)
        self.control.request("resume")
        self.hv_version_seen = self.control.hv_version

    def agent_socket(self):
        return os.path.join(self.run_dir, "agent.sock")

    def backend_version(self):
        return getattr(self, "hv_version_seen", "vmlab-hv (unlaunched)")

    def quit(self):
        ctl = getattr(self, "control", None)
        if ctl is not None:
            try:
                ctl.request("quit")
            except Exception:
                pass
            ctl.close()


# ---------------------------------------------------------------------------
# x86_64 KVM backends (phase 2, HV-D28)
#
# Same discipline, different machine: q35 + firmware boot, one qcow2 overlay
# on virtio-blk, virtio-vga, a NIC attached to vmlab's switch framing. The
# QEMU argv is vmlab's own builder output for the profile shape
# (src/qemu/cmdline.rs build_args, captured black-box in docs/hwref); the
# hv description names the same devices at QEMU's auto-assigned PCI
# functions. The Hyper-V enlightenments below are shared by both: the
# hv description names the same flags in the proto's spelling; the QEMU
# -cpu string wants vmlab's hv- spelling, with spinlocks at 0x1fff.
HYPERV_FLAGS = [
    "relaxed", "vapic", "spinlocks", "vpindex", "runtime", "time",
    "synic", "stimer", "frequencies", "reset", "tlbflush", "ipi",
]


def hyperv_cpu_flags():
    flags = ["hv-spinlocks=0x1fff" if f == "spinlocks" else f"hv-{f}" for f in HYPERV_FLAGS]
    if lib.host_apic_virtualization():
        flags.append("hv-avic")
    return ",".join(flags)



def qemu_x86_vm_argv(guest, smp, overlay, code_fd, vars_fd, serial, agent_sock,
                     qmp_sock, vnc_sock, switch_sock, nic_mac, host_mtu):
    """vmlab's exact KVM argv for the guest's profile shape (docs/hwref).

    One deviation, forced by the bench and applied to the hv backend too:
    `host_mtu` on the NIC (jumbo frames keep the framing peer and the
    stdlib guest tools off the net-workload ceiling). Nothing else differs
    from what vmlab spawns for a VM with one network segment.
    """
    profile = lib.X86_GUESTS[guest]
    windows = profile["hyperv"]
    cpu = "host,vmx=off,svm=off"
    argv = [
        "qemu-system-x86_64",
        "-name", f"vmlab:bench/{guest}",
        "-machine", "q35",
        "-accel", "kvm",
    ]
    if windows:
        argv += ["-cpu", f"host,vmx=off,svm=off,{hyperv_cpu_flags()}"]
    else:
        argv += ["-cpu", cpu]
    argv += [
        "-smp", str(smp),
        "-m", f"{profile['memory_mib']}M",
        "-display", "none",
        "-vnc", f"unix:{vnc_sock},share=ignore",
        "-qmp", f"unix:{qmp_sock},server=on,wait=off",
        "-monitor", "none",
        "-serial", f"file:{serial}",
    ]
    if windows and lib.host_apic_virtualization():
        argv += ["-global", "kvm-pit.lost_tick_policy=discard"]
    if profile["firmware"] == "ovmf":
        argv += [
            "-drive", f"if=pflash,format=raw,readonly=on,file={code_fd}",
            "-drive", f"if=pflash,format=raw,file={vars_fd}",
        ]
    argv += [
        "-chardev", f"socket,id=vagent0,path={agent_sock},server=on,wait=off",
        "-device", "virtio-serial-pci",
        "-device", "virtserialport,chardev=vagent0,name=vmlab.agent.0",
        "-blockdev",
        f"driver=qcow2,node-name=disk0,file.driver=file,file.filename={overlay}",
        "-device",
        # SeaBIOS honours an explicit boot order; OVMF boots the disk alone.
        "virtio-blk-pci,drive=disk0"
        + (",bootindex=50" if profile["firmware"] == "seabios" else ""),
    ]
    if switch_sock is None:
        # No network segment: `-nic none` is vmlab's own shape for it (and
        # the boot-only windows guest runs exactly that — the reference lab
        # boots this template without a NIC).
        argv += ["-nic", "none", "-device", "virtio-vga", "-usb",
                 "-device", "usb-tablet", "-S"]
        return argv
    mtu = f",host_mtu={host_mtu}" if host_mtu else ""
    argv += [
        "-netdev",
        f"stream,id=net0,server=off,addr.type=unix,addr.path={switch_sock}",
        "-device", f"virtio-net-pci,netdev=net0,mac={nic_mac}{mtu}",
        "-device", "virtio-vga",
        "-usb",
        "-device", "usb-tablet",
        "-S",
    ]
    return argv


class X86QemuBackend(QemuBackend):
    """vmlab's exact argv, x86_64 KVM profile shape."""

    def __init__(self, run_dir, guest, smp, pin, overlay, code_fd, vars_fd,
                 switch_sock, nic_mac, host_mtu, log):
        self.run_dir = run_dir
        self.arch = "x86_64"
        self.smp = smp
        self.pin = pin
        self.guest = guest
        self.serial = os.path.join(run_dir, "serial.log")
        self.agent_sock = os.path.join(run_dir, "agent.sock")
        self.qmp_sock = os.path.join(run_dir, "qmp.sock")
        self.vnc_sock = os.path.join(run_dir, "vnc.sock")
        self.log = log
        self.vm_argv = lib.taskset_prefix(pin) + qemu_x86_vm_argv(
            guest, smp, overlay, code_fd, vars_fd, self.serial, self.agent_sock,
            self.qmp_sock, self.vnc_sock, switch_sock, nic_mac, host_mtu,
        )


def hv_x86_machine_description(run_dir, guest, smp, overlay, code_fd, vars_fd,
                               switch_sock, nic_mac, host_mtu, ipxe_rom):
    """The MachineDescription for the machine the argv above builds.

    Devices sit at QEMU's own auto-assigned functions for that argv, so the
    guest sees the same PCI layout under either backend (D30). The NIC
    carries the separately built iPXE ROM where QEMU loads its own combo
    ROM (D16); SeaBIOS and the VGA ROM are the same bytes QEMU uses.
    """
    profile = lib.X86_GUESTS[guest]
    pci = {"bus": 0, "device": 0, "function": 0}

    def at(slot):
        return {**pci, "device": slot}

    if profile["firmware"] == "seabios":
        boot = {
            "kind": "firmware",
            "firmware": "seabios",
            "code": code_fd,
            # QEMU's bootindex=50 on the disk, as the fw_cfg bootorder file
            # QEMU writes for it (docs/hwref .../blobs/bootorder).
            "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"],
        }
    else:
        boot = {
            "kind": "firmware",
            "firmware": "ovmf",
            "code": code_fd,
            "vars": vars_fd,
            "secure_boot": False,
        }
    nics = []
    if switch_sock is not None:
        nic = {
            "pci": at(3),
            "mac": nic_mac,
            "attachment": {"kind": "stream", "socket": switch_sock},
            "rom": ipxe_rom,
        }
        if host_mtu:
            nic["mtu"] = host_mtu
        nics.append(nic)
    desc = {
        "machine": "q35",
        "name": f"vmlab:bench/{guest}",
        "arch": "x86_64",
        "accel": "kvm",
        "vcpus": smp,
        "memory_mib": profile["memory_mib"],
        "boot": boot,
        "disks": [{
            "id": "disk0",
            "pci": at(2),
            "path": overlay,
            "format": "qcow2",
            "read_only": False,
            "vmstate": True,
        }],
        "console_pci": at(1),
        "ahci_pci": {"bus": 0, "device": 31, "function": 2},
        "nics": nics,
        "display": {
            "device": "virtio-vga",
            "pci": at(4),
            "rom": None,
            "socket": os.path.join(run_dir, "display.sock"),
            "vnc_socket": os.path.join(run_dir, "vnc.sock"),
        },
        "tablet": at(5),
        "usb": at(5),
        "ports": [{"name": "vmlab.agent.0", "socket": os.path.join(run_dir, "agent.sock")}],
        "serial_log": os.path.join(run_dir, "serial.log"),
    }
    if profile["hyperv"]:
        desc["hyperv"] = HYPERV_FLAGS + (["avic"] if lib.host_apic_virtualization() else [])
    return desc


class X86HvBackend(HvBackend):
    """The same machine as a vmlab-hv machine description."""

    def __init__(self, run_dir, guest, smp, pin, overlay, code_fd, vars_fd,
                 switch_sock, nic_mac, host_mtu, log, hv_bin, ipxe_rom):
        if not os.path.isfile(hv_bin):
            raise lib.BenchError(
                f"{hv_bin} not built; run `cargo build --release -p vmlab-hv`"
            )
        if ipxe_rom and not os.path.isfile(ipxe_rom):
            raise lib.BenchError(f"{ipxe_rom} absent; run `just firmware-ipxe-build`")
        self.run_dir = run_dir
        self.arch = "x86_64"
        self.pin = pin
        self.guest = guest
        self.control_sock = os.path.join(run_dir, "control.sock")
        self.machine_path = os.path.join(run_dir, "machine.json")
        self.hv_bin = hv_bin
        self.log = log
        desc = hv_x86_machine_description(
            run_dir, guest, smp, overlay, code_fd, vars_fd,
            switch_sock, nic_mac, host_mtu, ipxe_rom,
        )
        with open(self.machine_path, "w") as f:
            json.dump(desc, f, indent=1)
        self.vm_argv = lib.taskset_prefix(pin) + [
            hv_bin, "--machine", self.machine_path,
            "--control", self.control_sock, "--paused",
        ]

"""Unit tests for the bench suite's pure logic (stdlib unittest).

Run from the repo: `just bench-test` (or `cd bench && python3 -m unittest`).
The VM-booting path is verified by the real runs the recipes drive; these
tests pin the pieces that must not drift silently: statistics, the history
format and its regression decision, the QEMU argv (vmlab's, per notes/03),
the vmlab-hv machine description against the proto's field set, and the
noise-control policy.
"""

import argparse
import json
import os
import tempfile
import unittest
from unittest import mock

import lib
from backends import hv_machine_description, qemu_vm_argv


class StatsTests(unittest.TestCase):
    def test_median_odd_and_even(self):
        self.assertEqual(lib.median_and_spread([3.0, 1.0, 2.0]), (2.0, 2.0))
        self.assertEqual(lib.median_and_spread([4.0, 1.0, 3.0, 2.0]), (2.5, 3.0))

    def test_median_single_sample(self):
        self.assertEqual(lib.median_and_spread([7.3]), (7.3, 0.0))

    def test_verdict_ok_within_spread(self):
        v, allowed = lib.regression_verdict(
            {"median": 28.0, "spread": 1.5}, {"median": 29.0, "spread": 0.5}
        )
        self.assertEqual(v, "ok")
        self.assertEqual(allowed, 1.5)

    def test_verdict_worse_beyond_spread(self):
        v, _ = lib.regression_verdict(
            {"median": 28.0, "spread": 1.5}, {"median": 35.0, "spread": 0.5}
        )
        self.assertEqual(v, "worse")

    def test_verdict_floor_for_tiny_spreads(self):
        # Zero measured spread must not make every move a regression.
        v, allowed = lib.regression_verdict(
            {"median": 2.0, "spread": 0.0}, {"median": 2.08, "spread": 0.0}
        )
        self.assertEqual(v, "ok")
        self.assertEqual(allowed, 0.1)

    def test_verdict_better(self):
        v, _ = lib.regression_verdict(
            {"median": 28.0, "spread": 0.5}, {"median": 20.0, "spread": 0.5}
        )
        self.assertEqual(v, "better")


class HistoryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.patch = mock.patch.object(lib, "HISTORY_DIR", self.tmp.name)
        self.patch.start()
        self.addCleanup(self.patch.stop)

    def record(self, **kw):
        rec = {
            "recorded": "2026-10-31T00:00:00",
            "commit": "f" * 40,
            "dirty": False,
            "host": "testhost",
            "backend": "qemu",
            "arch": "aarch64",
            "guest": "debian-13",
            "vcpus": 1,
            "workload": "boot_to_agent_ready",
            "unit": "s",
            "repeats": 3,
            "samples": [1.0, 2.0, 3.0],
            "median": 2.0,
            "spread": 2.0,
            "qemu_baseline": None,
            "env": {},
        }
        rec.update(kw)
        return rec

    def test_append_and_read_round_trip(self):
        lib.append_history("aarch64", self.record())
        lib.append_history("aarch64", self.record(vcpus=4))
        records = lib.read_history("aarch64")
        self.assertEqual([r["vcpus"] for r in records], [1, 4])
        self.assertEqual(records[0]["workload"], "boot_to_agent_ready")

    def test_read_missing_file_is_empty(self):
        self.assertEqual(lib.read_history("riscv64"), [])

    def test_latest_baseline_picks_newest_qemu_match(self):
        lib.append_history("aarch64", self.record(median=30.0, recorded="t1"))
        lib.append_history(
            "aarch64", self.record(backend="vmlab-hv", median=60.0, recorded="t2")
        )
        lib.append_history(
            "aarch64", self.record(median=28.0, recorded="t3", qemu_baseline=None)
        )
        lib.append_history("aarch64", self.record(median=26.0, recorded="t4"))
        base = lib.latest_baseline(
            lib.read_history("aarch64"), "aarch64", "debian-13", 1, "boot_to_agent_ready"
        )
        self.assertEqual(base["median"], 26.0)
        self.assertEqual(base["recorded"], "t4")

    def test_latest_baseline_none_when_only_hv(self):
        lib.append_history(
            "aarch64", self.record(backend="vmlab-hv", median=60.0)
        )
        self.assertIsNone(
            lib.latest_baseline(
                lib.read_history("aarch64"), "aarch64", "debian-13", 1,
                "boot_to_agent_ready",
            )
        )


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.patch = mock.patch.object(lib, "HISTORY_DIR", self.tmp.name)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.args = argparse.Namespace(arch="aarch64", backend="all", guest=None)

    def write(self, medians, backend="qemu", vcpus=1, workload="xz_9_32M"):
        for m, s in medians:
            lib.append_history(
                "aarch64",
                {
                    "recorded": "t", "commit": "f" * 40, "dirty": False,
                    "host": "h", "backend": backend, "arch": "aarch64",
                    "guest": "debian-13", "vcpus": vcpus, "workload": workload,
                    "unit": "s", "repeats": 3, "samples": [], "median": m,
                    "spread": s, "qemu_baseline": None, "env": {},
                },
            )

    def test_single_entry_is_not_a_failure(self):
        import check

        self.write([(34.0, 1.0)])
        regressions, notes = check.compare("aarch64", self.args)
        self.assertEqual(regressions, [])
        self.assertTrue(any("nothing to compare" in n for n in notes))

    def test_regression_within_noise_passes(self):
        import check

        self.write([(34.0, 2.0), (35.0, 1.0)])
        regressions, _ = check.compare("aarch64", self.args)
        self.assertEqual(regressions, [])

    def test_regression_beyond_noise_fails(self):
        import check

        self.write([(34.0, 0.5), (48.0, 1.0)])
        regressions, _ = check.compare("aarch64", self.args)
        self.assertEqual(len(regressions), 1)
        self.assertIn("worse", regressions[0])

    def test_improvement_is_noted_not_failed(self):
        import check

        self.write([(34.0, 0.5), (20.0, 1.0)])
        regressions, notes = check.compare("aarch64", self.args)
        self.assertEqual(regressions, [])
        self.assertTrue(any("better" in n for n in notes))

    def test_configs_do_not_cross_compare(self):
        import check

        # A big qemu regression must not taint the vcpus=4 config.
        self.write([(34.0, 0.5), (48.0, 0.5)])
        self.write([(20.0, 0.5), (20.2, 0.5)], vcpus=4)
        regressions, _ = check.compare("aarch64", self.args)
        self.assertEqual(len(regressions), 1)

    def test_main_exit_code(self):
        import check

        self.write([(34.0, 0.5), (48.0, 0.5)])
        self.assertEqual(check.main(["--arch", "aarch64"]), 1)
        self.write([(47.0, 0.5)])
        # 48 -> 47 is an improvement beyond the 5 % floor: clean run.
        self.assertEqual(check.main(["--arch", "aarch64"]), 0)


class QemuArgvTests(unittest.TestCase):
    def test_argv_is_vmlab_shape(self):
        argv = qemu_vm_argv(
            "riscv64", 4,
            overlay="/run/ov.qcow2", code_fd="/fw/CODE.fd", vars_fd="/run/vars.fd",
            serial="/run/serial.log", agent_sock="/run/agent.sock",
            qmp_sock="/run/qmp.sock", vnc_sock="/run/vnc.sock",
        )
        self.assertEqual(
            argv,
            [
                "qemu-system-riscv64",
                "-name", "vmlab:bench/riscv64",
                "-machine", "virt,acpi=off",
                "-accel", "tcg",
                "-cpu", "max",
                "-smp", "4",
                "-m", "2048M",
                "-display", "none",
                "-vnc", "unix:/run/vnc.sock,share=ignore",
                "-qmp", "unix:/run/qmp.sock,server=on,wait=off",
                "-monitor", "none",
                "-serial", "file:/run/serial.log",
                "-drive", "if=pflash,format=raw,readonly=on,file=/fw/CODE.fd",
                "-drive", "if=pflash,format=raw,file=/run/vars.fd",
                "-chardev", "socket,id=vagent0,path=/run/agent.sock,server=on,wait=off",
                "-device", "virtio-serial-pci",
                "-device", "virtserialport,chardev=vagent0,name=vmlab.agent.0",
                "-blockdev",
                "driver=qcow2,node-name=disk0,file.driver=file,"
                "file.filename=/run/ov.qcow2",
                "-device", "virtio-blk-pci,drive=disk0",
                "-nic", "none",
                "-device", "qemu-xhci",
                "-device", "usb-tablet",
                "-S",
            ],
        )

    def test_aarch64_machine_is_acpi_virt(self):
        argv = qemu_vm_argv(
            "aarch64", 1,
            overlay="/o", code_fd="/c", vars_fd="/v", serial="/s",
            agent_sock="/a", qmp_sock="/q", vnc_sock="/n",
        )
        self.assertIn("virt", argv[argv.index("-machine") + 1])
        self.assertNotIn("virt,acpi=off", argv)


# The MachineDescription field set of vmlab-hv-proto as of this crate. The
# bench emits a subset; if the proto renames or removes a field the bench
# uses, the hv backend's JSON would stop parsing (deny_unknown_fields) and
# this test names the culprit.
MACHINE_FIELDS = {
    "machine", "cpu", "hyperv", "display", "tablet", "tpm", "console_pci",
    "ahci_pci", "name", "arch", "accel", "vcpus", "memory_mib", "boot",
    "disks", "nics", "serial_log", "ports", "shares", "audio", "rtc_base",
}
BOOT_FIRMWARE_FIELDS = {"kind", "firmware", "code", "vars", "secure_boot", "bootorder"}
DISK_FIELDS = {"pci", "bus", "port", "cdrom", "id", "path", "format",
               "read_only", "vmstate"}
PORT_FIELDS = {"name", "socket"}


class HvMachineTests(unittest.TestCase):
    def setUp(self):
        self.desc = hv_machine_description(
            "/run", "riscv64", 4, "/run/ov.qcow2", "/fw/CODE.fd", "/run/vars.fd"
        )

    def test_fields_fit_proto_schema(self):
        self.assertTrue(set(self.desc) <= MACHINE_FIELDS, set(self.desc) - MACHINE_FIELDS)
        self.assertTrue(
            set(self.desc["boot"]) <= BOOT_FIRMWARE_FIELDS,
            set(self.desc["boot"]) - BOOT_FIRMWARE_FIELDS,
        )
        for disk in self.desc["disks"]:
            self.assertTrue(set(disk) <= DISK_FIELDS, set(disk) - DISK_FIELDS)
        for port in self.desc["ports"]:
            self.assertTrue(set(port) <= PORT_FIELDS, set(port) - PORT_FIELDS)

    def test_matches_qemu_backend_guest(self):
        self.assertEqual(self.desc["arch"], "riscv64")
        self.assertEqual(self.desc["accel"], "tcg")
        self.assertEqual(self.desc["vcpus"], 4)
        self.assertEqual(self.desc["memory_mib"], 2048)
        self.assertEqual(self.desc["boot"]["firmware"], "ovmf")
        self.assertEqual(self.desc["ports"][0]["name"], "vmlab.agent.0")
        self.assertEqual(self.desc["disks"][0]["format"], "qcow2")
        self.assertFalse(self.desc["disks"][0]["read_only"])


class NoiseControlTests(unittest.TestCase):
    def test_pin_windows(self):
        with mock.patch.object(os, "cpu_count", return_value=32):
            self.assertEqual(lib.pin_cpus(1), "16-19")
            self.assertEqual(lib.pin_cpus(4), "16-23")

    def test_pin_override_wins(self):
        self.assertEqual(lib.pin_cpus(4, override="2,6,8"), "2,6,8")

    def test_version_key_prefers_newer_template(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(lib, "TEMPLATE_ROOT", tmp):
                root = os.path.join(tmp, "riscv64", "debian-13")
                for v in ("13.20260601.9", "13.20261001.0", "13.20260601.10"):
                    os.makedirs(os.path.join(root, v))
                    open(os.path.join(root, v, "disk.qcow2"), "w").close()
                self.assertEqual(
                    lib.latest_template("riscv64", "debian-13"), "13.20261001.0"
                )

    def test_missing_template_is_an_error(self):
        with mock.patch.object(
            lib, "TEMPLATE_ROOT", "/nonexistent-vmlab-templates"
        ):
            with self.assertRaises(lib.BenchError):
                lib.latest_template("aarch64", "debian-13")


class ScriptSmokeTests(unittest.TestCase):
    def test_run_help(self):
        import run
        import contextlib
        import io

        with contextlib.redirect_stdout(io.StringIO()):
            args = run.parse_args(["--backend", "vmlab-hv", "--arch", "riscv64"])
        self.assertEqual(args.backend, "vmlab-hv")
        self.assertEqual(run.resolve_arches("all"), ["aarch64", "riscv64"])
        self.assertEqual(run.resolve_arches("riscv64,aarch64"), ["riscv64", "aarch64"])
        with self.assertRaises(lib.BenchError):
            run.resolve_arches("i686")

    def test_agent_frame_layout(self):
        f = lib.agent_frame(0, 7, b"xyz")
        self.assertEqual(f[:4], b"VMLB")
        length, kind, channel = lib.struct.unpack("<IBI", f[4:13])
        self.assertEqual((length, kind, channel), (3, 0, 7))
        self.assertEqual(f[13:], b"xyz")


if __name__ == "__main__":
    unittest.main()

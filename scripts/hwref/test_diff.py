import importlib.util
import json
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
SPEC = importlib.util.spec_from_file_location("hwref_diff", Path(__file__).with_name("diff.py"))
diff = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(diff)
import hv_capture

ROOT = Path(__file__).resolve().parents[2]
REFERENCE = ROOT / "docs/hwref/q35-seabios-linux-generic"


class NormalizeTests(unittest.TestCase):
    def test_times_counts_mac_and_kernel_addresses(self):
        pairs = [
            ("dmesg.txt", "[ 1.123] audit: audit(1234567890.123:1)\n",
             "[ 9.456] audit: audit(1234567899.456:8)\n"),
            ("dmesg.txt", "rtc: 2026-10-30T11:39:37 UTC\n", "rtc: 2026-10-31T12:30:52 UTC\n"),
            ("interrupts.txt", " 21: 50 42 IO-APIC uhci_hcd:usb1\n",
             " 21: 99 20 IO-APIC uhci_hcd:usb1\n"),
            ("serial16550.txt", "0: tx:123 rx:42\n", "0: tx:999 rx:12\n"),
            ("iomem.txt", "  48000000-48dfffff : Kernel code\n",
             "  55000000-55dfffff : Kernel code\n"),
            ("dmesg.txt", "net: 52:54:00:00:00:01\n", "net: 52:54:00:00:00:02\n"),
        ]
        for name, a, b in pairs:
            with self.subTest(name=name):
                self.assertEqual(diff.normalize_text(name, a), diff.normalize_text(name, b))

    def test_cpuid_host_mask_but_hypervisor_features_preserved(self):
        a = "0x00000001 0x00: eax=0x12345678\n0x40000003 0x00: eax=0x00000001\n"
        b = a.replace("12345678", "87654321")
        self.assertEqual(diff.normalize_text("cpuid.txt", a), diff.normalize_text("cpuid.txt", b))
        self.assertNotEqual(diff.normalize_text("cpuid.txt", a),
                            diff.normalize_text("cpuid.txt", a.replace("00000001\n", "00000002\n")))

    def test_pci_allocations_keep_type_size_and_identity(self):
        config = bytearray(256)
        config[:4] = bytes.fromhex("f4 1a 03 10")
        struct.pack_into("<I", config, 0x10, 0xc001)
        struct.pack_into("<Q", config, 0x14, 0x12340000000c)
        changed = bytearray(config)
        struct.pack_into("<I", changed, 0x10, 0x6001)
        struct.pack_into("<Q", changed, 0x14, 0x43210000000c)
        self.assertEqual(diff.pci_config(config), diff.pci_config(changed))
        changed[0] ^= 1
        self.assertNotEqual(diff.pci_config(config), diff.pci_config(changed))
        changed[0] ^= 1
        changed[0x14] = 4
        self.assertNotEqual(diff.pci_config(config), diff.pci_config(changed))
        a = "00:01.0 Device [1af4:1003]\n\tRegion 0: Memory at febd0000 [size=4K]\n"
        self.assertEqual(diff.normalize_pci(a), diff.normalize_pci(a.replace("febd0000", "80840000")))
        self.assertNotEqual(diff.normalize_pci(a), diff.normalize_pci(a.replace("4K", "8K")))

    def test_acpi_relocations_keep_pm_registers_and_header(self):
        a = bytearray(244)
        a[:4] = b"FACP"
        b = bytearray(a)
        b[9] = 4
        b[36:44] = bytes(range(8))
        b[132:148] = bytes(range(16))
        self.assertEqual(diff.acpi(a), diff.acpi(b))
        b[56] = 1
        self.assertNotEqual(diff.acpi(a), diff.acpi(b))

    def test_smbios_uuid_and_anchor_only(self):
        a = bytes([1, 24, 0, 0]) + bytes(20) + b"Vendor\0\0"
        b = a[:8] + bytes(range(16)) + a[24:]
        self.assertEqual(diff.smbios(a), diff.smbios(b))
        self.assertNotEqual(diff.smbios(a), diff.smbios(b.replace(b"Vendor", b"Other!")))
        anchor = b"_SM3_" + bytes(19)
        changed = bytearray(anchor)
        changed[5] = 2
        changed[16:24] = bytes(range(8))
        self.assertEqual(diff.smbios_anchor(anchor), diff.smbios_anchor(changed))
        processor = bytes([4, 24, 0, 0]) + bytes(20) + b"CPU\0\0"
        native = processor[:8] + bytes(range(8)) + processor[16:]
        self.assertEqual(diff.smbios(processor), diff.smbios(native))

    def test_unknown_binary_is_not_masked(self):
        self.assertNotEqual(diff.normalize_artifact("new-blob", b"\x00\xff"),
                            diff.normalize_artifact("new-blob", b"\x00\xfe"))

    def test_firmware_boundaries_move_but_ram_ceiling_remains_exact(self):
        with tempfile.TemporaryDirectory(prefix="hwref-map-test-") as tmp:
            guest = Path(tmp)
            outputs = []
            for base in (0x7ca5d000, 0x7ca5a000):
                text = (f"9 0x900000 {base-1:#x} System RAM\n"
                        f"10 {base:#x} {base+0x2fff:#x} ACPI Tables\n"
                        f"11 {base+0x3000:#x} 0x7eb3efff System RAM\n")
                (guest / "e820-memmap.txt").write_text(text)
                outputs.append(diff.mask_boundaries("e820-memmap.txt", text.encode(), diff.allocation_map(guest)))
            self.assertEqual(outputs[0], outputs[1])
            self.assertIn(b"0x7eb3efff", outputs[0])
            self.assertIn(b"acpi:0x3000", outputs[0])
            self.assertNotEqual(outputs[0], outputs[1].replace(b"0x7eb3efff", b"0x8eb3efff"))

    def test_all_checked_in_artifacts_are_supported(self):
        for root in (ROOT / "docs/hwref").iterdir():
            if (root / "guest").is_dir():
                self.assertTrue(diff.load_capture(root))


class DiffTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="hwref-diff-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "capture"
        shutil.copytree(REFERENCE, self.root)

    def test_identical_and_changed_capture_via_cli(self):
        command = [sys.executable, str(Path(diff.__file__)), str(REFERENCE), str(self.root)]
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        path = self.root / "guest/lspci-nn.txt"
        path.write_text(path.read_text().replace("8086:29c0", "8086:ffff"))
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("=== lspci-nn.txt ===", result.stdout)
        self.assertIn("+00:00.0", result.stdout)

    def test_missing_capture_and_missing_or_new_artifact(self):
        (self.root / "guest/cpuid.txt").unlink()
        with self.assertRaisesRegex(ValueError, "required artefact cpuid"):
            diff.compare(REFERENCE, self.root)
        shutil.copy2(REFERENCE / "guest/cpuid.txt", self.root / "guest/cpuid.txt")
        (self.root / "guest/unknown.bin").write_bytes(b"\0\xff")
        self.assertIn("unknown.bin: candidate only", diff.compare(REFERENCE, self.root))
        (self.root / "guest/usb.txt").unlink()
        self.assertIn("usb.txt: reference only", diff.compare(REFERENCE, self.root))

    def test_different_profiles_are_real_differences(self):
        report = diff.compare(REFERENCE, ROOT / "docs/hwref/q35-ovmf-win11")
        self.assertIn("acpi/TPM2: candidate only", report)
        self.assertIn("lspci-nn.txt", report)
        self.assertIn("1af4:1050", report)
        self.assertIn("cpuid.txt", report)

    def test_invalid_allow_list_is_an_input_error(self):
        path = Path(self.tmp.name) / "allow.json"
        for value in ([], {"decision": "D6"}, {"decision": "D6", "reference_only": {},
                                              "candidate_only_ids": [42]}):
            with self.subTest(value=value):
                path.write_text(json.dumps(value))
                with self.assertRaises(ValueError):
                    diff.compare(REFERENCE, self.root, path)

    def test_directional_d6_and_wrong_identity(self):
        rules = json.loads(diff.DEFAULT_ALLOW.read_text())
        usb = "00:1d.0 Class [0c03]: Device [8086:2934]\n"
        base = "00:00.0 Class [0600]: Device [8086:29c0]\n"
        a, b = {"lspci-nn.txt": base + usb}, {"lspci-nn.txt": base}
        diff.approved_pci(a, b, rules)
        self.assertEqual(a, b)
        a, b = {"lspci-nn.txt": base}, {"lspci-nn.txt": base + usb}
        diff.approved_pci(a, b, rules)
        self.assertNotEqual(a, b)
        a, b = {"lspci-nn.txt": base + usb.replace("2934", "ffff")}, {"lspci-nn.txt": base}
        diff.approved_pci(a, b, rules)
        self.assertNotEqual(a, b)
        xhci = "00:05.0 Class [0c03]: Device [1b36:000d]\n"
        a, b = {"lspci-nn.txt": base}, {"lspci-nn.txt": base + xhci}
        diff.approved_pci(a, b, rules)
        self.assertEqual(a, b)
        a, b = {"lspci-nn.txt": base}, {"lspci-nn.txt": base + xhci + xhci.replace("05.0", "06.0")}
        diff.approved_pci(a, b, rules)
        self.assertNotEqual(a, b)
        a, b = {"lspci-nn.txt": base + xhci}, {"lspci-nn.txt": base + xhci.replace("Class", "Changed")}
        diff.approved_pci(a, b, rules)
        self.assertNotEqual(a, b)


class HvCaptureTests(unittest.TestCase):
    def test_resume_uses_control_handshake_and_waits_past_events(self):
        with tempfile.TemporaryDirectory(prefix="hwref-control-test-") as tmp:
            path = Path(tmp) / "control.sock"
            received = []
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(path))
                server.listen(1)
                def serve():
                    connection, _ = server.accept()
                    with connection, connection.makefile("rwb") as stream:
                        received.append(json.loads(stream.readline()))
                        stream.write(b'{"type":"hello","proto_version":1}\n')
                        stream.flush()
                        received.append(json.loads(stream.readline()))
                        stream.write(b'{"type":"event","event":"resumed"}\n')
                        stream.write(b'{"type":"reply","id":1,"outcome":"ok"}\n')
                        stream.flush()
                thread = threading.Thread(target=serve, daemon=True)
                thread.start()
                with subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"]) as process:
                    try:
                        hv_capture.resume(path, process, timeout=2)
                    finally:
                        process.terminate()
                thread.join(timeout=2)
            self.assertEqual(received, [{"type": "hello", "proto_version": 1},
                                       {"type": "request", "id": 1, "command": {"command": "resume"}}])

    def test_only_known_support_errors_skip(self):
        self.assertTrue(hv_capture.unsupported_firmware("unknown variant `firmware`, expected `kernel`"))
        self.assertTrue(hv_capture.unsupported_firmware("unknown field `machine`"))
        self.assertFalse(hv_capture.unsupported_firmware("cannot open firmware image"))
        self.assertFalse(hv_capture.unsupported_firmware("KVM error"))

    def test_prepare_machine_preserves_pci_and_original_vars(self):
        with tempfile.TemporaryDirectory(prefix="hwref-hv-test-") as tmp:
            root = Path(tmp)
            (root / "esp.raw").write_bytes(b"probe")
            (root / "vars.fd").write_bytes(b"original-vars")
            class Assets:
                esp = root / "esp.raw"
            machine = {"machine": "q35", "boot": {"kind": "firmware", "vars": str(root / "vars.fd")},
                       "disks": [{"id": "hwref-probe", "pci_address": "00:02.0", "bus": "virtio"}],
                       "ports": [{"name": "vmlab.agent.0", "socket": "unused"}], "nics": []}
            run = root / "run"
            run.mkdir()
            result = hv_capture.prepare_machine(machine, run, root, Assets())
            self.assertEqual(result["disks"][0]["pci_address"], "00:02.0")
            self.assertEqual(Path(result["disks"][0]["path"]).read_bytes(), b"probe")
            Path(result["boot"]["vars"]).write_bytes(b"modified")
            self.assertEqual((root / "vars.fd").read_bytes(), b"original-vars")
            self.assertEqual(machine["ports"][0]["socket"], "unused")


if __name__ == "__main__":
    unittest.main()

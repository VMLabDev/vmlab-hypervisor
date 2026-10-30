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
import aml
import capture
import d6
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
REFERENCE = ROOT / "docs/hwref/q35-seabios-linux-generic"


class NormalizeTests(unittest.TestCase):
    def test_option_rom_identity_ignores_code_but_keeps_pci_identity(self):
        image = bytearray(512)
        image[:2] = b"\x55\xaa"
        struct.pack_into("<H", image, 0x18, 0x20)
        image[0x20:0x28] = b"PCIR\xf4\x1a\x00\x10"
        struct.pack_into("<H", image, 0x30, 1)
        image[0x35] = 0x80
        name = "pci-roms/0000:00:03.0.rom"
        original = diff.normalize_artifact(name, image)
        image[100] = 42
        self.assertEqual(original, diff.normalize_artifact(name, image))
        image[0x26] = 1
        self.assertNotEqual(original, diff.normalize_artifact(name, image))
        image[0x30] = 2
        with self.assertRaises(ValueError):
            diff.normalize_artifact(name, image)
        self.assertNotEqual(diff.normalize_text("lspci-nnnvvvxxxx.txt",
            "Expansion ROM at feb00000 [disabled] [size=256K]\n"),
            diff.normalize_text("lspci-nnnvvvxxxx.txt",
            "Expansion ROM at feb00000 [disabled] [size=128K]\n"))

    def test_pci_resource_maps_move_but_sizes_and_bars_remain_exact(self):
        with tempfile.TemporaryDirectory(prefix="hwref-bars-") as tmp:
            root = Path(tmp)
            def normalized(base, size="32"):
                (root / "lspci-nnnvvvxxxx.txt").write_text(
                    f"00:1f.2 AHCI [8086:2922]\n\tRegion 4: I/O ports at {base:x} [size={size}]\n")
                text = f"  {base:04x}-{base+31:04x} : 0000:00:1f.2\n    {base:04x}-{base+31:04x} : ahci\n"
                return diff.mask_boundaries("ioports.txt", text.encode(), diff.allocation_map(root))
            self.assertEqual(normalized(0xc180), normalized(0xc120))
            self.assertNotEqual(normalized(0xc180), normalized(0xc120, "64"))

    def test_d6_workspace_requires_the_exact_four_page_change(self):
        removed = {"00:1d.0", "00:1d.1", "00:1d.2", "00:1d.7"}
        left = {"e820-memmap.txt": "3 0x100000 0x7ffd7fff System RAM\n4 0x7ffd8000 0x7fffffff Reserved\n",
                "dmesg.txt": "Total pages: 1048438\nMemory: <available>K/4193752K available\nOn node 0, zone Normal: 40 pages in unavailable ranges\n"}
        right = {"e820-memmap.txt": left["e820-memmap.txt"].replace("7ffd7fff", "7ffdbfff").replace("7ffd8000", "7ffdc000"),
                 "dmesg.txt": left["dmesg.txt"].replace("1048438", "1048442").replace("4193752", "4193768").replace(": 40 pages", ": 36 pages")}
        changed = dict(left)
        d6.workspace(changed, right, removed)
        self.assertEqual(changed, right)
        for key, before, after in (("e820-memmap.txt", "7ffdc000", "7ffdd000"),
                                   ("e820-memmap.txt", "7fffffff", "8fffffff"),
                                   ("dmesg.txt", "1048442", "1048443")):
            bad = dict(right)
            bad[key] = bad[key].replace(before, after)
            changed = dict(left)
            d6.workspace(changed, bad, removed)
            self.assertNotEqual(changed, bad)
        changed = dict(left)
        d6.workspace(changed, right, set())
        self.assertEqual(changed, left)

    def test_d6_controller_logs_keep_errors_and_unrelated_devices(self):
        data = {"dmesg.txt": "pci 0000:00:05.0: [1b36:000d] endpoint\n"
                "xhci_hcd 0000:00:05.0: xHCI Host Controller\n"
                "xhci_hcd 0000:00:05.0: controller failed\n"
                "pci 0000:00:05.0: BAR 0 can't claim\n"
                "pci 0000:00:06.0: unrelated device\n"}
        d6.observations(data, {"00:05.0"})
        self.assertNotIn("endpoint", data["dmesg.txt"])
        self.assertNotIn("Host Controller", data["dmesg.txt"])
        self.assertIn("controller failed", data["dmesg.txt"])
        self.assertIn("can't claim", data["dmesg.txt"])
        self.assertIn("unrelated device", data["dmesg.txt"])

    def test_d6_keeps_a_gsi_used_by_another_function(self):
        text = "ACPI: \\_SB_.GSIB: Enabled at IRQ 17\n"
        data = {"dmesg.txt": text,
                "lspci-nnnvvvxxxx.txt": "00:1f.3 SMBus [8086:2930]\n\tInterrupt: pin B routed to IRQ 17\n"}
        d6.observations(data, {"00:1d.1"})
        self.assertEqual(data["dmesg.txt"], text)
        data.pop("lspci-nnnvvvxxxx.txt")
        d6.observations(data, {"00:1d.1"})
        self.assertEqual(data["dmesg.txt"], "")

    def test_pending_mouse_and_rom_rules_are_narrow_and_directional(self):
        rules = json.loads(diff.DEFAULT_ALLOW.read_text())
        mouse_rule = next(entry for entry in rules["platform_omissions"] if entry["device"] == "vmport/vmmouse")
        self.assertEqual((mouse_rule["category"], mouse_rule["decision"]), ("approved", "D15"))
        mouse = "input: VirtualPS/2 VMware VMMouse as /devices/platform/i8042/serio1/input/input<index>\n"
        generic = "input: ImExPS/2 Generic Explorer Mouse as /devices/platform/i8042/serio1/input/input<index>\n"
        nic = "00:03.0 Class [0200]: Device [1af4:1000]\n"
        left = {"dmesg.txt": mouse * 2 + "other mouse changed\n", "lspci-nn.txt": nic,
                "iomem.txt": "000c9000-000c9dff : Adapter ROM\n000ca000-000cc3ff : Adapter ROM\n000d0000-000d0fff : Adapter ROM\n",
                "fwcfg/blobs/genroms/kvmvapic.bin": "captured blob"}
        right = {"dmesg.txt": generic + "other mouse changed\n", "lspci-nn.txt": nic, "iomem.txt": "000d0000-000d0fff : Adapter ROM\n"}
        diff.approved_platform(left, right, rules)
        self.assertEqual(left["dmesg.txt"], right["dmesg.txt"])
        self.assertIn("000c9000-000c9dff : Adapter ROM", left["iomem.txt"])
        self.assertNotIn("000ca000-000cc3ff : Adapter ROM", left["iomem.txt"])
        bad = {"dmesg.txt": generic.replace("ImEx", "Other")}
        left = {"dmesg.txt": mouse * 2}
        diff.approved_platform(left, bad, rules)
        self.assertEqual(left["dmesg.txt"], mouse * 2)
        left = {"iomem.txt": "000c9000-000c9eff : Adapter ROM\n", "lspci-nn.txt": nic}
        diff.approved_platform(left, right, rules)
        self.assertIn("000c9eff", left["iomem.txt"])

    def test_pci_log_addresses_survive_measurement_normalization(self):
        text = "pci 0000:00:05.0: BAR 0 [mem 0xFEB00000-0xFEB03FFF]\n"
        normalized = diff.normalize_text("dmesg.txt", text)
        self.assertIn("0000:00:05.0", normalized)
        self.assertNotIn("FEB00000", normalized)
        self.assertNotEqual(normalized, diff.normalize_text("dmesg.txt", text.replace("05.0", "06.0")))

    def test_virtual_msi_irq_numbers_move_but_gsi_and_vectors_remain_exact(self):
        text = " 25: 1 2 PCI-MSIX-0000:00:01.0 1-edge virtio0-virtqueues\n"
        self.assertEqual(diff.normalize_text("interrupts.txt", text),
                         diff.normalize_text("interrupts.txt", text.replace("25:", "42:")))
        for changed in (text.replace(" 1-edge", " 2-edge"), text.replace("01.0", "02.0")):
            self.assertNotEqual(diff.normalize_text("interrupts.txt", text),
                                diff.normalize_text("interrupts.txt", changed))
        self.assertNotEqual(diff.normalize_text("interrupts.txt", " 9: 1 IO-APIC 9-fasteoi acpi\n"),
                            diff.normalize_text("interrupts.txt", "10: 1 IO-APIC 10-fasteoi acpi\n"))

    def test_d6_aml_parser_removes_only_exact_pci_address_devices(self):
        original = (REFERENCE / "guest/acpi/DSDT").read_bytes()
        slots = {"00:1d.0", "00:1d.1", "00:1d.2", "00:1d.7"}
        canonical, cuts = aml.omit_pci_functions(original, slots)
        self.assertEqual(cuts, [(0x2007, 0x2018), (0x2018, 0x2029),
                                (0x2029, 0x203a), (0x203a, 0x204b)])
        self.assertEqual(len(original) - len(canonical), 68)
        self.assertEqual(sum(canonical) & 255, 0)
        changed = bytearray(original)
        changed[0x2017] ^= 1
        self.assertEqual(aml.omit_pci_functions(changed, slots), (changed, []))
        changed = bytearray(original)
        changed[0x200] ^= 1
        self.assertNotEqual(aml.omit_pci_functions(changed, slots)[0], canonical)
        changed = bytearray(original)
        changed[4] ^= 1
        self.assertEqual(aml.omit_pci_functions(changed, slots), (changed, []))
        source = (REFERENCE / "guest/acpi/DSDT.dsl").read_text()
        reduced = aml.omit_asl_functions(source, slots)
        self.assertNotIn("Device (SE8)", reduced)
        self.assertIn("Device (S28)", reduced)
        xhci_omitted, cuts = aml.omit_pci_functions(original, {"00:05.0"})
        self.assertEqual(len(original) - len(xhci_omitted), 17)
        self.assertEqual(len(cuts), 1)
        source = source.replace("Name (_ADR, 0x001D0000)",
                                "Name (_ADR, 0x001D0000)\nMethod (TEST) { Return (One) }")
        self.assertIn("Device (SE8)", aml.omit_asl_functions(source, slots))

    def test_d6_loader_offsets_change_without_masking_record_contents(self):
        source = (REFERENCE / "guest/fwcfg/blobs/etc/table-loader").read_bytes()
        dsdt = (REFERENCE / "guest/acpi/DSDT").read_bytes()
        _, cuts = aml.omit_pci_functions(dsdt, {"00:1d.0", "00:1d.1", "00:1d.2", "00:1d.7"})
        reduced = aml.relocate_loader(source, cuts)
        self.assertEqual(struct.unpack_from("<I", reduced, 2 * 128 + 68)[0], len(dsdt) - 68)
        self.assertEqual(struct.unpack_from("<I", reduced, 3 * 128 + 116)[0], 0x222b - 68)
        changed = bytearray(source)
        changed[3 * 128 + 120] = 8
        self.assertNotEqual(aml.relocate_loader(changed, cuts), reduced)

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

    def test_d6_aml_allowance_does_not_hide_other_acpi_changes(self):
        slots = {"00:1d.0", "00:1d.1", "00:1d.2", "00:1d.7"}
        inventory = self.root / "guest/lspci-nn.txt"
        inventory.write_text("".join(line for line in inventory.read_text().splitlines(True)
                                     if line.split()[0] not in slots))
        path = self.root / "guest/acpi/DSDT"
        canonical, cuts = aml.omit_pci_functions(path.read_bytes(), slots)
        path.write_bytes(canonical)
        dsl = self.root / "guest/acpi/DSDT.dsl"
        dsl.write_text(aml.omit_asl_functions(dsl.read_text(), slots))
        loader = self.root / "guest/fwcfg/blobs/etc/table-loader"
        loader.write_bytes(aml.relocate_loader(loader.read_bytes(), cuts))
        self.assertIn("=== acpi/DSDT ===", diff.compare(REFERENCE, self.root, None))
        report = diff.compare(REFERENCE, self.root)
        self.assertNotIn("=== acpi/DSDT", report)
        self.assertNotIn("=== fwcfg/blobs/etc/table-loader", report)
        changed = bytearray(canonical)
        changed[0x200] ^= 1
        path.write_bytes(changed)
        self.assertIn("=== acpi/DSDT ===", diff.compare(REFERENCE, self.root))

    def test_phase_2a_omissions_have_categories_and_preserve_other_failures(self):
        rules = json.loads(diff.DEFAULT_ALLOW.read_text())
        self.assertEqual({entry["category"] for entry in rules["artifact_omissions"]},
                         {"deferred to 2c", "approved"})
        names = {"etc/smi/features-ok", "etc/smi/requested-features", "etc/smi/supported-features",
                 "etc/tpm/log", "genroms/kvmvapic.bin", "genroms/linuxboot_dma.bin"}
        for name in names:
            (self.root / "guest/fwcfg/blobs" / name).unlink()
        # A missing blob whose sysfs name still exists is a failed capture,
        # not an approved missing firmware interface.
        self.assertIn("fwcfg/blobs/etc/smi/features-ok: reference only", diff.compare(REFERENCE, self.root))
        for filename in ("files.txt", "sizes.txt"):
            path = self.root / "guest/fwcfg" / filename
            path.write_text("".join(line for line in path.read_text().splitlines(True)
                                    if line.split()[0].removeprefix("/sys/firmware/qemu_fw_cfg/by_name/") not in names))
        self.assertEqual(diff.compare(REFERENCE, self.root), "")
        self.assertIn("genroms/kvmvapic.bin: reference only", diff.compare(REFERENCE, self.root, None))
        path = self.root / "guest/fwcfg/blobs/etc/e820"
        path.write_bytes(path.read_bytes() + b"\0")
        self.assertIn("=== fwcfg/blobs/etc/e820 ===", diff.compare(REFERENCE, self.root))

    def test_deferred_file_changes_and_new_files_remain_failures(self):
        path = self.root / "guest/fwcfg/blobs/etc/smi/features-ok"
        path.write_bytes(b"\0")
        self.assertIn("=== fwcfg/blobs/etc/smi/features-ok ===", diff.compare(REFERENCE, self.root))
        path = self.root / "guest/fwcfg/blobs/etc/smi/new-feature"
        path.write_bytes(b"\1")
        self.assertIn("etc/smi/new-feature: candidate only", diff.compare(REFERENCE, self.root))

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
    def test_modern_seabios_disk_boot_matches_production_profile_argv(self):
        class Assets:
            bios_disk = Path("/assets/bios-disk.img")
        with tempfile.TemporaryDirectory(prefix="hwref-argv-") as tmp:
            run = Path(tmp)
            paths = capture.RunPaths(run, run, 1)
            cfg = capture.CONFIGS["q35-seabios-linux-modern"]
            with patch.object(capture, "sh"):
                argv = capture.build_argv(cfg, paths, Assets(), run, "q35-seabios-linux-modern")
            self.assertEqual(argv, [
                "-name", "vmlab:hwref/q35-seabios-linux-modern",
                "-machine", "q35", "-accel", "kvm", "-cpu", capture.CPU_HOST,
                "-smp", "2", "-m", "4096M", "-display", "none",
                "-vnc", f"unix:{paths.vnc_sock},share=ignore",
                "-qmp", f"unix:{paths.qmp_sock},server=on,wait=off", "-monitor", "none",
                "-serial", f"file:{run / 'serial.log'}",
                "-chardev", f"socket,id=vagent0,path={paths.agent_sock},server=on,wait=off",
                "-device", "virtio-serial-pci",
                "-device", "virtserialport,chardev=vagent0,name=vmlab.agent.0",
                "-blockdev", f"driver=qcow2,node-name=disk0,file.driver=file,file.filename={run / 'disk0.qcow2'}",
                "-device", "virtio-blk-pci,drive=disk0,bootindex=50",
                "-netdev", f"stream,id=net0,server=off,addr.type=unix,addr.path={paths.nic_sock[0]}",
                "-device", "virtio-net-pci,netdev=net0,mac=52:54:00:00:00:01",
                "-device", "virtio-vga", "-usb", "-device", "usb-tablet", "-S",
            ])

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

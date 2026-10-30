import importlib.util
import re
import shutil
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location("capture", Path(__file__).with_name("capture.py"))
capture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capture)


class ParserTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="hwref-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_pci_regions_domains_and_block_boundaries(self):
        nn = ("0000:00:01.0 Ethernet controller [0200]: Test NIC [1af4:1000]\n"
              "0000:00:02.0 VGA compatible controller [0300]: Test VGA [1234:1111]\n")
        (self.root / "lspci-nn.txt").write_text(nn)
        (self.root / "lspci-nnnvvvxxxx.txt").write_text(
            nn.splitlines()[0] + "\n"
            "\tSubsystem: Test subsystem [1af4:0001]\n"
            "\tRegion 0: I/O ports at c000 [size=32]\n"
            "\tRegion 1: Memory at fe000000 (64-bit, prefetchable) [size=4K]\n"
            "\tCapabilities: [40] MSI-X\n" + nn.splitlines()[1] + "\n"
            "\tExpansion ROM at ff000000 [disabled] [size=64K]\n")
        rows = capture.parse_pci(self.root)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["class"], "0200")
        self.assertEqual(rows[0]["sub"], "1af4:0001 Test subsystem")
        self.assertEqual(rows[0]["bars"], "I/O ports at c000 [size=32]; "
                         "Memory at fe000000 (64-bit, prefetchable) [size=4K]")
        self.assertEqual(rows[1]["bars"], "Expansion ROM at ff000000 [disabled] [size=64K]")

    def test_pci_short_bdf_and_unprefixed_bar(self):
        line = "00:01.0 Ethernet controller [0200]: Test NIC [1af4:1000]\n"
        (self.root / "lspci-nn.txt").write_text(line)
        (self.root / "lspci-nnnvvvxxxx.txt").write_text(
            line + "\tMemory at fe000000 [size=4K]\n\tKernel driver in use: test\n")
        self.assertEqual(capture.parse_pci(self.root)[0]["bars"],
                         "Memory at fe000000 [size=4K]")

    def test_acpi_facs_and_non_table_files(self):
        acpi = self.root / "acpi"
        acpi.mkdir()
        header = bytearray(struct.pack("<4sIBB6s8sI4sI", b"DSDT", 36, 2, 0,
                                       b"BOCHS ", b"BXPC    ", 1, b"BXPC", 1))
        header[9] = -sum(header) & 255
        (acpi / "DSDT").write_bytes(header)
        (acpi / "dynamic-SSDT1").write_bytes(header.replace(b"DSDT", b"SSDT", 1))
        facs = bytearray(64)
        struct.pack_into("<4sI", facs, 0, b"FACS", 64)
        facs[32] = 2
        (acpi / "FACS").write_bytes(facs)
        (acpi / "DSDT.dsl").write_text("/* disassembled table */" * 4)
        (acpi / "copy-errors.log").write_text("diagnostic" * 8)
        rows = capture.parse_acpi(self.root)
        self.assertEqual([r["name"] for r in rows], ["DSDT", "FACS", "dynamic-SSDT1"])
        self.assertEqual(rows[0]["tblid"], "BXPC")
        self.assertEqual(rows[1]["rev"], 2)
        self.assertEqual(rows[1]["oem"], "—")

    def test_acpi_missing_directory(self):
        self.assertEqual(capture.parse_acpi(self.root), [])

    def write_dmi(self, data):
        directory = self.root / "dmi-tables"
        directory.mkdir(exist_ok=True)
        (directory / "DMI").write_bytes(data)

    def test_smbios_short_type_one_and_record_boundary(self):
        self.write_dmi(bytes([1, 8, 0, 0, 1, 2, 0, 0]) + b"Vendor\0Product\0\0"
                       + bytes([127, 4, 0, 0, 0, 0]))
        result = capture.parse_smbios(self.root)
        self.assertEqual(result["manufacturer"], "Vendor")
        self.assertEqual(result["product"], "Product")
        self.assertNotIn("uuid", result)
        self.assertNotIn("sku", result)

    def test_smbios_truncated_records(self):
        for data in (bytes([1, 27, 0, 0]), bytes([0, 4, 0, 0, 0, 0]),
                     bytes([1, 8, 0, 0, 1, 0, 0, 0]) + b"unterminated"):
            with self.subTest(data=data):
                self.write_dmi(data)
                with self.assertRaises(ValueError):
                    capture.parse_smbios(self.root)

    def test_smbios_full_type_one_and_end_marker(self):
        record = bytearray([1, 27, 0, 0, 1, 2, 0, 0] + list(range(16)) + [0, 3, 4])
        self.write_dmi(record + b"Vendor\0Product\0SKU\0Family\0\0"
                       + bytes([127, 4, 0, 0, 0, 0]) + b"trailing garbage")
        result = capture.parse_smbios(self.root)
        self.assertEqual(result["uuid"], "03020100-0504-0706-0809-0a0b0c0d0e0f")
        self.assertEqual(result["sku"], "SKU")
        self.assertEqual(result["family"], "Family")


@unittest.skipUnless(shutil.which("gcc"), "gcc is required to exercise CPUID_C")
class CpuidTests(unittest.TestCase):
    def test_enumeration_with_synthetic_cpuid(self):
        stub = """static void cpuid(uint32_t leaf, uint32_t sub, uint32_t r[4]) {
    r[0] = r[1] = r[2] = r[3] = 0;
    switch (leaf) {
    case 0: r[0] = 0x1f; break;
    case 0x40000000: r[0] = 0x40000003; break;
    case 0x80000000: r[0] = 0x80000001; break;
    case 7:
        if (sub == 0) r[0] = 2;
        if (sub == 1) r[1] = 1;
        if (sub == 2) r[0] = 1;
        break;
    case 0xd:
        if (sub == 0) { r[0] = 4; r[3] = 8; }
        if (sub == 1) { r[2] = 1u << 11; r[3] = 1u << 8; }
        break;
    case 0x1f:
        if (sub < 2) { r[1] = 1u << sub; r[2] = (sub + 1) << 8; }
        break;
    case 0x12:
        if (sub == 0 || sub == 2) r[0] = 1;
        break;
    }
}
"""
        start = capture.CPUID_C.index("static void cpuid(")
        end = capture.CPUID_C.index("static void emit(", start)
        source = capture.CPUID_C[:start] + stub + capture.CPUID_C[end:]
        with tempfile.TemporaryDirectory(prefix="hwref-cpuid-test-") as tmp:
            executable = Path(tmp) / "cpuid-test"
            subprocess.run(["gcc", "-Wall", "-Wextra", "-Werror", "-x", "c", "-",
                            "-o", str(executable)], input=source, text=True, check=True,
                           capture_output=True)
            output = subprocess.run([str(executable)], check=True, text=True,
                                    capture_output=True, timeout=5).stdout
        leaves = {(int(a, 16), int(b, 16)) for a, b in
                  re.findall(r"^(0x[0-9a-f]+) (0x[0-9a-f]+):", output, re.M)}
        self.assertEqual({pair for pair in leaves if pair[0] == 0x40000000},
                         {(0x40000000, 0)})
        for leaf in range(0x40000000, 0x40000004):
            self.assertIn((leaf, 0), leaves)
        self.assertIn((0x80000000, 0), leaves)
        self.assertEqual({s for leaf, s in leaves if leaf == 7}, {0, 1, 2})
        self.assertEqual({s for leaf, s in leaves if leaf == 0xd}, {0, 1, 2, 11, 35, 40})
        self.assertEqual({s for leaf, s in leaves if leaf == 0x1f}, {0, 1, 2})
        self.assertEqual({s for leaf, s in leaves if leaf == 0x12}, {0, 1, 2, 3})


class ReferenceTests(unittest.TestCase):
    def test_serial_crlf_and_corrupt_stream(self):
        payload = b"test archive bytes"
        digest = capture.hashlib.sha256(payload).hexdigest()
        stream = (f"===HWREF-BEGIN {digest} {len(payload)}===\r\n".encode()
                  + capture.base64.b64encode(payload) + b"\r\n===HWREF-END===\r\n")
        self.assertEqual(capture.decode_stream(stream), payload)
        self.assertIsNone(capture.decode_stream(stream.replace(digest.encode(), b"0" * 64)))

    def test_checked_in_captures_are_complete(self):
        for name, cfg in capture.CONFIGS.items():
            with self.subTest(config=name):
                directory = capture.DOCS / name
                guest = directory / "guest"
                self.assertNotIn("CAPTURE FAILED", (directory / "README.md").read_text())
                self.assertGreater(len(capture.parse_pci(guest)), 5)
                self.assertGreater(len(capture.parse_acpi(guest)), 4)
                self.assertTrue(capture.parse_smbios(guest)["product"])
                self.assertIn("0x40000000", (guest / "cpuid.txt").read_text())
                self.assertIn("etc/e820", (guest / "fwcfg/sizes.txt").read_text())
                self.assertTrue((guest / "smbios.bin").stat().st_size)
                self.assertIn("e820", (guest / "e820-dmesg.txt").read_text())
                self.assertIn("PNP", (guest / "pnp.txt").read_text())
                self.assertIn("CPU0", (guest / "interrupts.txt").read_text())
                self.assertEqual((guest / "modprobe.log").read_text(), "")
                raw = (directory / ("serial.log" if cfg["stream"] == "serial2"
                                    else "agent-channel.log")).read_bytes()
                self.assertIsNotNone(capture.decode_stream(raw))
                if cfg.get("tpm"):
                    self.assertTrue((guest / "acpi/TPM2").exists())
                if cfg.get("usb"):
                    self.assertIn("1b36:000d", {r["id"] for r in capture.parse_pci(guest)})


if __name__ == "__main__":
    unittest.main()

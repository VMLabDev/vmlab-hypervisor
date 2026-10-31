"""Checks for the snapshot differential's documented clock exclusions."""

import importlib.util
from pathlib import Path
import struct
import unittest


SPEC = importlib.util.spec_from_file_location("snapshot_audit", Path(__file__).with_name("snapshot-audit.py"))
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)


class NormalizationTests(unittest.TestCase):
    def test_clock_msrs_do_not_hide_cet_or_spec_ctrl(self):
        entries = [(0x10, 10), (0x40000010, 20), (0x40000020, 30), (0x6A0, 1), (0x6A7, 0xA000), (0x48, 2)]
        encoded = struct.pack("<I", len(entries)) + b"".join(struct.pack("<IQ", *entry) for entry in entries)
        expected = {"0x000006a0": 1, "0x000006a7": 0xA000, "0x00000048": 2}
        self.assertEqual(AUDIT.normalize("vcpu0/msrs", list(encoded)), expected)
        self.assertEqual(AUDIT.normalize("vcpu0/all-msrs", {f"{index:#010x}": value for index, value in entries}), expected)

    def test_malformed_msrs_are_rejected(self):
        with self.assertRaises(AssertionError):
            AUDIT.normalize("vcpu0/msrs", list(struct.pack("<I", 1)))

    def test_tsc_offsets_preserve_inter_cpu_skew(self):
        before = {"clock": 1, "host_tsc": 2, "khz": 1000, "offsets": [[0, 100], [1, 110]]}
        after = {"clock": 100, "host_tsc": 200, "khz": 1000, "offsets": [[0, 200], [1, 210]]}
        self.assertEqual(AUDIT.normalize("tsc-offsets", before), AUDIT.normalize("tsc-offsets", after))
        after["offsets"][1][1] += 1
        self.assertNotEqual(AUDIT.normalize("tsc-offsets", before), AUDIT.normalize("tsc-offsets", after))

    def test_pit_preserves_every_non_timestamp_byte(self):
        for offset in range(112):
            changed = [0] * 112
            changed[offset] = 1
            ignored = offset < 72 and offset % 24 >= 16
            self.assertEqual(AUDIT.normalize("pit", changed) == [0] * 112, ignored, offset)

    def test_lapic_preserves_interrupts_and_timer_configuration(self):
        for offset in range(1024):
            changed = [0] * 1024
            changed[offset] = 1
            self.assertEqual(AUDIT.normalize("vcpu0/lapic", changed) == [0] * 1024, 0x390 <= offset < 0x394, offset)

    def test_kvmclock_preserves_flags_and_padding(self):
        for offset in range(48):
            changed = [0] * 48
            changed[offset] = 1
            ignored = offset < 8 or 16 <= offset < 32
            self.assertEqual(AUDIT.normalize("kvmclock", changed) == AUDIT.normalize("kvmclock", [0] * 48), ignored, offset)


if __name__ == "__main__":
    unittest.main()

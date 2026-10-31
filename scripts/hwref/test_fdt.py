import struct
import subprocess
import unittest

import fdt


def compile_tree(handle=1, irq=10, entropy=0):
    source = f"""/dts-v1/;
    / {{ #address-cells = <2>; #size-cells = <2>;
      chosen {{ rng-seed = <{entropy}>; }};
      intc {{ phandle = <{handle}>; #address-cells = <0>;
        #interrupt-cells = <1>; interrupt-controller; }};
      uart@10000000 {{ reg = <0 0x10000000 0 0x100>;
        interrupt-parent = <{handle}>; interrupts = <{irq}>;
        interrupts-extended = <{handle} {irq}>; }};
    }};"""
    return subprocess.run(["dtc", "-I", "dts", "-O", "dtb"], input=source.encode(),
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout


class FdtTests(unittest.TestCase):
    def test_allocation_and_entropy_do_not_hide_hardware_changes(self):
        original = compile_tree()
        self.assertEqual(fdt.normalize(original), fdt.normalize(compile_tree(4, entropy=123)))
        self.assertNotEqual(fdt.normalize(original), fdt.normalize(compile_tree(irq=11)))

    def test_malformed_offsets_and_missing_end_fail(self):
        original = compile_tree()
        for length in range(40):
            with self.assertRaises(ValueError):
                fdt.parse(original[:length])
        broken = bytearray(original)
        struct.pack_into(">I", broken, 8, len(original) + 4)
        with self.assertRaises(ValueError):
            fdt.parse(broken)
        structure, = struct.unpack_from(">I", original, 8)
        size, = struct.unpack_from(">I", original, 36)
        broken = bytearray(original)
        struct.pack_into(">I", broken, structure + size - 4, 4)
        with self.assertRaises(ValueError):
            fdt.parse(broken)

    def test_memory_addresses_are_preserved(self):
        nodes, reserved, boot = fdt.parse(compile_tree())
        self.assertEqual(nodes["/uart@10000000"]["reg"], struct.pack(">4I", 0, 0x10000000, 0, 0x100))
        self.assertEqual(reserved, [])
        self.assertEqual(boot, 0)

    def test_subset_policy_does_not_approve_hardware_changes(self):
        def tree(isa, irq=10):
            source = f'''/dts-v1/; / {{ cpus {{ cpu@0 {{ riscv,isa = "{isa}"; }}; }};
                uart {{ interrupts = <{irq}>; }}; }};'''
            return subprocess.run(["dtc", "-I", "dts", "-O", "dtb"], input=source.encode(),
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout
        reference = tree("rv64imafdcbvh")
        subset = tree("rv64imafdc_zicsr_zifencei")
        self.assertIsNone(fdt.compare(reference, subset)[0]["reason"])
        differences = fdt.compare(reference, subset, True)
        self.assertEqual(len(differences), 1)
        self.assertTrue(differences[0]["reason"].startswith("D7:"))
        differences = fdt.compare(reference, tree("rv64imafdc_zicsr_zifencei", 11), True)
        self.assertEqual(len(differences), 2)
        self.assertIsNone(differences[1]["reason"])


if __name__ == "__main__":
    unittest.main()

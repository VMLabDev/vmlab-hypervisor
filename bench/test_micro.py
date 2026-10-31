import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import lib
import micro
import report
import run


class MicroTests(unittest.TestCase):
    def test_coremark_build_keeps_iteration_counts_and_artifacts_distinct(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "target/tcg/coremark").mkdir(parents=True)
            with mock.patch.object(lib, "REPO_DIR", root), \
                    mock.patch.object(subprocess, "check_output", side_effect=[micro.COREMARK_REVISION + "\n", ""]), \
                    mock.patch.object(subprocess, "run") as execute:
                image = micro.build_coremark(1000)
                self.assertEqual(image.name, "coremark-1000.bin")
                compiler_commands = [call.args[0] for call in execute.call_args_list if call.args[0][0] == "clang"]
                self.assertEqual(len(compiler_commands), 6)
                self.assertTrue(all("-DITERATIONS=1000" in command for command in compiler_commands))

    def test_coremark_rejects_nonpositive_iterations(self):
        for iterations in [0, -1]:
            with self.assertRaises(lib.BenchError):
                micro.build_coremark(iterations)

    def test_actual_json_measurement(self):
        result = micro.measure([sys.executable, "-c", 'print(\'{"instructions": 100, "seconds": 0.25}\')'], dict(os.environ))
        self.assertEqual(result["instructions"], 100)

    def test_guest_failure_is_not_a_measurement(self):
        with self.assertRaises(lib.BenchError):
            micro.measure([sys.executable, "-c", "raise SystemExit(7)"], dict(os.environ))

    def test_timeout_reaps_the_process(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            micro.measure([sys.executable, "-c", "import time; time.sleep(60)"], dict(os.environ), timeout=0.05)

    def test_backend_routes_without_template_lookup(self):
        self.assertEqual(run.main(["--backend", "interpreter", "--arch", "riscv64", "--vcpus", "1", "--dry-run"]), 0)

    def test_backend_selection_ignores_conflicting_inherited_flags(self):
        inherited = {"VMLAB_TCG_SPECIALIZE": "1", "VMLAB_TCG_WARM": "1", "VMLAB_TCG_NO_MEMO": "1", "VMLAB_TCG_PROFILE": "1", "VMLAB_TCG_TRACE_PC": "all", "KEEP": "value"}
        self.assertEqual(micro.execution_environment("interpreter", inherited), {"KEEP": "value"})
        reference = micro.execution_environment("interpreter-reference", inherited)
        self.assertEqual(reference, {"KEEP": "value", "VMLAB_TCG_NO_MEMO": "1", "VMLAB_TCG_NO_INDEXED": "1"})
        self.assertEqual(micro.execution_environment("interpreter-specialized", reference), {"KEEP": "value", "VMLAB_TCG_SPECIALIZE": "1", "VMLAB_TCG_WARM": "1"})

    def test_specialized_backend_routes_without_template_lookup(self):
        self.assertEqual(run.main(["--backend", "interpreter-specialized", "--arch", "riscv64", "--vcpus", "1", "--dry-run"]), 0)

    def test_qemu_flat_backend_routes_without_template_lookup(self):
        self.assertEqual(run.main(["--backend", "qemu", "--arch", "riscv64", "--vcpus", "1",
                                   "--guest", "flat-rv64gc", "--dry-run"]), 0)


class QemuFlatTests(unittest.TestCase):
    def loop_image_bytes(self, stub_words):
        return b"\0" * (4 * stub_words) + self.loop_block() + b"\0" * 8

    @staticmethod
    def loop_block():
        return b"".join(word.to_bytes(4, "little") for word in micro.LOOP_WORDS)

    def test_build_loop_compiles_the_workload_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with mock.patch.object(lib, "REPO_DIR", root), \
                    mock.patch.object(subprocess, "run",
                                      side_effect=self._fake_toolchain) as execute:
                elf, image = micro.build_loop(10000000)
            self.assertEqual(elf.name, "loop-10000000.elf")
            self.assertEqual(image.name, "loop-10000000.bin")
            commands = [call.args[0] for call in execute.call_args_list]
            compile_command = next(c for c in commands if c[0] == "clang")
            self.assertIn("-DLOOP_ITERATIONS=10000000", compile_command)
            self.assertTrue(any(argument.endswith("loop.S") for argument in compile_command))
            self.assertTrue(any(argument.endswith("link.ld") for argument in compile_command))
            objcopy_command = next(c for c in commands if c[0] == "llvm-objcopy")
            self.assertIn("binary", objcopy_command)

    @staticmethod
    def _fake_toolchain(argv, **kwargs):
        if argv[0] == "llvm-objcopy":
            # A two-instruction zero stub keeps the loop block at +8.
            Path(argv[-1]).write_bytes(b"\0" * 8 + QemuFlatTests.loop_block() + b"\0" * 16)
        return subprocess.CompletedProcess(argv, 0)

    def test_build_loop_rejects_nonpositive_iterations(self):
        for iterations in [0, -1]:
            with self.assertRaises(lib.BenchError):
                micro.build_loop(iterations)

    def test_check_loop_image_accepts_both_stub_lengths(self):
        self.assertIsNone(micro.check_loop_image(self.loop_image_bytes(1)))
        self.assertIsNone(micro.check_loop_image(self.loop_image_bytes(2)))

    def test_check_loop_image_rejects_missing_or_duplicated_block(self):
        with self.assertRaises(lib.BenchError):
            micro.check_loop_image(b"\0" * 32)
        with self.assertRaises(lib.BenchError):
            micro.check_loop_image(self.loop_image_bytes(1) + self.loop_block())

    def test_loop_expected_instructions_match_measured_counts(self):
        # The interpreter measured 10 for one iteration and 30,000,008 for
        # ten million (a two-instruction li stub above the 12-bit immediate).
        self.assertEqual(micro.loop_expected_instructions(1), 10)
        self.assertEqual(micro.loop_expected_instructions(2047), 3 * 2047 + 7)
        self.assertEqual(micro.loop_expected_instructions(2048), 3 * 2048 + 8)
        self.assertEqual(micro.loop_expected_instructions(micro.LOOP_ITERATIONS),
                         3 * micro.LOOP_ITERATIONS + 8)

    def test_qemu_argv_boots_a_minimal_spike_machine(self):
        argv = micro.qemu_argv("/tmp/loop-10000000.elf")
        self.assertEqual(argv[0], "qemu-system-riscv64")
        pairs = [argv[i:i + 2] for i in range(len(argv) - 1)]
        for flag, value in (("-machine", "spike"), ("-cpu", "rv64"), ("-accel", "tcg"),
                            ("-smp", "1"), ("-m", "128M"), ("-bios", "none"),
                            ("-kernel", "/tmp/loop-10000000.elf")):
            self.assertIn([flag, value], pairs)
        for absent in ("-drive", "-netdev", "-device", "-cdrom"):
            self.assertNotIn(absent, argv)
        self.assertEqual(argv[-6:], ["-display", "none", "-serial", "none", "-monitor", "none"])

    def test_measure_qemu_times_a_successful_run(self):
        wall, out, _ = micro.measure_qemu([sys.executable, "-c", "print('ok')"], dict(os.environ))
        self.assertGreater(wall, 0.0)
        self.assertEqual(out.strip(), "ok")

    def test_measure_qemu_guest_failure_is_an_error(self):
        with self.assertRaises(lib.BenchError):
            micro.measure_qemu([sys.executable, "-c", "raise SystemExit(7)"], dict(os.environ))

    def test_measure_qemu_timeout_reaps_the_process(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            micro.measure_qemu([sys.executable, "-c", "import time; time.sleep(60)"],
                               dict(os.environ), timeout=0.05)

    def test_execution_environment_clears_flags_for_qemu(self):
        inherited = {"VMLAB_TCG_SPECIALIZE": "1", "VMLAB_TCG_WARM": "1", "KEEP": "value"}
        self.assertEqual(micro.execution_environment("qemu", inherited), {"KEEP": "value"})


class ReportFlatRatioTests(unittest.TestCase):
    def record(self, backend, workload, median):
        return {"recorded": "2026-10-31T00:00:00", "commit": "0" * 40, "dirty": False,
                "backend": backend, "arch": "riscv64", "guest": "flat-rv64gc", "vcpus": 1,
                "workload": workload, "unit": "s", "median": median, "spread": 0.1}

    def test_interpreter_groups_print_the_d1_ratio_against_qemu(self):
        records = [self.record("interpreter-specialized", "coremark_algorithms_1000_warm", 6.0),
                   self.record("qemu", "coremark_algorithms_1000_warm", 3.0)]
        with mock.patch.object(lib, "read_history", return_value=records), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            report.print_arch("riscv64", report.parse_args([]))
        self.assertIn("D1 bar: last interpreter-specialized median 6.00s vs qemu 3.00s",
                      out.getvalue())
        self.assertIn("= 2.00x -> BELOW D1", out.getvalue())

    def test_qemu_groups_do_not_print_a_ratio_against_themselves(self):
        records = [self.record("qemu", "scalar_loop_30000000_warm", 0.5)]
        with mock.patch.object(lib, "read_history", return_value=records), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            report.print_arch("riscv64", report.parse_args([]))
        self.assertNotIn("D1 bar", out.getvalue())

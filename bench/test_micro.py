import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import lib
import micro
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

    def test_specialized_backend_routes_without_template_lookup(self):
        self.assertEqual(run.main(["--backend", "interpreter-specialized", "--arch", "riscv64", "--vcpus", "1", "--dry-run"]), 0)

    def test_backend_selection_ignores_conflicting_inherited_flags(self):
        inherited = {"VMLAB_TCG_SPECIALIZE": "1", "VMLAB_TCG_WARM": "1", "VMLAB_TCG_NO_MEMO": "1", "VMLAB_TCG_PROFILE": "1", "VMLAB_TCG_TRACE_PC": "all", "KEEP": "value"}
        self.assertEqual(micro.execution_environment("interpreter", inherited), {"KEEP": "value"})
        reference = micro.execution_environment("interpreter-reference", inherited)
        self.assertEqual(reference, {"KEEP": "value", "VMLAB_TCG_NO_MEMO": "1", "VMLAB_TCG_NO_INDEXED": "1"})
        self.assertEqual(micro.execution_environment("interpreter-specialized", reference), {"KEEP": "value", "VMLAB_TCG_SPECIALIZE": "1", "VMLAB_TCG_WARM": "1"})

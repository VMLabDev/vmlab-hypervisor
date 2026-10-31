import os
import subprocess
import sys
import unittest

import lib
import micro
import run


class MicroTests(unittest.TestCase):
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

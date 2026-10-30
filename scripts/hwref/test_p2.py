"""P2 black-box proof. Opt in with HWREF_RECAPTURE=1 on a provisioned KVM host."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
CONFIG = "q35-seabios-linux-generic"


@unittest.skipUnless(os.environ.get("HWREF_RECAPTURE") == "1",
                     "P2 recapture requires HWREF_RECAPTURE=1, KVM, firmware, and probe tools")
class P2Tests(unittest.TestCase):
    def test_two_qemu_boots_match_and_hardware_change_fails(self):
        with tempfile.TemporaryDirectory(prefix="hwref-p2-") as tmp:
            roots = [Path(tmp) / name for name in ("reference", "candidate")]
            for root in roots:
                result = subprocess.run([sys.executable, str(HERE / "capture.py"),
                                         "--output", str(root), CONFIG],
                                        capture_output=True, text=True, timeout=600)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                if "SKIP:" in result.stdout:
                    self.skipTest(result.stdout.strip())
                self.assertTrue((root / CONFIG / "guest").is_dir(), result.stdout + result.stderr)
            command = [sys.executable, str(HERE / "capture.py"), "diff",
                       str(roots[0] / CONFIG), str(roots[1] / CONFIG), "--no-allow-list"]
            result = subprocess.run(command, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(result.stdout, "")
            pci = roots[1] / CONFIG / "guest/lspci-nn.txt"
            pci.write_text(pci.read_text().replace("8086:29c0", "8086:ffff"))
            result = subprocess.run(command, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("=== lspci-nn.txt ===", result.stdout)


if __name__ == "__main__":
    unittest.main()

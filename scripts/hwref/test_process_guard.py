import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

from process_guard import stop_process


class ProcessTests(unittest.TestCase):
    def test_abrupt_capture_exit_stops_its_vm(self):
        with tempfile.TemporaryDirectory() as temporary:
            pid_file = Path(temporary) / "pid"
            environment = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parent))
            ready = Path(temporary) / "ready"
            parent_code = """
from process_guard import spawn_process
import os, pathlib, sys, time
child = spawn_process([sys.executable, '-c',
    'import pathlib,sys,time; pathlib.Path(sys.argv[1]).touch(); time.sleep(600)', sys.argv[2]])
deadline = time.monotonic() + 5
while not pathlib.Path(sys.argv[2]).exists():
    assert child.poll() is None and time.monotonic() < deadline
    time.sleep(0.01)
pathlib.Path(sys.argv[1]).write_text(str(child.pid))
os._exit(0)
"""
            parent = subprocess.Popen([sys.executable, "-c", parent_code,
                                       str(pid_file), str(ready)], env=environment)
            parent.wait(timeout=5)
            pid = int(pid_file.read_text())
            deadline = time.monotonic() + 5
            try:
                while True:
                    try:
                        state = Path(f"/proc/{pid}/stat").read_text().split()[2]
                    except FileNotFoundError:
                        break
                    if state == "Z":
                        break
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
            finally:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_stuck_helper_is_killed_and_reaped(self):
        with tempfile.TemporaryDirectory() as temporary:
            ready = Path(temporary) / "ready"
            process = subprocess.Popen([
                sys.executable, "-c",
                "import signal, pathlib, sys, time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                "pathlib.Path(sys.argv[1]).touch(); time.sleep(600)", str(ready)],
                start_new_session=True)
            try:
                deadline = time.monotonic() + 5
                while not ready.exists():
                    self.assertIsNone(process.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                process.terminate()
                with self.assertRaises(subprocess.TimeoutExpired):
                    process.wait(timeout=0.05)
                stop_process(process)
                self.assertEqual(process.returncode, -signal.SIGKILL)
                self.assertFalse(Path(f"/proc/{process.pid}").exists())
            finally:
                stop_process(process)

    def test_cleanup_kills_descendants_after_parent_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            pid_file = Path(temporary) / "pid"
            process = subprocess.Popen([
                sys.executable, "-c",
                "import subprocess, pathlib, sys; "
                "child = subprocess.Popen(['sleep', '600']); "
                "pathlib.Path(sys.argv[1]).write_text(str(child.pid))", str(pid_file)],
                start_new_session=True)
            try:
                process.wait(timeout=5)
                pid = int(pid_file.read_text())
                stop_process(process)
                deadline = time.monotonic() + 5
                while True:
                    try:
                        state = Path(f"/proc/{pid}/stat").read_text().split()[2]
                    except FileNotFoundError:
                        break
                    # Reparented descendants are reaped by init; a zombie owns no RAM.
                    if state == "Z":
                        break
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
            finally:
                stop_process(process)


if __name__ == "__main__":
    unittest.main()

"""Own capture process groups and share the Rust harness's two VM slots."""

import fcntl
import ctypes
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def spawn_process(argv, **kwargs):
    """Set parent-death signalling in a fresh interpreter, never in preexec_fn.

    Capture programs already have socket-reader threads when they spawn VMs.
    The single-threaded wrapper sets PR_SET_PDEATHSIG, then execs the child in
    place so its PID and process group remain owned by the caller.
    """
    return subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                             str(os.getpid()), *map(str, argv)],
                            start_new_session=True, **kwargs)


class VmPermit:
    def __init__(self):
        limit = int(os.environ.get("VMLAB_HV_TEST_VM_LIMIT", "2"))
        if limit < 1:
            raise ValueError("VMLAB_HV_TEST_VM_LIMIT must be positive")
        directory = Path(tempfile.gettempdir()) / f"vmlab-hv-vm-slots-{os.geteuid()}"
        directory.mkdir(exist_ok=True)
        deadline = time.monotonic() + 600
        while True:
            for slot in range(limit):
                file = (directory / f"{slot}.lock").open("a+b")
                try:
                    fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    self.file = file
                    return
                except BlockingIOError:
                    file.close()
            if time.monotonic() >= deadline:
                raise TimeoutError("waiting for a test VM slot")
            time.sleep(0.02)

    def close(self):
        self.file.close()


def stop_process(process):
    if process is None:
        return
    # Every caller uses start_new_session=True, so PGID is the owned child PID.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


if __name__ == "__main__":
    parent = int(sys.argv[1])
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    if os.getppid() != parent:
        sys.exit(127)
    os.execvp(sys.argv[2], sys.argv[2:])

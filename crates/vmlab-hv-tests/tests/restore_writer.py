"""Guest workload for the S1/P1 online restore regression."""

import hashlib
import os
from pathlib import Path
import signal
import sys
import time


ROOTS = [Path('/var/tmp/restore-test'), Path('/mnt/restore-check')]
PIDS = Path('/run/restore-writer.pids')


def start():
    pids = []
    for cpu in range(2):
        pid = os.fork()
        if pid:
            pids.append(pid)
            continue
        os.setsid()
        with open('/dev/null', 'r+b', buffering=0) as null:
            for fd in range(3):
                os.dup2(null.fileno(), fd)
        os.sched_setaffinity(0, {cpu})
        sequence = 0
        while True:
            for root in ROOTS:
                directory = root / str(cpu)
                directory.mkdir(exist_ok=True)
                payload = os.urandom(65536)
                temporary = directory / 'pending'
                with temporary.open('wb') as output:
                    output.write(hashlib.sha256(payload).hexdigest().encode() + b'\n' + payload)
                    output.flush()
                    os.fsync(output.fileno())
                temporary.replace(directory / f'file-{sequence % 128}')
                fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
                os.fsync(fd)
                os.close(fd)
            sequence += 1
    PIDS.write_text(' '.join(map(str, pids)))


def stop():
    pids = list(map(int, PIDS.read_text().split()))
    for pid in pids:
        os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    for pid in pids:
        status = Path(f'/proc/{pid}/stat')
        while True:
            try:
                state = status.read_text().split()[2]
            except (FileNotFoundError, ProcessLookupError):
                break
            if state == 'Z':
                break
            assert time.monotonic() < deadline, f'writer {pid} did not stop'
            time.sleep(.01)
    for root in ROOTS:
        for cpu in range(2):
            files = list((root / str(cpu)).glob('file-*'))
            assert files, f'writer {cpu} made no progress on {root}'
            for path in files:
                digest, payload = path.read_bytes().split(b'\n', 1)
                assert len(payload) == 65536, path
                assert digest.decode() == hashlib.sha256(payload).hexdigest(), path


{'start': start, 'stop': stop}[sys.argv[1]]()

"""Capture translated keyboard characters from a Linux virtual console."""

import fcntl
import os
from pathlib import Path
import select
import termios
import time
import tty

fd = os.open('/dev/tty1', os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
saved = termios.tcgetattr(fd)
try:
    fcntl.ioctl(fd, 0x5606, 1)  # VT_ACTIVATE, on this disposable guest's tty1.
    tty.setraw(fd)
    termios.tcflush(fd, termios.TCIFLUSH)
    Path('/run/viewer-repeat-ready').touch()
    received = bytearray()
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if select.select([fd], [], [], .1)[0]:
            received.extend(os.read(fd, 4096))
            if b'\r' in received or b'\n' in received:
                break
    Path('/run/viewer-repeat.txt').write_bytes(received)
    assert received.endswith(b'\r'), repr(received)
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
    os.close(fd)

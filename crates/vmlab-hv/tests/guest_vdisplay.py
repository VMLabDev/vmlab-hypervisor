"""Publish C2 frames through Linux's PCI resource mappings, without a GPU."""

import ctypes
import faulthandler
import json
import mmap
import os
from pathlib import Path
import struct
import time

faulthandler.enable()

device = next(path for path in Path('/sys/bus/pci/devices').iterdir()
              if (path / 'vendor').read_text().strip() == '0x1af4'
              and (path / 'device').read_text().strip() == '0x1110')
with (device / 'config').open('r+b', buffering=0) as config:
    config.seek(4)
    command = struct.unpack('<H', config.read(2))[0]
    config.seek(4)
    config.write(struct.pack('<H', command | 2))
with (device / 'resource2').open('r+b', buffering=0) as resource, \
        (device / 'resource0').open('r+b', buffering=0) as registers:
    size = int((device / 'resource').read_text().splitlines()[2].split()[1], 16) \
        - int((device / 'resource').read_text().splitlines()[2].split()[0], 16) + 1
    shared = mmap.mmap(resource.fileno(), size)
    doorbell = mmap.mmap(registers.fileno(), 256)
    assert shared[:8] == b'VMLABDSP'
    assert struct.unpack_from('<II', shared, 8) == (1, 4096)
    atomic = ctypes.CDLL('libatomic.so.1')
    release = getattr(atomic, '__atomic_store_4')
    release.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int]
    address = ctypes.addressof(ctypes.c_char.from_buffer(shared))
    doorbell_address = ctypes.addressof(ctypes.c_char.from_buffer(doorbell))

    def publish(offset, value):
        release(address + offset, value, 3)

    width, height = 80, 48
    stride = width * 4
    buffer_stride = struct.unpack_from('<I', shared, 0x24)[0]
    buffers_offset = struct.unpack_from('<Q', shared, 0x28)[0]
    pattern = b'\x21\x65\xa9\x00' * width * height
    struct.pack_into('<II', shared, 0x100, 1, 1001)
    struct.pack_into('<iiIIIIIII', shared, 0x204, -3, 42, 1, 1, 1, 1, 0, 0, 0)
    shared[0x1000:0x1004] = b'\x03\x04\x05\xff'
    publish(0x200, 1)
    deadline = time.monotonic() + 8
    seq = 0
    request = None
    while time.monotonic() < deadline:
        index = seq % 3
        offset = buffers_offset + index * buffer_stride
        shared[offset:offset + len(pattern)] = pattern
        struct.pack_into('<IIII', shared, 0x118, width, height, stride, 0x34325258)
        struct.pack_into('<IIII', shared, 0x128, 0, 0, 0, 0)
        struct.pack_into('<I', shared, 0x114, index)
        struct.pack_into('<Q', shared, 0x108, time.monotonic_ns())
        seq += 1
        publish(0x110, seq)
        # MMIO requires a scalar register store, not libc's vectorized memcpy.
        release(doorbell_address + 12, 0, 3)
        request = struct.unpack_from('<III', shared, 0x34)
        struct.pack_into('<I', shared, 0x138, request[2])
        time.sleep(.05)
    print(json.dumps({'pci': device.name, 'frames': seq, 'request': request}))
    doorbell.close()
    shared.close()

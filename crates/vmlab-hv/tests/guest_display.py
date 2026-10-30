"""P3 guest probe: Linux input events and DRM modesetting through public UAPI."""

import array
import ctypes
import fcntl
import glob
import json
import mmap
import os
from pathlib import Path
import selectors
import struct
import sys
import time


def record():
    selector = selectors.DefaultSelector()
    for path in glob.glob('/dev/input/event*'):
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        name = Path('/sys/class/input', Path(path).name, 'device/name').read_text().strip()
        selector.register(fd, selectors.EVENT_READ, (name, bytearray()))
    assert selector.get_map(), 'no Linux input devices'
    with open('/run/p3-events.jsonl', 'w', buffering=1) as output:
        Path('/run/p3-input-ready').touch()
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            for key, _ in selector.select(.2):
                name, pending = key.data
                pending.extend(os.read(key.fd, 4096))
                while len(pending) >= 24:
                    _, _, kind, code, value = struct.unpack_from('qqHHi', pending)
                    del pending[:24]
                    output.write(json.dumps([name, kind, code, value]) + '\n')


def check():
    expected = [('keyboard', 1, 30, 1), ('keyboard', 1, 30, 0),
                ('keyboard', 1, 48, 1), ('keyboard', 1, 48, 0),
                ('Tablet', 3, 0, 16383), ('Tablet', 3, 1, 16383),
                ('Tablet', 1, 272, 1), ('Tablet', 1, 272, 0),
                ('Tablet', 1, 274, 1), ('Tablet', 1, 274, 0), ('Tablet', 2, 8, -1),
                ('Tablet', 3, 0, 5044), ('Tablet', 3, 1, 12800)]
    deadline = time.monotonic() + 15
    while True:
        events = [json.loads(line) for line in Path('/run/p3-events.jsonl').read_text().splitlines()]
        missing = [event for event in expected if not any(
            event[0] in name and [kind, code, value] == list(event[1:])
            for name, kind, code, value in events)]
        if not missing:
            print(json.dumps({'events': len(events), 'verified': expected}))
            return
        if time.monotonic() > deadline:
            raise AssertionError({'missing': missing, 'events': events})
        time.sleep(.05)


def scanout():
    fd = os.open(sorted(glob.glob('/dev/dri/card*'))[0], os.O_RDWR)

    def ioctl(number, data):
        fcntl.ioctl(fd, 0xc0006400 | (len(data) << 16) | number, data, True)
        return data

    resources = ioctl(0xa0, bytearray(64))
    counts = struct.unpack_from('4I', resources, 32)
    ids = [array.array('I', [0] * count) for count in counts]
    crtcs, connectors = ids[1:3]
    struct.pack_into('4Q', resources, 0, *(values.buffer_info()[0] for values in ids))
    ioctl(0xa0, resources)
    assert crtcs and connectors, 'DRM has no CRTC or connector'
    connector = connectors[0]
    crtc = crtcs[0]
    selected = None
    for _ in range(100):
        info = bytearray(80)
        struct.pack_into('I', info, 48, connector)
        ioctl(0xa7, info)
        count = struct.unpack_from('I', info, 32)[0]
        modes = bytearray(count * 68)
        if count:
            struct.pack_into('II', info, 36, 0, 0)
            struct.pack_into('Q', info, 8, ctypes.addressof(ctypes.c_char.from_buffer(modes)))
            ioctl(0xa7, info)
            for offset in range(0, len(modes), 68):
                mode = modes[offset:offset + 68]
                if struct.unpack_from('H', mode, 4)[0] == 800 and struct.unpack_from('H', mode, 14)[0] == 600:
                    selected = mode
                    break
        if selected:
            break
        time.sleep(.05)
    assert selected, '800x600 is absent from DRM connector modes'

    def dumb(width, height, pixel):
        data = ioctl(0xb2, bytearray(struct.pack('6IQ', height, width, 32, 0, 0, 0, 0)))
        _, _, _, _, handle, pitch, size = struct.unpack('6IQ', data)
        mapping = ioctl(0xb3, bytearray(struct.pack('IIQ', handle, 0, 0)))
        offset = struct.unpack_from('Q', mapping, 8)[0]
        pixels = mmap.mmap(fd, size, offset=offset)
        for y in range(height):
            pixels[y * pitch:y * pitch + width * 4] = pixel * width
        return handle, pitch, pixels

    handle, pitch, pixels = dumb(800, 600, b'\x30\x60\xa0\xff')
    framebuffer = ioctl(0xae, bytearray(struct.pack('7I', 0, 800, 600, pitch, 32, 24, handle)))
    fb_id = struct.unpack_from('I', framebuffer)[0]
    connector_ids = array.array('I', [connector])
    mode = bytearray(struct.pack('Q7I', connector_ids.buffer_info()[0], 1, crtc, fb_id, 0, 0, 0, 1)) + selected
    ioctl(0xa2, mode)
    cursor, _, cursor_pixels = dumb(64, 64, b'\xff\xff\xff\xff')
    ioctl(0xbb, bytearray(struct.pack('IIiiIIIii', 3, crtc, 123, 234, 64, 64, cursor, 2, 3)))
    if os.fork() == 0:
        with open('/dev/null', 'r+b', buffering=0) as null:
            for stream in range(3):
                os.dup2(null.fileno(), stream)
        time.sleep(45)
        pixels.close()
        cursor_pixels.close()
        os._exit(0)
    print(json.dumps({'width': 800, 'height': 600, 'cursor': [123, 234, 64, 64]}))


{'record': record, 'check': check, 'scanout': scanout}[sys.argv[1]]()

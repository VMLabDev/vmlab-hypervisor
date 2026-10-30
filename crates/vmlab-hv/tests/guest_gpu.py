"""Render a virgl clear and install its scanout through the public DRM UAPI."""

import array
import ctypes as c
import fcntl
import glob
import json
from pathlib import Path
import os
import select
import struct
import time


fd = os.open(sorted(glob.glob('/dev/dri/card*'))[0], os.O_RDWR)
width, height = 1024, 768


def ioctl(number, data):
    fcntl.ioctl(fd, 0xc0006400 | (len(data) << 16) | number, data, True)
    return data


# DRM_VIRTGPU_RESOURCE_CREATE creates a 3D render target and its context.
pitch = width * 4
created = ioctl(0x44, bytearray(struct.pack('14I', 2, 2, 2 | (1 << 18),
                width, height, 1, 1, 0, 0, 1, 0, 0, pitch * height, pitch)))
handle, resource = struct.unpack_from('II', created, 40)
assert handle and resource

# Virgl protocol: CREATE_OBJECT(SURFACE), SET_FRAMEBUFFER_STATE, CLEAR.
# These are the commands Mesa emits for an OpenGL color-buffer clear.
commands = array.array('I', [
    (5 << 16) | (8 << 8) | 1, 1, resource, 2, 0, 0,
    (3 << 16) | 5, 1, 0, 1,
    (8 << 16) | 7, 4,
    *struct.unpack('4I', struct.pack('4f', .7, .2, .1, 1.)), 0, 0, 0,
])
handles = array.array('I', [handle])
execution = bytearray(64)
struct.pack_into('IIQQIi', execution, 0, 2, len(commands) * 4,
                 commands.buffer_info()[0], handles.buffer_info()[0], 1, -1)
ioctl(0x42, execution)
fence = struct.unpack_from('i', execution, 28)[0]
assert fence >= 0
poll = select.poll()
poll.register(fence, select.POLLIN)
assert poll.poll(10000), 'virgl clear fence timed out'
os.close(fence)
renderer = 'virgl DRM 3D color-buffer clear'

resources = ioctl(0xa0, bytearray(64))
ids = [array.array('I', [0] * count) for count in struct.unpack_from('4I', resources, 32)]
struct.pack_into('4Q', resources, 0, *(values.buffer_info()[0] for values in ids))
ioctl(0xa0, resources)
crtc, connector = ids[1][0], ids[2][0]
selected = None
for _ in range(100):
    info = bytearray(80)
    struct.pack_into('I', info, 48, connector)
    ioctl(0xa7, info)
    count = struct.unpack_from('I', info, 32)[0]
    modes = bytearray(count * 68)
    if count:
        struct.pack_into('II', info, 36, 0, 0)
        struct.pack_into('Q', info, 8, c.addressof(c.c_char.from_buffer(modes)))
        ioctl(0xa7, info)
        for offset in range(0, len(modes), 68):
            mode = modes[offset:offset + 68]
            if struct.unpack_from('H', mode, 4)[0] == width and struct.unpack_from('H', mode, 14)[0] == height:
                selected = mode
                break
    if selected:
        break
    time.sleep(.05)
assert selected, 'preferred size is absent from DRM connector modes'
fb = ioctl(0xae, bytearray(struct.pack('7I', 0, width, height, pitch, 32, 24, handle)))
fb_id = struct.unpack_from('I', fb)[0]
connectors = array.array('I', [connector])
mode = bytearray(struct.pack('Q7I', connectors.buffer_info()[0], 1, crtc, fb_id, 0, 0, 0, 1)) + selected
ioctl(0xa2, mode)
Path('/run/gpu-scanout.json').write_text(json.dumps({'renderer': renderer, 'width': width, 'height': height}))
time.sleep(180)

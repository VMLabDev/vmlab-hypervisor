"""Minimal independent RFB 3.8 client used by the S1 display acceptance test."""

import json
import socket
import struct
import sys


with socket.socket(socket.AF_UNIX) as stream:
    stream.settimeout(15)
    stream.connect(sys.argv[1])

    def read(size):
        result = bytearray()
        while len(result) < size:
            data = stream.recv(size - len(result))
            assert data, 'unexpected RFB EOF'
            result.extend(data)
        return bytes(result)

    assert read(12) == b'RFB 003.008\n'
    stream.sendall(b'RFB 003.008\n')
    types = read(read(1)[0])
    assert 1 in types, types
    stream.sendall(b'\1')
    assert read(4) == bytes(4)
    stream.sendall(b'\1')
    width, height = struct.unpack('>HH', read(4))
    pixel_format = read(16)
    assert pixel_format[0] == 32
    read(struct.unpack('>I', read(4))[0])
    stream.sendall(struct.pack('>BBHi', 2, 0, 1, 0))
    stream.sendall(struct.pack('>BBHHHH', 3, 0, 0, 0, width, height))
    assert read(2) == b'\0\0'
    rectangles = struct.unpack('>H', read(2))[0]
    assert rectangles == 1
    x, y, w, h, encoding = struct.unpack('>HHHHi', read(12))
    assert (x, y, w, h, encoding) == (0, 0, width, height, 0)
    pixels = read(width * height * 4)
    assert any(pixels), 'RFB full frame is blank'
    for down in (1, 0):
        stream.sendall(struct.pack('>BBHI', 4, down, 0, ord('b')))
    for buttons in (1, 0):
        stream.sendall(struct.pack('>BBHH', 5, buttons, 123, 234))
    print(json.dumps({'width': width, 'height': height, 'full_frame_bytes': len(pixels)}))

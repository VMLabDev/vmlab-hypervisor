"""A minimal ALSA PCM client for guests without alsa-utils.

The ubuntu-26.04 template is a cloud image: it binds snd_hda_intel to the
hypervisor's HDA controller but ships no aplay or arecord. This script talks
to /dev/snd directly through the public ALSA kernel interface (uapi
sound/asound.h), which is the same contract alsa-utils sits on:

    python3 alsa.py unmute                           # amps to 0 dB, like a session
    python3 alsa.py play <rate> <channels>          < pcm.raw
    python3 alsa.py record <rate> <channels> <sec>  > pcm.raw

PCM is interleaved signed 16-bit little-endian on stdin/stdout. `unmute`
drives the same amplifier verbs a desktop session's mixer would.
Diagnostics (chosen buffer and period) go to stderr.
"""

import ctypes
import errno
import fcntl
import os
import struct
import sys
import time

FORMAT_S16_LE = 2
ACCESS_RW_INTERLEAVED = 3
SUBFORMAT_STD = 0

# Hardware-parameter slots, by their snd_pcm_hw_param index.
PARAM_ACCESS, PARAM_FORMAT, PARAM_SUBFORMAT = 0, 1, 2  # mask slots
PARAM_CHANNELS, PARAM_RATE = 5, 6  # interval slots; interval i is index i - 3


class Mask(ctypes.Structure):
    _fields_ = [("bits", ctypes.c_uint32 * 8)]


class Interval(ctypes.Structure):
    _fields_ = [
        ("min", ctypes.c_uint32),
        ("max", ctypes.c_uint32),
        ("flags", ctypes.c_uint32),
    ]


class HwParams(ctypes.Structure):
    _fields_ = [
        ("flags", ctypes.c_uint32),
        ("masks", Mask * 3),
        ("mres", Mask * 5),
        ("intervals", Interval * 12),
        ("ires", Interval * 9),
        ("rmask", ctypes.c_uint32),
        ("cmask", ctypes.c_uint32),
        ("info", ctypes.c_uint32),
        ("msbits", ctypes.c_uint32),
        ("rate_num", ctypes.c_uint32),
        ("rate_den", ctypes.c_uint32),
        ("fifo_size", ctypes.c_ulong),
        ("reserved", ctypes.c_ubyte * 64),
    ]


def _ioc(direction, kind, number, size):
    return (direction << 30) | (size << 16) | (ord(kind) << 8) | number


def _io(kind, number):
    return _ioc(0, kind, number, 0)


def _iowr(kind, number, size):
    return _ioc(3, kind, number, size)


IOCTL_HW_PARAMS = _iowr("A", 0x11, ctypes.sizeof(HwParams))
IOCTL_HW_REFINE = _iowr("A", 0x10, ctypes.sizeof(HwParams))
IOCTL_HW_FREE = _io("A", 0x12)
IOCTL_PREPARE = _io("A", 0x40)
IOCTL_START = _io("A", 0x42)
IOCTL_DROP = _io("A", 0x43)
IOCTL_DRAIN = _io("A", 0x44)


def negotiate(fd, rate, channels):
    """Pin the format the harness wants; the kernel refines the rest.

    Follows the alsa-lib convention: every mask and interval starts
    full-open, SNDRV_PCM_IOCTL_HW_REFINE narrows them against the device's
    constraints, and SNDRV_PCM_IOCTL_HW_PARAMS accepts the result. With
    ALSA_DEBUG_ANY in the environment nothing is pinned, so a refine of the
    raw constraints can be inspected.
    """
    params = HwParams()
    for mask in params.masks:
        for word in range(8):
            mask.bits[word] = 0xFFFFFFFF
    for interval in params.intervals:
        interval.min = 0
        interval.max = 0xFFFFFFFF
    if not os.environ.get("ALSA_DEBUG_ANY"):
        params.masks[PARAM_ACCESS].bits[0] = 1 << ACCESS_RW_INTERLEAVED
        params.masks[PARAM_FORMAT].bits[0] = 1 << FORMAT_S16_LE
        params.masks[PARAM_SUBFORMAT].bits[0] = 1 << SUBFORMAT_STD
        for slot, value in ((PARAM_CHANNELS, channels), (PARAM_RATE, rate)):
            params.intervals[slot - 3].min = value
            params.intervals[slot - 3].max = value
    params.rmask = 0xFFFFFFFF
    params.cmask = 0
    fcntl.ioctl(fd, IOCTL_HW_REFINE, params)
    # Resolve the still-open sizing parameters, as alsa-lib does before
    # SNDRV_PCM_IOCTL_HW_PARAMS: prefer one 256-frame period (the host's
    # audio quantum) inside the refined range, and a whole number of
    # periods in the buffer range.
    def resolve(slot, prefer):
        span = params.intervals[slot - 3]
        return min(max(span.min, prefer), span.max)

    period = max(resolve(8, 256) & ~63, 64)  # 64-frame steps, like HDA
    buffer = resolve(12, period * 8)
    buffer -= buffer % period
    for slot, value in ((8, period), (12, max(buffer, period))):
        params.intervals[slot - 3].min = value
        params.intervals[slot - 3].max = value
    fcntl.ioctl(fd, IOCTL_HW_PARAMS, params)
    print(f"alsa: {rate} Hz {channels} ch, period {period} frames, "
          f"buffer {max(buffer, period)} frames", file=sys.stderr)
    return max(period, 1)


def play(device, rate, channels):
    """Play raw PCM from stdin. With ALSA_MARKER set to a virtio-serial
    port device, one marker byte is written there immediately before the
    first PCM write; the host sees it within the port's own latency
    (sub-millisecond), which is the instant the A4 harness measures
    from. With ALSA_STAMP set, the guest clock is written to that path
    at the same moment, for diagnostics."""
    fd = os.open(device, os.O_RDWR)
    try:
        period = negotiate(fd, rate, channels)
        fcntl.ioctl(fd, IOCTL_PREPARE)
        frame_bytes = 2 * channels
        chunk = period * frame_bytes
        stamped = False
        while True:
            data = sys.stdin.buffer.read(chunk)
            if not data:
                break
            while data:
                if not stamped:
                    stamped = True
                    marker = os.environ.get("ALSA_MARKER")
                    if marker:
                        with open(marker, "wb", buffering=0) as port:
                            port.write(b"M\n")
                    stamp = os.environ.get("ALSA_STAMP")
                    if stamp:
                        open(stamp, "w").write(f"{time.time():.9f}\n")
                try:
                    written = os.write(fd, data)
                except OSError as error:
                    if error.errno == errno.EPIPE:
                        # Underrun: reprepare and keep going.
                        fcntl.ioctl(fd, IOCTL_PREPARE)
                        continue
                    raise
                data = data[written:]
        fcntl.ioctl(fd, IOCTL_DRAIN)
    finally:
        try:
            fcntl.ioctl(fd, IOCTL_HW_FREE)
        except OSError:
            pass
        os.close(fd)


def record(device, rate, channels, seconds):
    fd = os.open(device, os.O_RDWR)
    try:
        period = negotiate(fd, rate, channels)
        fcntl.ioctl(fd, IOCTL_PREPARE)
        fcntl.ioctl(fd, IOCTL_START)
        frame_bytes = 2 * channels
        chunk = period * frame_bytes
        wanted = seconds * rate * frame_bytes
        done = 0
        deadline = time.monotonic() + seconds + 5
        while done < wanted and time.monotonic() < deadline:
            try:
                data = os.read(fd, min(chunk, wanted - done))
            except OSError as error:
                if error.errno == errno.EPIPE:
                    fcntl.ioctl(fd, IOCTL_PREPARE)
                    fcntl.ioctl(fd, IOCTL_START)
                    continue
                raise
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            done += len(data)
        fcntl.ioctl(fd, IOCTL_DROP)
    finally:
        try:
            fcntl.ioctl(fd, IOCTL_HW_FREE)
        except OSError:
            pass
        os.close(fd)

class ElemId(ctypes.Structure):
    _fields_ = [
        ("numid", ctypes.c_uint32),
        ("iface", ctypes.c_uint32),
        ("device", ctypes.c_uint32),
        ("subdevice", ctypes.c_uint32),
        ("name", ctypes.c_char * 44),
        ("index", ctypes.c_uint32),
    ]


class ElemList(ctypes.Structure):
    _fields_ = [
        ("offset", ctypes.c_uint32),
        ("space", ctypes.c_uint32),
        ("used", ctypes.c_uint32),
        ("items", ctypes.c_uint32),
        ("pids", ctypes.POINTER(ElemId)),
        ("reserved", ctypes.c_ubyte * 50),
    ]


class ElemInfo(ctypes.Structure):
    _fields_ = [
        ("id", ElemId),
        ("type", ctypes.c_uint32),
        ("access", ctypes.c_uint32),
        ("count", ctypes.c_uint32),
        ("owner", ctypes.c_int32),
        ("value", ctypes.c_ubyte * 128),
        ("reserved", ctypes.c_ubyte * 64),
    ]


class ElemValue(ctypes.Structure):
    _fields_ = [
        ("id", ElemId),
        ("indirect", ctypes.c_uint32),
        ("value", ctypes.c_long * 128),
        ("reserved", ctypes.c_ubyte * 128),
    ]


def unmute():
    """Turn every playback and capture control on, volume at maximum.

    The kernel boots the generic codec with its amplifiers muted, exactly
    as a desktop session's mixer finds it; PipeWire unmutes at login. This
    walks the same SNDRV_CTL_IOCTL_* interface a mixer uses. Maximum is
    0 dB on this codec, so PCM crosses the device untouched.
    """
    ctl = os.open("/dev/snd/controlC0", os.O_RDWR)
    try:
        listing = ElemList(0, 0, 0, 0, None, (ctypes.c_ubyte * 50)())
        fcntl.ioctl(ctl, _iowr("U", 0x10, ctypes.sizeof(ElemList)), listing)
        ids = (ElemId * listing.items)()
        request = ElemList(0, listing.items, 0, listing.items, ids,
                           (ctypes.c_ubyte * 50)())
        fcntl.ioctl(ctl, _iowr("U", 0x10, ctypes.sizeof(ElemList)), request)
        for ident in ids[: request.used]:
            if b"Playback" not in ident.name and b"Capture" not in ident.name:
                continue
            info = ElemInfo(ident)
            try:
                fcntl.ioctl(ctl, _iowr("U", 0x11, ctypes.sizeof(ElemInfo)), info)
            except OSError:
                continue
            if not info.access & 2:  # SNDRV_CTL_ELEM_ACCESS_WRITE
                continue
            if info.type == 1:  # boolean: the switch
                target = 1
            elif info.type == 2:  # integer: the volume
                lo, hi = struct.unpack_from("lll", info.value, 0)[:2]
                target = hi
            else:
                continue
            value = ElemValue(ident)
            for channel in range(info.count):
                value.value[channel] = target
            fcntl.ioctl(ctl, _iowr("U", 0x13, ctypes.sizeof(ElemValue)), value)
            print(f"mixer: {ident.name.decode(errors='replace')} = {target}",
                  file=sys.stderr)
    finally:
        os.close(ctl)


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "unmute":
        unmute()
        return
    if len(sys.argv) < 4 or sys.argv[1] not in ("play", "record"):
        raise SystemExit(__doc__)
    rate, channels = int(sys.argv[2]), int(sys.argv[3])
    if sys.argv[1] == "play":
        play("/dev/snd/pcmC0D0p", rate, channels)
    else:
        record("/dev/snd/pcmC0D0c", rate, channels, int(sys.argv[4]))


if __name__ == "__main__":
    main()

"""Bounded AML/ASL parsing for the approved D6 PCI-function omissions."""

import re
import struct


def package(data: bytes, offset: int) -> tuple[int, int]:
    """Decode ACPI PkgLength, returning the body start and package end."""
    following = data[offset] >> 6
    width = following + 1
    if offset + width > len(data):
        raise ValueError("truncated AML package length")
    length = data[offset] & (15 if following else 63)
    for index in range(following):
        length |= data[offset + index + 1] << (4 + index * 8)
    if length < width or offset + length > len(data):
        raise ValueError("invalid AML package extent")
    return offset + width, offset + length


def length_bytes(length: int, width: int) -> bytes:
    if width == 1:
        if length >= 64:
            raise ValueError("AML package length overflow")
        return bytes([length])
    if length >= 1 << (4 + 8 * (width - 1)):
        raise ValueError("AML package length overflow")
    return bytes([((width - 1) << 6) | (length & 15)]) + bytes(
        (length >> (4 + 8 * index)) & 255 for index in range(width - 1))


def omit_pci_functions(data: bytes, slots: set[str]) -> tuple[bytes, list[tuple[int, int]]]:
    """Remove only exact Name(_ADR) devices in a device-only _SB.PCI0 scope.

    Other AML is opaque and remains byte-exact. Unexpected terms or additional
    methods in an omitted function prevent normalization of that scope.
    Returned ranges refer to the original DSDT, including its ACPI header.
    """
    if data[:4] != b"DSDT" or len(data) < 36 or struct.unpack_from("<I", data, 4)[0] != len(data):
        return data, []
    wanted = {f"S{(int(slot[3:5], 16) << 3) | int(slot[6]):02X}_".encode():
              (int(slot[3:5], 16) << 16) | int(slot[6]) for slot in slots}
    for outer in range(36, len(data) - 16):
        if data[outer] != 0x10:
            continue
        try:
            body, end = package(data, outer + 1)
            if data[body:body + 5] != b"\\_SB_":
                continue
            inner = body + 5
            if data[inner] != 0x10:
                continue
            children, inner_end = package(data, inner + 1)
            if data[children:children + 4] != b"PCI0" or inner_end != end:
                continue
            cursor = children + 4
            ranges = []
            while cursor < inner_end:
                if data[cursor:cursor + 2] != b"\x5b\x82":
                    raise ValueError("PCI scope contains a non-device term")
                device_body, device_end = package(data, cursor + 2)
                name = bytes(data[device_body:device_body + 4])
                if name in wanted:
                    expected = b"\x08_ADR\x0c" + struct.pack("<I", wanted[name])
                    if data[device_body + 4:device_end] != expected:
                        raise ValueError("omitted PCI function has additional AML")
                    ranges.append((cursor, device_end))
                cursor = device_end
            if not ranges or len(ranges) != len(wanted):
                continue
        except (IndexError, ValueError):
            continue
        removed = sum(stop - start for start, stop in ranges)
        result = bytearray(data)
        for start, stop in reversed(ranges):
            del result[start:stop]
        for start, content, stop in ((outer, body, end), (inner, children, inner_end)):
            width = content - start - 1
            result[start + 1:content] = length_bytes(stop - start - 1 - removed, width)
        struct.pack_into("<I", result, 4, len(result))
        result[9] = 0
        result[9] = -sum(result) & 255
        return bytes(result), ranges
    return data, []


TOKEN = re.compile(r'/\*.*?\*/|//[^\n]*|"(?:\\.|[^"\\])*"|0[xX][0-9a-fA-F]+|'
                   r'[A-Za-z_\\][A-Za-z_0-9.\\]*|[0-9]+|[^\s]', re.S)


def omit_asl_functions(text: str, slots: set[str]) -> str:
    """Parse balanced Device bodies; remove only a single exact _ADR declaration."""
    tokens = [m for m in TOKEN.finditer(text) if not m[0].startswith(("/*", "//"))]
    wanted = {f"S{(int(slot[3:5], 16) << 3) | int(slot[6]):02X}":
              (int(slot[3:5], 16) << 16) | int(slot[6]) for slot in slots}
    ranges = []
    for index in range(len(tokens) - 5):
        values = [m[0] for m in tokens[index:index + 5]]
        if values[0] != "Device" or values[1] != "(" or values[3:] != [")", "{"]:
            continue
        name = values[2].rstrip("_")
        if name not in wanted:
            continue
        depth = 1
        end = index + 5
        while end < len(tokens) and depth:
            depth += (tokens[end][0] == "{") - (tokens[end][0] == "}")
            end += 1
        body = [m[0] for m in tokens[index + 5:end - 1]]
        if (depth == 0 and len(body) == 6 and body[:4] == ["Name", "(", "_ADR", ","]
                and body[-1] == ")" and re.fullmatch(r"0[xX][0-9a-fA-F]+|[0-9]+", body[4])
                and int(body[4], 0) == wanted[name]):
            ranges.append((tokens[index].start(), tokens[end - 1].end()))
    for start, stop in reversed(ranges):
        text = text[:start] + text[stop:]
    return text


def relocate_loader(data: bytes, cuts: list[tuple[int, int]]) -> bytes:
    """Parse table-loader records and account only for the removed DSDT bytes."""
    result = bytearray(data)
    # FACS occupies the first 64 bytes of etc/acpi/tables.
    cuts = [(start + 64, stop + 64) for start, stop in cuts]

    def moved(offset):
        if any(start <= offset < stop for start, stop in cuts):
            raise ValueError("table-loader points into an omitted AML device")
        return offset - sum(stop - start for start, stop in cuts if stop <= offset)

    for index, (record,) in enumerate(struct.iter_unpack("128s", data)):
        command = struct.unpack_from("<I", record)[0]
        dest = record[4:60].split(b"\0", 1)[0]
        fields = []
        if command == 2 and dest == b"etc/acpi/tables":
            fields = [116]
        elif command == 3 and dest == b"etc/acpi/tables":
            offset, start, length = struct.unpack_from("<III", record, 60)
            struct.pack_into("<III", result, index * 128 + 60,
                             moved(offset), moved(start), moved(start + length) - moved(start))
        elif command == 4:
            source = record[60:116].split(b"\0", 1)[0]
            fields = ([116] if dest == b"etc/acpi/tables" else [])
            fields += ([120] if source == b"etc/acpi/tables" else [])
        for field in fields:
            offset = struct.unpack_from("<I", record, field)[0]
            struct.pack_into("<I", result, index * 128 + field, moved(offset))
    return bytes(result)

"""Parse DTB v17 and normalize ordering, phandle allocation, and boot entropy."""

import json
import struct
import re


def parse(data):
    if len(data) < 40:
        raise ValueError("truncated FDT header")
    magic, total, structure, strings, reserve, version, compatible, boot, names_size, size = struct.unpack_from(">10I", data)
    if magic != 0xD00DFEED or version < 17 or compatible > 17:
        raise ValueError("unsupported FDT header")
    if not 40 <= reserve <= structure <= strings <= total <= len(data) or reserve % 8 or structure % 4:
        raise ValueError("invalid FDT block offsets")
    if structure + size > strings or strings + names_size > total:
        raise ValueError("FDT block exceeds its bounds")
    reserved = []
    while reserve + 16 <= structure:
        address, length = struct.unpack_from(">QQ", data, reserve)
        reserve += 16
        if address == length == 0:
            break
        reserved.append((address, length))
    else:
        raise ValueError("unterminated FDT reservation map")
    nodes = {}
    stack = []
    offset = structure
    end = structure + size
    while offset + 4 <= end:
        token, = struct.unpack_from(">I", data, offset)
        offset += 4
        if token == 1:
            terminator = data.find(b"\0", offset, end)
            if terminator < 0:
                raise ValueError("unterminated FDT node")
            name = data[offset:terminator].decode("ascii")
            if "/" in name or (not stack and (name or nodes)):
                raise ValueError("invalid FDT node name or root")
            stack.append(name)
            path = "/" + "/".join(stack[1:])
            if path in nodes:
                raise ValueError("duplicate FDT node")
            nodes[path] = {}
            offset = (terminator + 4) & ~3
        elif token == 2:
            if not stack:
                raise ValueError("unbalanced FDT node")
            stack.pop()
        elif token == 3:
            if not stack or offset + 8 > end:
                raise ValueError("invalid FDT property")
            length, name_offset = struct.unpack_from(">II", data, offset)
            offset += 8
            if name_offset >= names_size or offset + length > end:
                raise ValueError("FDT property exceeds its bounds")
            terminator = data.find(b"\0", strings + name_offset, strings + names_size)
            if terminator < 0:
                raise ValueError("unterminated FDT property name")
            name = data[strings + name_offset:terminator].decode("ascii")
            properties = nodes["/" + "/".join(stack[1:])]
            if name in properties:
                raise ValueError("duplicate FDT property")
            properties[name] = data[offset:offset + length]
            offset = (offset + length + 3) & ~3
        elif token == 4:
            continue
        elif token == 9:
            if stack or "/" not in nodes or offset != end:
                raise ValueError("invalid FDT end token")
            return nodes, reserved, boot
        else:
            raise ValueError(f"unknown FDT token {token}")
    raise ValueError("missing FDT end token")


def cells(data):
    if len(data) % 4:
        raise ValueError("FDT cell property is not word-aligned")
    return list(struct.unpack(f">{len(data) // 4}I", data))


def normalize(data):
    nodes, reserved, boot = parse(data)
    handles = {}
    for path, properties in nodes.items():
        for name in ("phandle", "linux,phandle"):
            if name in properties:
                values = cells(properties[name])
                if len(values) != 1 or values[0] in (0, 0xFFFFFFFF):
                    raise ValueError("invalid FDT phandle")
                handle = values[0]
                if handle in handles and handles[handle] != path:
                    raise ValueError("duplicate FDT phandle")
                handles[handle] = path

    def reference(handle):
        if handle not in handles:
            raise ValueError(f"unknown FDT phandle {handle}")
        return handles[handle]

    def count(path, name, default):
        values = cells(nodes[path].get(name, struct.pack(">I", default)))
        if len(values) != 1 or values[0] > 16:
            raise ValueError("invalid FDT cell count")
        return values[0]

    normalized = {}
    for path, properties in nodes.items():
        result = {}
        for name, value in properties.items():
            if name in ("phandle", "linux,phandle") or (path == "/chosen" and name in ("rng-seed", "kaslr-seed")):
                continue
            if name in ("interrupt-parent", "regmap", "cpu", "msi-parent"):
                words = cells(value)
                if not words:
                    raise ValueError("empty FDT phandle reference")
                result[name] = [reference(words[0]), *words[1:]]
            elif name == "interrupts-extended":
                words = cells(value)
                entries = []
                while words:
                    parent = reference(words[0])
                    width = count(parent, "#interrupt-cells", 1)
                    if len(words) < 1 + width:
                        raise ValueError("truncated FDT extended interrupt")
                    entries.append([parent, *words[1:1 + width]])
                    words = words[1 + width:]
                result[name] = entries
            elif name == "interrupt-map":
                words = cells(value)
                child_width = count(path, "#address-cells", 2) + count(path, "#interrupt-cells", 1)
                entries = []
                while words:
                    if len(words) <= child_width:
                        raise ValueError("truncated FDT interrupt map")
                    parent = reference(words[child_width])
                    width = child_width + 1 + count(parent, "#address-cells", 0) + count(parent, "#interrupt-cells", 1)
                    if len(words) < width:
                        raise ValueError("truncated FDT interrupt-map parent")
                    entries.append([*words[:child_width], parent, *words[child_width + 1:width]])
                    words = words[width:]
                result[name] = entries
            else:
                result[name] = value.hex()
        normalized[path] = result
    return json.dumps({"boot_cpu": boot, "reservations": sorted(reserved), "nodes": normalized}, sort_keys=True, indent=2) + "\n"


def compare(reference, candidate, riscv_subset=False):
    """Return every semantic difference, with narrowly scoped D7 reasons."""
    left, right = (json.loads(normalize(blob)) for blob in (reference, candidate))
    differences = []

    def changed(path, name, a, b):
        if a == b:
            return
        reason = None
        if riscv_subset and re.fullmatch(r"/cpus/cpu@[0-9a-f]+", path) and a is not None:
            expected = {
                "riscv,isa": b"rv64imafdc_zicsr_zifencei\0".hex(),
                "riscv,isa-extensions": b"i\0m\0a\0f\0d\0c\0zicsr\0zifencei\0".hex(),
            }
            if name in expected and b == expected[name]:
                reason = "D7: advertise the validated RV64GC subset, rather than QEMU max"
            elif name in ("riscv,cbom-block-size", "riscv,cbop-block-size", "riscv,cboz-block-size") and a == "00000040" and b is None:
                reason = "D7: omit block-size properties for unadvertised Zicbo extensions"
        differences.append({"node": path, "property": name, "reference": a,
                            "candidate": b, "reason": reason})

    for name in ("boot_cpu", "reservations"):
        changed("/", name, left[name], right[name])
    a, b = left["nodes"], right["nodes"]
    for path in sorted(a.keys() | b.keys()):
        if path not in a or path not in b:
            changed(path, "node", a.get(path), b.get(path))
            continue
        for name in sorted(a[path].keys() | b[path].keys()):
            changed(path, name, a[path].get(name), b[path].get(name))
    return differences


def main():
    import argparse
    from pathlib import Path
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--riscv-subset", action="store_true", help="explain the phase-3 D7 ISA differences")
    args = parser.parse_args()
    differences = compare(args.reference.read_bytes(), args.candidate.read_bytes(), args.riscv_subset)
    print(json.dumps(differences, sort_keys=True, indent=2))
    return int(any(item["reason"] is None for item in differences))


if __name__ == "__main__":
    raise SystemExit(main())

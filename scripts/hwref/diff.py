#!/usr/bin/env python3
"""Compare normalized guest hardware observations. No third-party dependencies."""

from __future__ import annotations

import argparse
import difflib
import json
import re
import sys
from pathlib import Path

DEFAULT_ALLOW = Path(__file__).with_name("approved-differences.json")
BDF = r"(?:[0-9a-f]{4}:)?[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]"


def hex_dump(data: bytes) -> str:
    return "".join(f"{i:04x}: {data[i:i+16].hex(' ')}\n" for i in range(0, len(data), 16))


def acpi(data: bytes) -> bytes:
    result = bytearray(data)
    signature = bytes(data[:4])
    fields = {
        b"FACP": [(36, 8), (132, 16)],
        b"FACS": [(12, 4), (24, 8)],
        b"RSDT": [(36, len(data) - 36)],
        b"XSDT": [(36, len(data) - 36)],
        b"BGRT": [(40, 8)],
    }.get(signature, [])
    if data.startswith(b"RSD PTR "):
        fields = [(8, 1), (16, 4), (24, 8), (32, 1)]
    elif signature != b"FACS" and len(data) >= 36:
        fields += [(9, 1)]  # checksum covers relocated pointers
    for offset, size in fields:
        if offset + size <= len(result):
            result[offset:offset+size] = bytes(size)
    return bytes(result)


def smbios(data: bytes) -> bytes:
    """Keep structures and strings; mask only the system UUID."""
    result = bytearray(data)
    offset = 0
    while offset < len(data):
        if offset + 4 > len(data):
            raise ValueError("truncated SMBIOS header")
        kind, size = data[offset:offset+2]
        if size < 4 or offset + size > len(data):
            raise ValueError("invalid SMBIOS structure length")
        end = data.find(b"\0\0", offset + size)
        if end < 0:
            raise ValueError("unterminated SMBIOS strings")
        if kind == 1 and size >= 24:
            result[offset+8:offset+24] = bytes(16)
        if kind == 4 and size >= 16:
            result[offset+8:offset+16] = bytes(8)  # native CPUID signature/features
        offset = end + 2
        if kind == 127:
            break
    return bytes(result)


def smbios_anchor(data: bytes) -> bytes:
    result = bytearray(data)
    if data.startswith(b"_SM3_") and len(data) >= 24:
        result[5] = 0
        result[16:24] = bytes(8)
    elif data.startswith(b"_SM_") and len(data) >= 31:
        result[4] = result[21] = 0
        result[24:28] = bytes(4)
    elif data.startswith(b"_DMI_") and len(data) >= 15:
        result[5] = 0
        result[8:12] = bytes(4)
    return bytes(result)


def pci_config(data: bytes) -> bytes:
    """Mask assigned BAR bases and MSI destinations, keeping BAR type bits."""
    result = bytearray(data)
    if len(data) < 64:
        return data
    header = data[14] & 0x7f
    limit = 0x18 if header == 1 else 0x28
    offset = 0x10
    while offset < limit:
        value = int.from_bytes(data[offset:offset+4], "little")
        flags = value & (3 if value & 1 else 15)
        result[offset:offset+4] = flags.to_bytes(4, "little")
        if not value & 1 and value & 6 == 4:
            result[offset+4:offset+8] = bytes(4)
            offset += 4
        offset += 4
    result[0x30:0x34] = (int.from_bytes(data[0x30:0x34], "little") & 0x7ff).to_bytes(4, "little")
    pointer = data[0x34] & 0xfc
    seen = set()
    while pointer >= 0x40 and pointer + 4 <= len(data) and pointer not in seen:
        seen.add(pointer)
        if data[pointer] == 5:  # PCI MSI, 32/64-bit message destination and data
            size = 10 if data[pointer+2] & 0x80 else 6
            end = min(pointer + 4 + size, len(data))
            result[pointer+4:end] = bytes(end - pointer - 4)
        pointer = data[pointer+1] & 0xfc
    return bytes(result)


def normalize_pci(text: str) -> str:
    blocks = re.split(rf"(?=^{BDF} )", text, flags=re.M)
    output = []
    for block in blocks:
        rows = list(re.finditer(r"^([0-9a-f]{2,3}): ((?:[0-9a-f]{2} ?){16})$", block, re.M))
        if rows:
            config = bytearray(max(int(m[1], 16) + 16 for m in rows))
            for match in rows:
                offset = int(match[1], 16)
                config[offset:offset+16] = bytes.fromhex(match[2])
            masked = pci_config(bytes(config))
            for match in reversed(rows):
                offset = int(match[1], 16)
                block = block[:match.start()] + f"{match[1]}: {masked[offset:offset+16].hex(' ')}" + block[match.end():]
        block = re.sub(r"((?:Memory|I/O ports|Expansion ROM) at )\S+", r"\1<allocation>", block)
        block = re.sub(r"(Address: )\S+( Data: )\S+", r"\1<msi>\2<msi>", block)
        output.append(block)
    return "".join(output)


def normalize_text(name: str, text: str) -> str:
    text = text.replace("\r\n", "\n")
    text = re.sub(r"\b(?:[0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}\b", "<mac>", text)
    text = re.sub(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b", "<uuid>", text)
    if name == "lspci-nnnvvvxxxx.txt":
        text = normalize_pci(text)
    if name == "cpuid.txt":
        # The host policy passes through native leaves. Hypervisor leaves are
        # our contract, including Hyper-V, and must remain visible to the gate.
        text = "\n".join(l for l in text.splitlines() if l.startswith("CPU ") or l.startswith("0x4")) + "\n"
    if name == "cpuinfo.txt":
        text = "\n".join(l for l in text.splitlines() if l.startswith(("processor", "siblings", "cpu cores"))) + "\n"
    if name == "interrupts.txt":
        text = re.sub(r"(?m)^(\s*\S+:)(?:\s+\d+)+", r"\1 <counts>", text)
    if name == "serial16550.txt":
        text = re.sub(r"\b(tx|rx):\d+", r"\1:<count>", text)
    if name == "proc-pci-devices.txt":
        rows = []
        for line in text.splitlines():
            fields = line.split()
            if len(fields) >= 17:
                fields[3:10] = [f"{int(v, 16) & 15:016x}" for v in fields[3:10]]
            rows.append("\t".join(fields))
        text = "\n".join(rows) + "\n"
    if name.endswith(".dsl") or name == "acpi-dsl/iasl.log":
        text = re.sub(r"(?m)^.*(?:Disassembly of|Disassembling to|Parsing completed|disassembly completed|Compilation of|Input file|Output file|ASL Output|Binary Output|AML Output|Table checksum|Checksum :|File appears to be binary: found).*$", "", text)
        text = re.sub(r"(FACS Address|DSDT Address|Image Address)(\s*:\s*)[0-9A-F]+", r"\1\2<allocation>", text)
        # iasl includes a redundant raw table dump after the decoded fields.
        text = text.split("Raw Table Data:")[0]
    if name in ("dmesg.txt", "e820-dmesg.txt"):
        text = re.sub(r"(?m)^\[\s*\d+\.\d+\]\s*", "", text)
        text = re.sub(r"(?m)^clocksource: Long readout interval, skipping watchdog check:.*\n?", "", text)
        text = re.sub(r"\baudit\(\d+\.\d+:\d+\)", "audit(<time>)", text)
        text = re.sub(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d", "<time>", text)
        text = re.sub(r"0x[0-9a-f]{6,16}\b", "<address>", text)
        text = re.sub(r"\b[0-9a-f]{8,16}\b", "<address>", text)
        text = re.sub(r"\b\d+\.\d+\b", "<measurement>", text)
        text = re.sub(r"(Memory: )\d+K/", r"\1<available>K/", text)
        text = re.sub(r"\b\d+K reserved\b", "<reserved>K reserved", text)
        text = re.sub(r"(efi: Freeing EFI boot services memory: )\d+K", r"\1<allocation>K", text)
        text = "\n".join(sorted(text.splitlines())) + "\n"
    if name == "iomem.txt":
        text = re.sub(r"(?m)^(\s*)[0-9a-f]+-[0-9a-f]+( : Kernel (?:code|rodata|data|bss))$",
                      r"\1<kernel-allocation>\2", text)
        kernel = sorted(l.strip() for l in text.splitlines() if "<kernel-allocation>" in l)
        text = "\n".join(l for l in text.splitlines() if "<kernel-allocation>" not in l)
        text += "\n" + "\n".join(kernel)
    if name == "fwcfg/listing.txt":
        text = re.sub(r"\w{3}\s+\d+\s+\d\d:\d\d", "<time>", text)
    if name == "usb.txt":
        text = re.sub(r"Bus \d+ Device \d+:", "USB device:", text)
        text = re.sub(r"/sys/bus/usb/devices/(?:usb\d+|\d+-[\d.]+)", "<usb-path>", text)
        text = "\n".join(sorted(text.splitlines())) + "\n"
    if name == "dmidecode.txt":
        text = re.sub(r"(Table at |_SM[23]?_ at |Entry point at )0x[0-9A-Fa-f]+", r"\1<allocation>", text)
        text = re.sub(r"(?m)^(\s*ID:) (?:[0-9A-F]{2} ){7}[0-9A-F]{2}$", r"\1 <host-cpuid>", text)
    if name == "uname.txt":
        text = re.sub(r"Linux \S+ ", "Linux <hostname> ", text, count=1)
    return "\n".join(l.rstrip() for l in text.splitlines() if l.strip()) + ("\n" if text.strip() else "")


def normalize_artifact(name: str, data: bytes) -> str:
    if name.startswith("acpi/") and not name.endswith((".dsl", ".log", ".pre")):
        return hex_dump(acpi(data))
    if name == "fwcfg/blobs/etc/acpi/rsdp":
        return hex_dump(acpi(data))
    if name in ("dmi-tables/DMI", "fwcfg/blobs/etc/smbios/smbios-tables"):
        return hex_dump(smbios(data))
    if name in ("dmi-tables/smbios_entry_point", "fwcfg/blobs/etc/smbios/smbios-anchor"):
        return hex_dump(smbios_anchor(data))
    if name == "smbios.bin":
        # dmidecode's dump format puts structures at offset 32.
        return hex_dump(smbios_anchor(data[:32]) + smbios(data[32:]))
    try:
        text = data.decode("utf-8")
        if "\0" in text:
            raise UnicodeError()
    except UnicodeError:
        return hex_dump(data)
    return normalize_text(name, text)


def allocation_map(guest: Path) -> dict[int, str]:
    """Name movable firmware and Linux setup-data boundaries by role and size.

    Keep all other e820 endpoints exact, including the guest RAM ceiling and
    architectural holes. Adjacent RAM endpoints inherit the same boundary.
    """
    allocations = []
    path = guest / "e820-memmap.txt"
    if path.exists():
        for line in path.read_text().splitlines():
            fields = line.split(maxsplit=3)
            if len(fields) == 4 and fields[3] == "ACPI Tables":
                allocations.append(("acpi", int(fields[1], 16), int(fields[2], 16)))
    path = guest / "dmesg.txt"
    if path.exists():
        for match in re.finditer(r"e820: update \[mem (0x[0-9a-f]+)-(0x[0-9a-f]+)\] usable ==> (usable|reserved)", path.read_text()):
            start, end = int(match[1], 16), int(match[2], 16)
            if start >= 0x100000:
                allocations.append((f"setup-{len(allocations)}-{match[3]}", start, end))
    result = {}
    for role, start, end in allocations:
        size = end - start + 1
        for value, suffix in ((start, "start"), (end, "end"), (start-1, "before"), (end+1, "after")):
            result.setdefault(value, f"<{role}:{size:#x}:{suffix}>")
    return result


def mask_boundaries(name: str, data: bytes, allocations: dict[int, str]) -> bytes:
    if name == "e820-memmap.txt":
        text = data.decode()
        text = re.sub(r"0x[0-9a-f]+", lambda m: allocations.get(int(m[0], 16), m[0]), text)
        return text.encode()
    if name == "iomem.txt":
        text = data.decode()
        def region(match):
            start = allocations.get(int(match[2], 16), match[2])
            end = allocations.get(int(match[3], 16), match[3])
            return f"{match[1]}{start}-{end} :"
        return re.sub(r"(?m)^(\s*)([0-9a-f]+)-([0-9a-f]+) :", region, text).encode()
    return data


def load_capture(root: Path) -> dict[str, str]:
    guest = root / "guest"
    if not guest.is_dir():
        raise ValueError(f"{root}: missing guest/ capture directory")
    readme = root / "README.md"
    if readme.exists() and "CAPTURE FAILED" in readme.read_text():
        raise ValueError(f"{root}: capture is marked failed")
    required = ("lspci-nn.txt", "lspci-nnnvvvxxxx.txt", "cpuid.txt", "fwcfg/sizes.txt", "smbios.bin",
                "acpi/DSDT", "acpi/FACP", "e820-memmap.txt", "interrupts.txt")
    for name in required:
        if not (guest / name).is_file() or not (guest / name).stat().st_size:
            raise ValueError(f"{root}: missing or empty required artefact {name}")
    if not re.search(rf"^{BDF} .*\[[0-9a-f]{{4}}:[0-9a-f]{{4}}\]",
                     (guest / "lspci-nn.txt").read_text(), re.M):
        raise ValueError(f"{root}: no PCI functions in capture")
    if not re.search(r"^0x[0-9a-f]{8} 0x[0-9a-f]+:", (guest / "cpuid.txt").read_text(), re.M):
        raise ValueError(f"{root}: no CPUID leaves in capture")
    result = {}
    allocations = allocation_map(guest)
    for path in sorted(guest.rglob("*")):
        if path.is_file():
            name = path.relative_to(guest).as_posix()
            result[name] = normalize_artifact(name, mask_boundaries(name, path.read_bytes(), allocations))
    return result


def approved_pci(left: dict[str, str], right: dict[str, str], rules: dict) -> None:
    """D6 is directional: reference USB functions may disappear, xHCI may appear.

    Remove only whole functions whose identity matches. Never permit edits to
    a function present on both sides, or an arbitrary device at a USB slot.
    """
    def inventory(text):
        return {m[1].removeprefix("0000:"): m[2] for m in re.finditer(
            rf"^({BDF}) .*?\[([0-9a-f]{{4}}:[0-9a-f]{{4}})\]", text, re.M)}

    a = inventory(left.get("lspci-nn.txt", ""))
    b = inventory(right.get("lspci-nn.txt", ""))
    removed = {slot for slot, identity in rules["reference_only"].items()
               if a.get(slot) == identity and slot not in b}
    added = {slot for slot, identity in b.items() if slot not in a and identity in rules["candidate_only_ids"]}
    # D6 permits one replacement controller, not an arbitrary number of them.
    if len(added) != 1 or any(identity in rules["candidate_only_ids"] for identity in a.values()):
        added = set()
    for captures, slots in ((left, removed), (right, added)):
        for name in ("lspci-nn.txt", "lspci-nnnvvvxxxx.txt"):
            if name in captures:
                blocks = re.split(rf"(?=^{BDF} )", captures[name], flags=re.M)
                captures[name] = "".join(block for block in blocks
                                         if block.split(" ", 1)[0].removeprefix("0000:") not in slots)
        if "proc-pci-devices.txt" in captures:
            keys = {slot[:2] + f"{(int(slot[3:5], 16) << 3) | int(slot[6]):02x}" for slot in slots}
            captures["proc-pci-devices.txt"] = "".join(
                line for line in captures["proc-pci-devices.txt"].splitlines(True)
                if line.split()[0] not in keys)
        if "lspci-tv.txt" in captures:
            # The q35 root bus is fixed at 0000:00. Keep bridges and nesting;
            # remove only the identity-checked functions on that bus.
            captures["lspci-tv.txt"] = "".join(
                line for line in captures["lspci-tv.txt"].splitlines(True)
                if not any(re.search(rf"[+\\]-{re.escape(slot[3:])}\s", line) for slot in slots))
        for name in ("iomem.txt", "ioports.txt"):
            if name not in captures:
                continue
            lines = []
            parent_indent = None
            for line in captures[name].splitlines(True):
                indent = len(line) - len(line.lstrip())
                if parent_indent is not None and indent > parent_indent:
                    continue
                parent_indent = None
                if any(f"0000:{slot}" in line for slot in slots):
                    parent_indent = indent
                    continue
                lines.append(line)
            captures[name] = "".join(lines)
        if "usb.txt" in captures:
            drivers = []
            if any(slot.startswith("00:1d.") for slot in slots):
                drivers.extend(("uhci_hcd", "ehci_hcd"))
            if slots & added:
                drivers.append("xhci-hcd")
            captures["usb.txt"] = "".join(
                line for line in captures["usb.txt"].splitlines(True)
                if not any(driver in line and "Host Controller" in line for driver in drivers))


def compare(reference: Path, candidate: Path, allow: Path | None = DEFAULT_ALLOW) -> str:
    left, right = load_capture(reference), load_capture(candidate)
    if allow:
        rules = json.loads(allow.read_text())
        if (not isinstance(rules, dict) or rules.get("decision") != "D6"
                or not isinstance(rules.get("reference_only"), dict)
                or not isinstance(rules.get("candidate_only_ids"), list)):
            raise ValueError("allow-list requires D6, reference_only object, and candidate_only_ids list")
        for slot, identity in rules["reference_only"].items():
            if not re.fullmatch(BDF, slot) or not isinstance(identity, str) or not re.fullmatch(r"[0-9a-f]{4}:[0-9a-f]{4}", identity):
                raise ValueError("invalid reference_only PCI address or identity")
        if not all(isinstance(identity, str) and re.fullmatch(r"[0-9a-f]{4}:[0-9a-f]{4}", identity)
                   for identity in rules["candidate_only_ids"]):
            raise ValueError("invalid candidate_only_ids PCI identity")
        approved_pci(left, right, rules)
    report = []
    for name in sorted(left.keys() | right.keys()):
        if name not in left or name not in right:
            report.append(f"=== {name}: {'candidate only' if name not in left else 'reference only'} ===\n")
        elif left[name] != right[name]:
            report.append(f"=== {name} ===\n")
            report.extend(difflib.unified_diff(left[name].splitlines(True), right[name].splitlines(True),
                                               fromfile=f"reference/{name}", tofile=f"candidate/{name}"))
    return "".join(report)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--allow-list", type=Path, default=DEFAULT_ALLOW)
    parser.add_argument("--no-allow-list", action="store_true")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    try:
        report = compare(args.reference, args.candidate, None if args.no_allow_list else args.allow_list)
        if args.report:
            args.report.write_text(report)
        else:
            print(report, end="")
        return int(bool(report))
    except (ValueError, OSError, KeyError) as error:
        print(f"hwref diff: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

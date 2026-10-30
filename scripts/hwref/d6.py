"""Derived observations of identity-checked D6 controller omissions."""

import re


def observations(capture: dict[str, str], slots: set[str]) -> None:
    if not slots:
        return
    text = capture.get("dmesg.txt", "")
    buses = {m[2] for m in re.finditer(
        r"^(?:uhci_hcd|ehci-pci|xhci_hcd) 0000:(\S+): new USB bus registered, assigned bus number (\d+)$",
        text, re.M) if m[1] in slots}
    used_gsis = set()
    for block in re.split(r"(?=^00:[0-9a-f]{2}\.[0-7] )", capture.get("lspci-nnnvvvxxxx.txt", ""), flags=re.M):
        slot = re.match(r"00:([0-9a-f]{2})\.[0-7] ", block)
        pin = re.search(r"Interrupt: pin ([ABCD])", block)
        if slot and pin:
            device, index = int(slot[1], 16), ord(pin[1]) - ord("A")
            used_gsis.add(16 + (4 + (device + index) % 4 if device < 25 else index))
    omitted_gsis = {16 + int(slot[-1]) for slot in slots if slot in ("00:1d.0", "00:1d.1", "00:1d.2")}
    if "00:1d.7" in slots:
        omitted_gsis.add(19)
    lines = []
    for line in text.splitlines():
        pci = re.match(r"pci 0000:(\S+): ", line)
        controller = re.match(r"(?:uhci_hcd|ehci-pci|xhci_hcd) 0000:(\S+): ", line)
        error = re.search(r"error|fail|timeout|invalid|can't|unable|not responding|died|warning", line, re.I)
        if pci and pci[1] in slots and not error:
            continue
        if controller and controller[1] in slots and not error:
            continue
        root = re.match(r"usb usb(\d+): ", line)
        hub = re.match(r"hub (\d+)-0:1\.0: (?:USB hub found|\d+ ports detected)$", line)
        if (root and root[1] in buses and re.search(
                r": (?:Manufacturer:|Product:|SerialNumber:|New USB device|We don't know the algorithms for LPM)", line)) or (hub and hub[1] in buses):
            continue
        gsi = re.fullmatch(r"ACPI: \\_SB_\.GSI([A-H]): Enabled at IRQ (\d+)", line)
        if gsi and int(gsi[2]) in omitted_gsis - used_gsis:
            continue
        for slot in slots:
            line = line.replace(f"usb-0000:{slot}-", "usb-<D6-controller>-")
            line = line.replace(f"/0000:{slot}/usb", "/<D6-controller>/usb")
            line = line.replace(f"28754-0000:{slot}-", "28754-<D6-controller>-")
        if re.match(r"usb \d+-[\d.]+: new high-speed USB device number \d+ using (?:ehci-pci|xhci_hcd)$", line):
            line = re.sub(r"using (?:ehci-pci|xhci_hcd)$", "using <D6-controller>", line)
        lines.append(line)
    if "dmesg.txt" in capture:
        capture["dmesg.txt"] = "\n".join(sorted(lines)) + ("\n" if lines else "")
    if "interrupts.txt" in capture:
        lines = []
        for line in capture["interrupts.txt"].splitlines(True):
            if any(f"PCI-MSIX-0000:{slot} " in line for slot in slots):
                continue
            irq = re.fullmatch(r"\s*(\d+): <counts>\s+IO-APIC\s+(\d+)-fasteoi\s+(?:uhci_hcd|ehci_hcd):usb\d+\n", line)
            if irq and int(irq[1]) == int(irq[2]) and int(irq[1]) in omitted_gsis:
                continue
            lines.append(line)
        capture["interrupts.txt"] = "".join(lines)


def workspace(left: dict[str, str], right: dict[str, str], removed: set[str]) -> None:
    """SeaBIOS uses four fewer pages without the four ICH9 USB functions.

    Confirmed by QEMU disk boots with and without only those functions.
    Require the exact 16-KiB adjacent RAM/reservation change. RAM ceilings,
    other reservations and unexpected derived-count changes stay failures.
    """
    if removed != {"00:1d.0", "00:1d.1", "00:1d.2", "00:1d.7"}:
        return
    pattern = r"(?m)^(\d+) 0x100000 (0x[0-9a-f]+) System RAM\n(\d+) (0x[0-9a-f]+) (0x[0-9a-f]+) Reserved$"
    a = re.search(pattern, left.get("e820-memmap.txt", ""))
    b = re.search(pattern, right.get("e820-memmap.txt", ""))
    if not a or not b:
        return
    old, new = int(a[4], 16), int(b[4], 16)
    if (new - old != 0x4000 or int(a[2], 16) + 1 != old
            or int(b[2], 16) + 1 != new or a[5] != b[5]):
        return
    for name in ("e820-memmap.txt", "iomem.txt"):
        if name in left:
            for before, after in ((old - 1, new - 1), (old, new)):
                left[name] = re.sub(rf"\b{before:x}\b", f"{after:x}", left[name])
                left[name] = left[name].replace(f"0x{before:x}", f"0x{after:x}")
    name = "dmesg.txt"
    if name not in left or name not in right:
        return
    left[name] = left[name].replace(f"last_pfn = {old >> 12:#x} ", f"last_pfn = {new >> 12:#x} ")
    for pattern, delta in ((r"Total pages: (\d+)", 4),
                           (r"Memory: <available>K/(\d+)K available", 16),
                           (r"On node 0, zone (?:DMA32|Normal): (\d+) pages in unavailable ranges", -4)):
        a, b = re.search(pattern, left[name]), re.search(pattern, right[name])
        if a and b and int(b[1]) - int(a[1]) == delta:
            left[name] = left[name][:a.start(1)] + b[1] + left[name][a.end(1):]

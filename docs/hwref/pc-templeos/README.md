# TempleOS 5.03.2

Captured on 2026-10-31 from `x86_64/templeos@5.03.2`, booted through
`vmlab up temple` under QEMU 11.1.2 and KVM. This is the TempleOS guest,
not the historical Linux superset probe. `qemu-argv.json` records the exact
running argv; `environment.json` records binary and metadata hashes.

## Machine and method

The production profile supplies pc/i440fx, SeaBIOS, IDE, standard VGA,
two vCPUs, 512 MiB, and the HolyC agent on COM1. COM2 carries the serial log.
There is no NIC, CD-ROM, or floppy attachment. The capture adds only QEMU
trace flags and a second QMP observation socket. No host devices are attached.

The guest disk is a disposable store-backed overlay. Its backing chain and
post-stop image check are recorded beside this file. The VM ran inside a
24-GiB user scope with `MemorySwapMax=0`; `cgroup.txt` records the scope.
Captures ran one at a time alongside other sessions' reserved VM slot.

## Exercised

- E1: Boot reached the tiled TempleOS desktop and the HolyC agent.
  `Dir;` returned the `C:/Home` directory. The guest reported
  `screen=640 x 480` from `GR_WIDTH` and `GR_HEIGHT`.
- E2: `FileWrite`, `FileRead`, and `Del` created, read back, and removed
  `~/HWREF.TXT`. Readback was `HWREF DISK CHECK`.
- E3: Keyboard `a` and Backspace events, absolute pointer movement and
  a click were sent through vmlab. HMP relative mouse movement was also sent.
  Guest reads immediately before and after reported `mouse=320,240` both
  times. Pointer consumption is therefore not proven by this capture.
- E4: The agent stop path produced QMP `SHUTDOWN` with `guest = true`
  and `reason = guest-shutdown`.

## Evidence and limits

Per-device Markdown files list register accesses, counts, value sets, and
first-use order. `event-values.json` records normalized command samples.
Static HMP files record qtree, PCI, memory regions, IRQs, ROMs, and USB.
`acpi/` contains checksummed guest-installed ACPI tables read through QMP.
The two SMBIOS binaries contain the guest's entry point and structures.
`fwcfg-files.json` is a paused HMP inventory with memory-region tracing
disabled during host port accesses. Host fw_cfg reads are excluded from
guest access counts. `cmos-observed.json` contains RTC-indexed value samples,
not an independent atomic dump of all CMOS bytes.

The trace contains 58,768,082 events. Its raw prefix is capped at 64 MiB;
58,069,495 later lines were omitted from the raw file. Counts cover the
full stream. Argument and value sets have explicit truncation flags.
KVM's in-kernel PIC/PIT paths are not a complete QEMU bus trace.
Physical-memory snapshots can add observer reads to VGA aperture rows;
those aperture counts are not guest-only.

No alternate display resolution was exercised. The observed desktop remains
640x480. The VBE trace contains firmware ID/enable probing, not a guest
resolution change. No CD, floppy, or NIC exercise was possible without
adding devices absent from this machine. Snapshots, reconnect, suspend, and
throughput were not tested. No S1 or S2 implementation changed, and this
capture does not establish a vmlab-hv parity pass.

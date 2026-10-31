# Windows 7 7600.0.1

Captured on 2026-10-31 from the owner's `x86_64/windows-7@7600.0.1`
template, with QEMU 11.1.2 and KVM. This is a Windows boot, not a Linux probe.
`qemu-argv.json` is the running process's argv, produced by `vmlab up win7`.
`environment.json` identifies the binaries and template metadata.

## Machine and observations

The `windows-legacy` profile supplies pc/i440fx, SeaBIOS, IDE, e1000,
standard VGA, Hyper-V CPU flags, and the virtio-serial agent channel.
The template supplies two vCPUs and 4 GiB. The capture lab attaches one NIC
to an isolated vmlab segment and a fixture CD built through vmlab's `media`
block. No floppy is attached. There are no host-device attachments.

Only tracing and a second QMP socket were added to the generated command.
The original QMP socket remains connected to vmlab.
Every VM is inside a user scope capped at 24 GiB with no swap.
`cgroup.txt` records the VM's scope. Disks use disposable qcow2 overlays;
`overlay-chain.json` identifies the store-backed chain.

## Exercised

- E1: First-boot setup completed and the Windows desktop appeared.
  The agent returned `Microsoft Windows [Version 6.1.7600]`.
- E2: An interactive probe changed to 640x480x16 and 800x600x16,
  performed GDI `BitBlt` and screen-copy operations, and restored the mode.
  Every interactive mode and blit operation returned success.
  The first probe ran through the agent service in session 0 and failed
  its display operations. That failure is retained in `exercise.log`.
  The successful run used Win+R on the visible desktop.
- E3: A write-through temporary file was written, flushed, read back as
  `HWREF DISK CHECK`, and deleted. The probe opened `D:\HWREF.TXT` and
  read the CD fixture's expected text.
- E4: The inbox Intel PRO/1000 MT driver obtained `10.219.87.2` through
  DHCP. Both ICMP requests to `10.219.87.1` received replies with no loss.
- E5: Start-menu and Escape chords, pointer movement, and a click were
  sent through vmlab. UHCI/tablet and PS/2 events are in the device summaries.
- E6: The stop ladder ended in QMP `SHUTDOWN` with `guest = true` and
  `reason = guest-shutdown`. `shutdown-events.json` records the event.

## Implementer evidence

`ide.md`, `network.md`, `display.md`, `usb.md`, `input.md`, `serial.md`,
`timers.md`, `floppy-dma.md`, and `platform.md` list register accesses,
counts, value sets, and first-use order. `event-values.json` retains command
opcodes and bounded argument samples. Indexed VGA VBE writes include mode
widths 640, 800, and 1024; heights 480, 600, and 768; and depths 16, 24, and 32.
The complete observed ATA/ATAPI opcode sets are in the IDE event samples.

`qtree.txt`, `pci.txt`, `mtree.txt`, `irq.txt`, `roms.txt`, and `usb.txt`
are HMP observations after the guest exercises. `acpi/` contains checksummed
guest-installed tables read through QMP physical-memory observation.
`smbios-entry.bin` and `smbios-tables.bin` are the guest's SMBIOS structures.
`fwcfg-files.json` inventories the fw_cfg file directory through HMP port
reads with vCPUs paused and memory-region tracing disabled. These host reads
are not counted as guest register accesses.

## Limits

The full stream contains 79,909,198 events. The raw prefix is capped at
64 MiB; 79,205,650 later lines were not retained. Register and event counts
cover the full stream. A value set or argument sample marked truncated does
not establish that later values or variants were absent. Trace overhead
makes boot duration unsuitable for performance comparisons.

KVM's in-kernel PIC/PIT paths do not necessarily produce QEMU device trace
events. IRQ totals and kernel-handled accesses are not a complete bus trace.
Physical-memory snapshots can add observer reads to VGA aperture rows;
those aperture counts are not guest-only.
No floppy media, agent reconnect, snapshot, suspend, or throughput test ran.
No S1 or S2 implementation changed. This reference does not establish a
vmlab-hv parity pass.

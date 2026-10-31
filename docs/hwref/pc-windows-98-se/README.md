# Windows 98 Second Edition

Captured on 2026-10-31 from `x86/windows-98-se@4.10.2222.5` through
`vmlab up win98`, using QEMU 11.1.2 and KVM. This is the Windows 98 guest,
not a Linux probe. `qemu-argv.json` records the exact running command.
`environment.json` records executable and template-metadata hashes.

## Machine and method

The `windows-9x` profile supplies pc/i440fx, SeaBIOS, IDE, Cirrus VGA,
PCnet, local-time RTC, and the legacy agent on COM1. COM2 carries the log.
The template supplies one vCPU, 128 MiB, and `cpu_model = "pentium3"`.
The scratch lab attaches one NIC to an isolated segment and a fixture CD
built through vmlab's `media` block. No floppy medium or host device is
attached. Only tracing and a second QMP socket were added to vmlab's argv.

The VM ran in a user scope capped at 24 GiB with `MemorySwapMax=0`.
`cgroup.txt` records the scope. Disks use a disposable store-backed overlay.
The first boot ran ScanDisk after reporting an unclean shutdown in the stored
image and completed additional Windows setup. The final capture is a second
boot of that same overlay, after those setup changes. The template store was
not modified. `overlay-chain.json` and `overlay-check.json` record the chain
and the post-stop qcow2 check.

## Exercised

- E1: Boot reached the Windows desktop and the legacy agent. `ver`
  returned `Windows 98 [Version 4.10.2222]` on the first run.
- E2: The Windows probe changed to 640x480x16 and 800x600x16,
  performed GDI blits and screen copies, and restored 640x480x8.
  Every mode and blit call returned success. The final stream contains
  2,918 Cirrus BitBLT starts. A mode-set-only device is insufficient.
- E3: A temporary file was written, flushed, read back as
  `HWREF DISK CHECK`, and deleted. `D:\HWREF.TXT` opened and returned
  the expected optical-media fixture text.
- E4: NIC enumeration and an ICMP attempt were exercised, but networking
  did not work. `ipconfig /all` reported only a PPP adapter with no address.
  Ping reported `Destination host unreachable`. The final trace contains
  one PCnet reset and no PCnet initialization or PCnet I/O accesses.
  This capture does not establish the Windows PCnet driver's requirements.
- E5: Keyboard and pointer input were sent through vmlab. Timed QMP key
  sequences also navigated System Properties. The device tree showed
  Advanced Power Management support. Exact input delivery was not independently
  validated; the input trace and the QMP chord log are retained.
- E6: Both stop attempts ended in QMP `SHUTDOWN` with `guest = false`
  and `reason = host-qmp-quit`, after the power-button event.
  A clean guest shutdown is **not proven**. The first run's failure evidence
  is in `first-run/`; the final event is in `shutdown-events.json`.

## Cirrus requirements established by this run

The complete BitBLT control-field sets are captured separately from bounded
argument samples in `indexed-registers.json`:

| Field | Values | Starts |
|---|---|---:|
| ROP | `0x0d`, `0x59`, `0xd6`, `0x05` | 2918 |
| Mode | `0xc0`, `0x80`, `0x01`, `0x00`, `0xd0`, `0x90` | 2918 |
| Mode extension | `0x00` | 2918 |
| Write mask | `0x00` | 2918 |

`display.md` lists the extended graphics-register writes with counts and
value sets. `event-values.json` includes bounded width, height, pitch,
source-address, and destination-address samples. Those samples are truncated;
they are not the complete set of geometries or command orderings.

## Other evidence and bounds

Per-device Markdown files record ports and MMIO, access counts, value sets,
and first-use order. `commands.json` counts ATA/ATAPI command opcodes.
Static HMP files contain qtree, PCI, memory regions, IRQs, ROMs, and USB.
`acpi/` contains checksummed guest-installed tables read through QMP.
The SMBIOS entry point and structure binaries are included.
`fwcfg-files.json` inventories the firmware directory with vCPUs paused
and memory-region tracing disabled. Host fw_cfg reads are excluded from
guest-access counts. `cmos-observed.json` contains RTC-indexed samples,
not an atomic full CMOS dump.

The final stream contains 48,172,390 events. Its raw prefix is capped at
64 MiB; 47,482,383 later lines were omitted from the raw file. Counts cover
the full stream, with explicit truncation flags on bounded value sets.
Physical-memory snapshots can add observer reads to VGA aperture rows;
those aperture counts are not guest-only. KVM's in-kernel PIC/PIT paths are
not a complete QEMU bus trace.

No floppy medium, snapshot, reconnect, suspend, or throughput test ran.
No S1 or S2 implementation changed. The NIC and shutdown gaps prevent this
capture from proving complete Windows 98 behavior or vmlab-hv parity.

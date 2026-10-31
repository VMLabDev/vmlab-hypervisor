# In-place reset and OVMF first paint

## Reset behavior

Machine descriptions default to in-place reset. Set `exit_on_reset: true` to
retain the earlier clean exit and `shutdown {reason: guest_reset}` event.
In-place reset emits `reset` after restoring the machine, then resumes it.
The existing stop/resume notifications bracket the operation.

The owner confirmed this default on 2026-10-31 to match vmlab's QEMU launch
without `-no-reboot`. It supersedes the specification's earlier phase-1
microvm wording about reporting every triple fault as `guest_reset`.

The event worker serializes reset against snapshots and run-state changes.
It parks every vCPU and completes deferred KVM I/O before resetting devices.
`Resettable` clears volatile device state without replacing host connections.
Devices with ordinary snapshot state restore their captured power-on state
through the same reset path. RTC calendar and CMOS survive; flash VARS and
attached disk contents survive. TPM reset sends swtpm STOP and INIT with
DELETE_VOLATILE, keeping the same backend connection.

Reset restores the initial CPU registers, MSRs, XSAVE, XCRs, debug registers,
LAPIC, PIC, IOAPIC and PIT. Firmware starts at the architectural reset vector;
direct boot reloads the kernel, initrd, ACPI tables and bootstrap page tables.
APs wait for INIT/SIPI. On the tested AMD host, a firmware triple fault also
requires a protected-mode transition before restoring boot state.

The control, serial-port, NIC and display listeners and connections are retained.
The agent starts again inside the rebooted guest and completes a new handshake.
Partial NIC stream framing is retained so a reset cannot corrupt the host's
length-prefixed Ethernet stream.

## Evidence at the control socket

S1 launch tests reboot the microvm twice through each Linux reboot method:
triple fault, keyboard controller, PCI CF9 and ACPI. They retain one process,
one control connection and one NIC connection. KVM firmware payload tests cover
CF9, ACPI, i8042, triple fault, the Hyper-V reset MSR and reset from SMM.
They also check AP startup state, synchronized TSCs, and clearing SYSENTER_CS
and DR0. A four-vCPU payload starts each AP through directed INIT/SIPI messages,
resets the machine, and starts all three APs again.

Real-template tests issue reboot through the guest agent. Ubuntu 26.04.3 with
SeaBIOS retained PID 2660113; Windows 10 19045.2006.9 with OVMF retained PID
2701277. Both completed another agent handshake, executed a command and
published non-black frames on the existing display connection. The tests no
longer relaunch Windows after its setup reboot.

Device tests cover transport queues and MSI-X, AHCI media retention, xHCI,
VGA fallback after native GPU use, RTC continuity, VARS persistence and swtpm
PCR reinitialization. Physical passthrough hardware and accelerated GPU reset
were not exercised by these tests.

Virtio interrupt handles carry a host-only activation epoch. Callbacks from
before reset or snapshot load cannot change the new transport's ISR or
configuration generation. Layered snapshot restore also delays worker activation
until both transport state and wrapped-device queue indices have been restored.
The always-running event worker exposed a concurrent USB detach race. Detach now
serializes controller access and attachment removal; a 16-caller regression
requires one successful detach and one deletion event.

### Windows validation limit

Earlier debug runs intermittently stopped in Windows `HaliHaltSystem`, identified
using the guest's kernel image and Microsoft's matching public symbols. This
occurred before the agent became ready after a reset, and after snapshot load.
An isolated read-only KVM observer captured bugcheck `0x5c`, subtype `0x110`,
timer problem `0x19`, status `0xc0000001`. The matching kernel's public symbols
and unwind metadata locate the failure in `HalpTimerInitializeClock`: its clock
interrupt self-test did not observe an interrupt. The selected interrupt source
and underlying cause are still under investigation. An untouched baseline using
process relaunch passed, so a reset-related cause remains possible.

After the interrupt-epoch and restore-order fixes, the full gate's Windows reboot
and snapshot cases passed. Four additional isolated snapshot runs passed in
120.72, 114.20, 115.11 and 118.07 seconds. Earlier failures also occurred after
those fixes; the successful runs do not establish that the intermittent halt is
resolved. Investigating it remains follow-up work.

Retained guest disks and logs are under `.scratch/reset-artifacts.3uNABW/` rather
than the host's RAM-backed `/tmp`. Their machine descriptions retain the original
temporary paths and need path updates before another launch.

## First-paint measurements

`scripts/first-frame.py` starts each VM paused, copies fresh VARS, connects RFB,
then measures resume to the first changed frame containing non-black RGB pixels.
It excludes QEMU's non-black paused placeholder. The probe attaches no disk,
uses q35, two host vCPUs, 4 GiB RAM and virtio-vga, and reaps each VM.

Host: Ryzen 9 9950X, KVM, Linux 7.2.10-1-cachyos. QEMU: 11.1.2.
The five-run release comparison used vmlab's OVMF 2026.08+ds-2 unchanged on
both hypervisors. CODE SHA-256:
`7e34e3b83224968bb0714c0ebc88f75c5c37b4cc892c902e511ccc7a7c107f94`.
The baseline executable was built from `1b75edf7d1800b6fcc027fcdc1f2e72636f23e15`.
Raw measurements are in `reset-first-frame.csv`.

| Measurement | Before | After |
| --- | --- | --- |
| Release vmlab-hv, five ordinary runs | 0.563-0.586 s | 0.571-0.651 s |
| QEMU, same firmware, five ordinary runs | 1.378-1.417 s | unchanged reference |
| Instrumented debug boot under concurrent guest I/O | 25.143 s; next run exceeded 30 s | 1.207-1.456 s, six runs |
| Flash fdatasync calls in those instrumented boots | 5,653; 5,304 at timeout | 18-19 |
| Time inside fdatasync | 23.710 s; 28.882 s at timeout | 0.078-0.110 s |

The instrumented system firmware CODE SHA-256 was
`2febd26c0b4cf95a636a941afa37d64a552723443ed9ac72f763f6840da98cb4`.
Its first three after-runs used that image; the remaining three used vmlab's
image. Interposition measured actual libc fdatasync latency; it injected no
delay. Host I/O load was uncontrolled, so these samples demonstrate the delay
mechanism, not a fixed speedup ratio. Ordinary release latency did not improve.

The cause was `fdatasync` after every programmed NOR word. The change batches
durability on a nominal 16 ms interval and flushes at reset, snapshot and
shutdown. File writes still complete before flash reports ready. This permits
a short host-power-loss durability window between barriers. Flush errors stop
the VM. GOP setup, guest-visible VGA modes and firmware code are unchanged.

Run the comparison with the same CODE and VARS inputs:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  python3 scripts/first-frame.py --binary target/release/vmlab-hv \
  --code /home/wil/dev/vmlab/guest/dist/firmware/x86_64/OVMF_CODE_4M.fd \
  --vars /home/wil/dev/vmlab/guest/dist/firmware/x86_64/OVMF_VARS_4M.fd --runs 5
```

The reset implementation uses the KVM API and documented swtpm control protocol:
https://docs.kernel.org/virt/kvm/api.html
https://man.archlinux.org/man/swtpm_ioctls.3.en
No QEMU source was read.

### Follow-up with vmlab's recording configuration

The recording configuration uses one vCPU, 512 MiB, a linked Alpine disk and
`/usr/share/qemu/vgabios-virtio.bin`. On the same OVMF build, the earlier
p2-wintimer measurements found:

| Executable | VARS on host disk | With concurrent fdatasync load |
| --- | --- | --- |
| Before reset batching, `1b75edf` | All five exceeded 30 s | Not measured |
| Reset batching, `e740fb1` | 0.573-0.598 s | 0.640-0.724 s |
| Background sync, `18732e6` | 0.511-0.542 s | 0.552-0.598 s |

`18732e6` moves the batched fdatasync off the vCPU threads. Reset, snapshot
and shutdown still wait for durable VARS. The previous agent's measurements
are recorded in `wintimer-handoff.md`.

A new five-pair verification on 2026-10-31 measured 0.491-0.531 s for
vmlab-hv and 1.083-1.744 s for QEMU firmware paint. Raw samples are in
`wintimer-first-frame.csv`. VARS and temporary overlays were on the host disk.
The Alpine image was version 3.23.10. Other guests were running on the host.

All five QEMU runs were already non-black while paused. In that measurement,
all five vmlab-hv runs were black while paused. QEMU's paused text, "Display output is not
active.", is host output. The probe excludes it from firmware-paint timing.
A recording check that accepts that text measures a different event from
the first guest-painted frame.

The owner subsequently selected a host placeholder for vmlab-hv too.
The display server now publishes "Display output is not active." on a dark
background before the first resume and whenever the guest has no active
scanout. It returns after reset. Native clients that negotiate `frame_flags`
receive `FrameFlags::PLACEHOLDER`; older clients and RFB clients see the image
without that metadata. Guest-paint checks exclude flagged frames. A configured
black guest scanout remains a guest frame.

The updated `first-frame.py` uses the native flag to exclude vmlab-hv's
placeholder, and the paused-image hash to exclude QEMU's placeholder. A paired
verification measured 0.513 s for vmlab-hv's first non-black guest frame and
1.091 s for QEMU. Both were already non-black while paused. The native wire
layout and capability negotiation are specified in
`../crates/vmlab-hv-display/PROTOCOL.md`.

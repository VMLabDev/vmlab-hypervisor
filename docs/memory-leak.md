# Memory growth investigation, 2026-10-31

Full TPM VM attachment is unavailable in this branch. An additional q35
virtiofs mount/write probe timed out. Neither limitation is reported as a
successful end-to-end test below.

## Measurements

Each scenario ran for 600 seconds with a fresh overlay on the local Ubuntu
26.04.3 template, two vCPUs, and 4096 MiB RAM. RSS and cgroup v2 memory were
sampled every five seconds. Every VM command ran in a scope with
`MemoryMax=48G` and `MemorySwapMax=0`, with at most two booting test processes.

| ID | Scenario | RSS range, final 300 seconds, KiB | Peak scope memory, MiB | Final fds / threads |
|---|---|---:|---:|---:|
| M1 | Std VGA, no client | 952204 | 961.57 | 16 / 10 |
| M2 | Native client, no buffer releases or further reads | 939396 | 941.21 | 21 / 12 |
| M3 | RFB client, continuous raw updates | 964184 | 972.72 | 21 / 13 |
| M4 | 20 online save/load/delete cycles, then idle | 4298644 | 5701.53 | 19 / 11 |
| M5 | Virtio-vga, active virtio-gpu DRM framebuffer | 937136 | 940.07 | 19 / 11 |
| M6 | Linux VM, swtpm backend traffic, persistent virtiofsd | 968344-968352 | 979.00 | 24 / 11 |

The 720 samples are in `memory-leak-measurements.csv`. Cgroup totals include
the probe and helpers. M1-M4 used the original executable; M5-M6 used the
fixed executable. Kernel logs confirmed `virtio_gpudrmfb` as the primary
framebuffer in M5. None of these completed runs was OOM-killed.

M6 exercised the existing TPM device/backend separately because machine
validation refuses TPM attachment with "OVMF and TPM require phase 2c".
It completed at least 59,300 random requests and 593 TPM save/restore cycles.
Peak RSS was 12,308 KiB for swtpm, 3,132 KiB for the TPM driver process, and
7,932 KiB combined for virtiofsd and its sandbox child. The q35 guest saw
the filesystem PCI device; successful filesystem I/O is not claimed.

## Confirmed findings and fixes

- F1: Serial and network transmit staging grew while a socket peer stopped
  reading. The serial device test crossed 1,114,112 staged bytes after 17
  writes; the network test crossed 1,116,024 bytes after 273 writes.
  The corrected S1 reproduction resolved the named character device through
  sysfs before writing. A 64 MiB write raised VMM RSS from 119,428 to
  187,824 KiB before the fix, versus 119,492 to 124,812 KiB afterward.
  Transmit staging now applies backpressure at 1 MiB plus at most one
  descriptor chain. Network frames are additionally bounded by the wire
  limit. Writable socket events resume pending descriptors; disconnected
  serial clients release pending output. S1 drained all 64 MiB intact.
- F2: Dropping a `Vm` detached its vCPU threads and retained guest RAM.
  The 16 MiB test VM retained one shared-state reference and one RAM reference;
  serial output grew from 9,973 to 18,821 bytes in 150 ms after drop.
  `Vm::drop` now requests exit and joins vCPU and timer workers, including
  partially constructed machines. The regression drops 16 running or paused
  VMs and requires zero surviving shared-state and RAM references.
- F3: The control handler and event sink formed an ownership cycle. Control
  and display listeners also survived their VM. Weak ownership, owner-aware
  accept loops, socket shutdown, and worker joins now release them. Quit
  replies finish before the control server closes connections. Separately,
  1,000 disconnected control clients retained 1,000 weak registry entries
  without broadcasts; the fixed registry retains at most one expired entry.
- F4: Launch-test panics and virtiofsd startup failures could bypass child
  cleanup. Hwref swtpm cleanup sent termination without reaping or escalation.
  Guards now own process groups and reap children, including descendants.
  Parent-death signalling covers abrupt runner exits in Rust and Python.
  Tests cover a SIGTERM-ignoring helper, a descendant after its parent exits,
  injected startup panic, a 30 ms watchdog deadline, and abrupt runner exit.
  Shared file locks default to two VMs across S1, acceptance, and hwref
  processes. A third slot request times out while both slots are held.
- F5: Continuous frame capture did not retain frame history in these probes.
  A regression publishes 4,000 frames with no client and with three held
  buffers, requiring at most four live pixel allocations. M1-M3 and M5
  plateaued rather than growing with frame count.
- F6: Whole-vmstate restore is a transient allocation, not a demonstrated
  cumulative leak. M4 peaked at 5,052,816 KiB RSS and settled at 4,298,644 KiB.
  Post-cycle RSS rose by 14,136 KiB across the first three cycles, then
  remained constant through cycle 20. Streaming restore was not changed.

## Verification and limits

`just ci::check` passed under the memory cap with `RUST_TEST_THREADS=1`:
fmt, clippy with warnings denied, and 488 reported test passes. The 1 GiB
throughput baseline remains ignored by default. The Alpine BIOS countdown
test skipped because `target/s1/alpine-bios.qcow2` is absent.

The hwref Python suite passed 45 tests with one opt-in P2 recapture test
skipped. The actual QEMU clock oracle, S1 serial-memory regression, S1
panic cleanup, display shutdown, and online snapshot stress tests passed.
No `todo!` or `unimplemented!` remains in the changed code. Every process
group started for this investigation was stopped; completed job panes were
closed. Other agents' jobs were left untouched.

No device trait, snapshot format, or wire protocol changed. The server APIs
gain explicit shutdown and owner-aware serving. Test support gains
`ManagedChild`, `VmPermit`, and `MachineSpec::extra_ports`; `Guest::child`
now owns a `ManagedChild`.

The 03:18 journal identifies the terminal scope with 162.5G swap usage and
records 186,226,802,688 bytes used / 200,111,423,488 total in its OOM decision.
It provides no per-process RSS history. These reproductions establish leak
paths but cannot identify which PIDs accounted for the historical event.
Windows and full VMM TPM attachment were not tested. The additional q35
virtiofs mount/write timeout remains a separate functional issue; the
existing microvm virtiofs integration tests passed.

## Repeat the probes

Build `vmlab-hv`, the `memory_agent` example in `vmlab-hv-tests`, and the
`memory_tpm` example in `vmlab-hv-devices`. Select `idle`, `native`, `rfb`,
`snapshots`, `virtio`, or `helpers`:

```sh
systemd-run --user --scope -q -p MemoryMax=48G -p MemorySwapMax=0 -- \
  python scripts/memory_probe.py idle --template /path/to/disk.qcow2 \
  --output /path/to/new-results
```

The probe refuses an uncapped scope, acquires a VM slot, creates a fresh
overlay, and cleans up its children. `VMLAB_HV_TEST_VM_LIMIT` controls the
shared slot count, default 2. Rust test children have a 600-second default
deadline, configurable with `VMLAB_HV_TEST_PROCESS_TIMEOUT_SECS`.

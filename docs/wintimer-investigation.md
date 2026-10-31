# Windows timer and snapshot investigation

This report records the 2026-10-31 follow-up to `wintimer-handoff.md`.
Windows remains unresolved. The final full CI gate fails its Windows snapshot
case, and the required 20-reset and 20-load proof is unmet.
The investigation uses VMM state, KVM tracing, guest agent commands, screenshots,
and Windows event logs. No Windows binary disassembly, symbol analysis, or
memory-dump analysis was performed. No QEMU source was read.

## Reproduced defects and fixes

- F1: Rapid snapshot loads could leave the guest TSC ahead of the saved instant.
  KVM's legacy IA32_TSC write heuristic can match successive writes instead of
  applying rollback. The S1 reproduction observed an 81 ms error. New snapshots
  capture exact KVM TSC offsets with a common host-TSC and KVM-clock pair.
  Restore applies offsets directly and rejects a changed TSC frequency. Hosts
  without the offset interface retain the legacy path. Old snapshots still load.
  `rapid_snapshot_loads_roll_back_guest_tsc` fails before the fix and passes after it.
- F2: A connected console worker could DMA after queue indices were captured,
  leaving the saved RAM and queues inconsistent. Snapshot save and load now hold
  a synchronous device barrier across RAM, kernel, and device state. Console
  and network workers acknowledge the barrier after in-flight callbacks finish.
  `snapshot_stops_console_dma_before_saving_ram` exercises S1 and verifies both
  save and restore boundaries. A network test verifies deferred receive DMA.
- F3: Hyper-V HCALL exits require a subsequent KVM_RUN to complete their result
  and instruction pointer. The VMM completed deferred I/O and MMIO before
  parking, but omitted HCALL. Loading a snapshot could therefore let a stale
  completion overwrite restored registers and use the old hypercall's cached
  next RIP. A protected-mode PostMessage loop reproduced this on its first
  load. Completing HCALL before parking passes the same S1 reproduction for
  20 loads. SynIC notification exits do not require this completion.
- F4: The snapshot acceptance test could reconnect after an automatic guest
  reboot and mistake that for a successful load. It now rejects reset events
  after each restored command, even when file contents match. Failure output
  includes readable guest boot time and Windows event records.

These defects are independently reproduced. Their fixes do not by themselves
establish the cause of the historical `0x5c/0x110/0x19` halt.

## Timer and interrupt evidence

Windows programs STIMER0 through KVM's Hyper-V implementation. Observed steady
state is configuration `0x3000b`, count `156250` in 100 ns units, and SINT3 at
vector `0xd1`. This is periodic 15.625 ms delivery through a SynIC message page.
Other vCPUs use one-shot STIMER0 configuration `0x30009` between idle periods.
The sampled LAPIC timer is masked and HPET is disabled in the relevant failure.

Reset restores captured power-on SynIC, STIMER, LAPIC, PIT, and irqchip state.
SynIC and STIMER MSRs read zero immediately after reset. Snapshot load restores
RAM before kernel state, then device state. Kernel restore orders irqchips,
PIT, KVM clock, per-vCPU architectural state and MSRs, LAPIC, TSC deadline, and
vCPU events. The deadline MSR follows LAPIC restore because SET_LAPIC can cancel
an armed deadline. All vCPUs remain parked throughout.

The owner-provided trace records actual KVM MSR writes, STIMER programming and
expiration, SynIC delivery, APIC acceptance, and MSI and irqchip activity.
Filtering uses the test process's thread IDs, not names shared by other VMs.
The retained window is `.scratch/wintimer-trace/snapshot100-window.log`.
Its process was PID 3740629 with vCPU TIDs 3740651 through 3740654.

That run failed on snapshot load 3. Windows event 1001 reports bugcheck
`0x50 (0xfffffffffffffff8, 0x44, 0xfffff8066e28086f, 0xe)` followed by an
automatic reboot. A screenshot shows the new boot's lock screen. The guest's
eight-byte test file then reads as zeros. This is not evidence of a successful
load with only file corruption. The historical `0x5c` was not observed in this
run.

The third restore starts at trace time `40366.113808`. The last posted SynIC
interrupt before the crash is at `40368.602855`. At `40369.395`, the VMM samples
a timer message of type `0x80000010` in SINT3, vector `0xd1` pending in IRR,
IF cleared, and TPR `0xf0`. The guest resets at `40386.2264`; new-boot SynIC
delivery begins at `40387.849561`. Thus a timer interrupt was pending but masked
at the sampled failure state. This does not establish why the guest stopped
accepting it.

Periodic STIMER expiration returning zero is not sufficient evidence of fresh
delivery. KVM can coalesce a periodic timer into an occupied SynIC message
slot. The trace analysis therefore distinguishes expiration from SynIC IRQ
delivery and APIC acceptance. No failed SynIC IRQ return was found in this
process's retained window.

Removing the Hyper-V `avic` CPUID recommendation is not a reliable way to disable
hardware AVIC on this host's KVM. The resulting 20-reset pass is useful as a
profile comparison, but does not rule AVIC in or out. No restore-order changes
were made on the strength of that comparison.

After the HCALL fix, a new snapshot soak again failed on load 3 with the same
`0x50` parameters except the randomized instruction address. Its readable event
report is `.scratch/hcall-snapshot20.log`, and its retained KVM window is
`.scratch/wintimer-trace/hcall-snapshot20-window.log`. The VM was PID 3863554.
This establishes that HCALL completion alone does not resolve this crash.

The paired reset soak passed four resets, then timed out while requesting the
fifth reboot through the agent. Its final timer state has IF set, TPR zero,
an empty SynIC timer slot, three halted vCPUs, and one active userspace vCPU.
Successful SynIC delivery continues through `42075.939397`, immediately before
VM shutdown at `42076.100361`. This is not the high-TPR snapshot freeze.
The failure records are `.scratch/hcall-reset20.log`,
`.scratch/vms/hv-p1-Nie3rE/stderr.log`, and
`.scratch/wintimer-trace/hcall-reset20-failure.log`.

## Display and first paint

`18732e6` removes background VARS fdatasync from vCPU execution. The earlier
disk-backed baseline exceeded 30 seconds in all five runs. Background sync
measured 0.511-0.542 seconds, or 0.552-0.598 seconds with concurrent fdatasync
load. Reset, snapshot, and shutdown still wait for durability.

Five new paired runs measured vmlab-hv guest paint at 0.491-0.531 seconds and
QEMU firmware paint at 1.083-1.744 seconds. The raw data is
`wintimer-first-frame.csv`. These measurements use the same OVMF build, one
vCPU, 512 MiB, virtio-vga, and an Alpine disk on the host filesystem.

The owner selected a host placeholder. The display now publishes
"Display output is not active." while paused before first resume and whenever
there is no active guest scanout, including after reset. A configured black
scanout remains a real guest frame. RFB clients see the placeholder image.
Native clients that negotiate `FRAME_FLAGS` receive `FrameFlags::PLACEHOLDER`.
Unmodified clients receive zero header flags and retain the version-1 payload.
The wire test pins the placeholder flag bytes. S1 checks cover paused initial
output, guest paint, and the placeholder returning after reset.

Guest-paint checks now exclude the flag. The revised first-frame probe measured
0.513 seconds for vmlab-hv and 1.091 seconds for QEMU, with both already showing
host output while paused. Protocol details are in
`../crates/vmlab-hv-display/PROTOCOL.md`.

## Shared interfaces

`Snapshot` and `PciSnapshot` gain default `quiesce` and `resume_snapshot`
methods. They delimit asynchronous guest-memory access. The console and network
implement them; VirtioPci forwards them. New snapshots can contain the optional
`tsc-offsets` section. Native display frames carry `FrameFlags`; clients negotiate
the `FRAME_FLAGS` capability. VGA snapshot version 2 preserves whether the guest
has programmed a legacy scanout and accepts version-1 snapshots.

## Validation limits

The required Windows proof is not complete. The latest instrumented reset soak
passed four resets before an agent-command timeout. The snapshot soak passed
two loads before a bugcheck and automatic reboot. Earlier 20-reset and 40-load
passes do not override these failures or prove the historical halt fixed.

`just ci::check` passes formatting and workspace clippy with `-D warnings`, but
fails `acceptance::windows_10_ovmf_agent_and_snapshot`. Its first restored
command times out. The final state is firmware execution, disabled STIMERs,
and a programmed firmware LAPIC timer, after another reset request. The other
15 executable acceptance tests pass, including Windows reset and Windows
Server with TPM. The log is `.scratch/ci-final.log`.

An earlier full-gate attempt used a long TMPDIR and failed the virtio-fs socket
path-length limit. The final attempt uses `/home/wil/hvci.UPcw6O`, on disk,
and passes that integration test when run independently. Every VM-bearing
command uses the 24 GiB systemd memory cap with no swap allowance.

One regression command briefly overlapped the two Windows VMs, exceeding the
two-VM concurrency limit with a small payload VM. That command was stopped.
The subsequent serial run of all 16 snapshot tests passes, including the TSC,
DMA-barrier, and HCALL reproductions. The log is
`.scratch/hcall-regressions-final.log`.

Independent runs complete the checks that the failed gate did not reach:

```sh
cargo test -p vmlab-hv --test '*'
cargo test --workspace --exclude vmlab-hv
```

They use the same short disk-backed TMPDIR, serial test execution, and capped
systemd scope. All 16 executable integration tests pass. The remaining crates
complete 658 tests with zero failures and two ignored tests. This includes all
snapshot regressions and ten Linux snapshot cycles during fsync-heavy writes.
Logs are `.scratch/ci-integrations.log` and `.scratch/ci-other-crates.log`.
Herdr's final wait encounters a missing-marker parser error, but the job pane
shows `EXIT=0` and both Cargo logs finish successfully. These independent results
do not make the full gate green.

The owner-provided root trace is no longer needed. Failure windows are retained
in this worktree rather than depending on the growing shared trace file.

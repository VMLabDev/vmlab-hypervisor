# Windows timer and snapshot investigation

This report records the 2026-10-31 follow-ups to `wintimer-handoff.md`.
Round 2 identifies lost user CET state as the cause of the reproduced Windows
snapshot bugcheck `0x50`. Windows passes 20 consecutive loads and 20 consecutive
resets. The final `just ci::check` passes 693 Rust tests with zero failures
and two existing ignored benchmarks.
The investigation uses VMM state, KVM tracing, guest agent commands, screenshots,
and Windows event logs. No Windows binary disassembly, symbol analysis, or
memory-dump analysis was performed. No QEMU source files were opened or used.

## Round 2: user shadow-stack state lost on load

F5: Snapshot save used an architectural MSR whitelist. It omitted
`IA32_U_CET` at `0x6a0` and `IA32_PL3_SSP` at `0x6a7`. Windows changes these
registers during thread switches. Load restored the saved registers and RAM,
but left CET control and the user shadow-stack pointer from the later thread.

The stopped-state differential records a saved vCPU 1 with `U_CET=1` and
`PL3_SSP=0x6aea5fef28`. Immediately after load, both read zero. Other vCPUs
also retain a later thread's CET values. Main-RAM hashes match the saved RAM.
The ordinary registers, special registers, FPU, XSAVE, XCRs, debug registers,
CPUID, LAPIC, vCPU events, MP state, and nested-state header match too.
The remaining non-clock difference is `IA32_SPEC_CTRL`, also absent from
the whitelist.

That unchanged binary fails on load 9. Windows event 1001 reports
`0x50 (0xfffffffffffffff8, 0x44, 0xfffff8002848086f, 0xe)` and the strict
acceptance check rejects its automatic reboot. AMD's page-fault error-code
definition assigns bit 6 to shadow-stack access and bit 2 to user mode.
A shadow-stack push with a zero pointer accesses `-8`. This explains the
otherwise unusual fault address and error bits without inspecting Windows code
or its memory dump. The register mismatch is directly observed. Its connection
to the Windows fault follows the architectural error-code definition.

The processor reference is AMD64 Architecture Programmer's Manual, Volume 2,
revision 3.44, section 8.4.2:
https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content

Save now enumerates `KVM_GET_MSR_INDEX_LIST` and adds the expanded Hyper-V
SynIC and STIMER ranges. It probes entries individually, so one MSR unavailable
under a vCPU's CPUID policy cannot truncate the rest of the list. Required
Hyper-V entries still fail explicitly if unreadable. Load restores every saved
entry and checks the complete write count. The LAPIC still precedes the
TSC-deadline MSR. XCRs and debug registers now have optional snapshot sections.
Older snapshots remain readable but cannot recover CPU state they never saved.

`control_snapshot_restores_cet_user_shadow_stack` fails against the original
snapshot code with `(U_CET, PL3_SSP) = (0, 0)` instead of `(1, 0xa000)`.
It passes with the fix, including guest `RDMSR` observations after resume and
both enabled and disabled CET contexts. The separate extended-CPU regression
first fails for DR0, XCR0, IA32_PAT, and IA32_SYSENTER_CS, then passes.

The baseline evidence is retained in `.scratch/round2-windows-audit.log`,
`.scratch/round2-audit/`, and `.scratch/round2-diff-before.log`.
The CET reproduction is `.scratch/round2-cet-before.log`.
The fixed 20-load soak is `.scratch/round2-snapshot20.log`.
Its audit is `.scratch/round2-audit-fixed/`, with comparison results in
`.scratch/round2-diff-after.log`. All 20 loads have zero non-clock differences.
All tests use one VM at a time under the 24 GiB systemd cap with swap disabled.

An earlier uninstrumented run of the unchanged binary passes 20 loads.
That run is `.scratch/round2-baseline.log`. A passing soak alone did not
exclude the state-loss defect.

### Snapshot-state coverage

`VMLAB_HV_SNAPSHOT_AUDIT` names an existing directory for stopped-state JSON
records. Save records the source state. Load records RAM before kernel restore
and the complete state after device restore, before any vCPU resumes.
Audit errors are logged without interrupting snapshot save or load.
`python scripts/snapshot-audit.py <directory>` compares each load with its save.
Six Python tests check the comparator's clock exclusions.

| State | Capture and restore | Audit result |
| --- | --- | --- |
| General and special registers | GET and SET_REGS, GET and SET_SREGS | Byte-identical |
| FPU and XSAVE | GET and SET_FPU, GET and SET_XSAVE | Byte-identical; host XSAVE2 size is 4096 bytes |
| XCRs and debug registers | New optional sections | Byte-identical; independently reproduced loss before the fix |
| Architectural and Hyper-V MSRs | KVM index list plus expanded TLFS ranges | CET and SPEC_CTRL differ before the fix; only clock MSRs differ after it |
| LAPIC and vCPU events | Existing sections, events last | Byte-identical, including pending exceptions, interrupts, and NMIs |
| PIC and IOAPIC | Existing GET and SET_IRQCHIP sections | Byte-identical |
| PIT and KVM clock | Existing GET and SET_PIT2 and CLOCK sections | Only documented host timestamps differ |
| TSC frequency and offsets | Existing exact-offset restore | Frequency and inter-vCPU offset differences preserved |
| CPUID | Fixed at construction; unchanged machine digest | Byte-identical without SET_CPUID after KVM_RUN |
| Nested state | Nested VMX and SVM masked by the Windows CPU policy | Inactive 128-byte header remains byte-identical, including GIF |
| SynIC, hypercall, reference-TSC, and VP-assist pages | Guest RAM plus their MSRs | RAM hashes and non-clock MSRs match before vCPU resume |
| Hyper-V crash and invariant controls | Included in the full KVM MSR list | Byte-identical |

The comparator excludes three running counters: IA32_TSC, Hyper-V VP_RUNTIME,
and TIME_REF_COUNT. It also excludes KVM clock timestamps, the common host-TSC
translation, PIT `count_load_time`, and the LAPIC timer's live current count.
It still checks TSC frequency, relative vCPU offsets, timer configuration,
pending interrupts, and all remaining bytes. The KVM API contract is at
https://docs.kernel.org/virt/kvm/api.html.

F6: RAM capture walked `MemLayout::regions`, which omits mappings firmware
adds later. PAM shadow RAM already had a device snapshot, but non-secure
SMRAM did not. Save now walks every region in `GuestRam.mem`. Load validates
the complete stream before memory writes and clears only its recorded regions.
The legacy boot-layout region list remains accepted for old snapshots.
`control_snapshot_restores_ram_outside_the_boot_layout` fails before this
change with `after!` instead of `before`, then passes through the control socket.
Evidence is `.scratch/round2-memory-before.log` and
`.scratch/round2-memory-after.log`. This is a separate omission, not the
identified cause of the CET fault.

VGA framebuffer memory remains owned by the VGA device snapshot. Writable
pflash VARS remain in the pflash section. Immutable firmware ROM mappings do
not need rollback. Secure SMRAM has its own `smm-ram` section, isolated from
the DMA view. No dirty bitmap selects snapshot RAM pages. Every recorded page
is captured under the existing device DMA barrier. Assigned VFIO and host USB
devices, and accelerated GPUs, remain refused for online snapshots.

The temporary periodic timer sampler and verbose Hyper-V exit tracing have
been removed. The opt-in exit-state dump, crash report, strict reboot check,
and acceptance cycle controls remain. The older findings below describe the
state of the investigation before F5 and F6.

### Round 2 validation

The required Windows soaks pass. The reset soak verifies 20 consecutive
reboots, an agent command after each, and preserved process, control, and display
connections. Its log is `.scratch/round2-reset20.log`. The retained VMM log
contains 22 reset requests, including setup and firmware reset, and no guest
crash reports. The snapshot soak verifies 20 consecutive loads, correct file
rollback, and no automatic reboot. The strict F4 check is unchanged.

The first full `just ci::check` passes 693 Rust tests with zero failures and
two existing ignored benchmarks. Those benchmarks measure 1 GiB sequential
disk throughput and live PipeWire loopback latency. The log is
`.scratch/round2-ci.log`. This includes the Linux fsync-heavy restore test,
container snapshots and network traffic, firmware, display, and TPM checks.

The final diagnostic change also passes all three control-socket regressions
with an invalid audit destination. Twelve audit-file errors are logged without
interrupting save or load. The log is `.scratch/round2-audit-errors.log`.
The full gate also passes on that final code, again with 693 tests passed,
zero failures, and the same two ignored benchmarks. Its log is
`.scratch/round2-ci-final.log`. Both full gates use `RUST_TEST_THREADS=1`,
`VMLAB_HV_SNAPSHOT_CYCLES=20`, the release binary, a short disk-backed TMPDIR,
and `systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0`.
The final gate therefore repeats the 20-load Windows proof without audit overhead.

The six Python audit-comparator tests pass separately with
`python -m unittest discover -s scripts -p 'test_snapshot_audit.py'`.
Real USB and VFIO checks were not run because no device was designated through
`VMLAB_HV_TEST_USB_DEVICE` or `VMLAB_HV_TEST_VFIO_DEVICE`. No host device was
claimed, reset, or rebound.

Round 2 changes no control-protocol types or device traits.
`snapshot::format::Reader` gains `Clone` for validation before RAM writes.
The `xcrs` and `debugregs` sections are optional. New RAM streams include the
firmware-added regions. Old streams remain accepted, but snapshots predating
F5 cannot reconstruct omitted CET state. Take new Windows snapshots to use
the fix.

The historical `0x5c` failure was not reproduced or independently attributed
to CET. No root tracing, Windows disassembly, symbol analysis, or memory-dump
analysis was needed for the reproduced `0x50`.

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

## Round 1 validation limits

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

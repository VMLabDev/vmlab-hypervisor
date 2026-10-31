# Handoff: p2-wintimer (Windows timer halt, OVMF first paint)

Branch `wp/p2-wintimer`. Commits:

- `18732e6` perf(vmm): sync OVMF VARS batches off the vCPU threads. Finished.
- `95fb43c` wip(vmm): diagnostics and soak harness. Investigation aids only;
  prune before merge.

## Part 2: OVMF first paint (finished, apart from one decision)

Measured with `scripts/first-frame.py --vcpus 1 --memory 512 --disk <alpine-3.23
qcow2> --vga-rom /usr/share/qemu/vgabios-virtio.bin`. Those options match
vmlab's record-lab machine. Firmware is vmlab's OVMF 2026.08 (CODE SHA-256
`7e34e3b8…`). The measurement is resume to the first non-black RFB frame.

| Binary | VARS on tmpfs | VARS on host disk | Disk plus fsync load |
| --- | --- | --- | --- |
| 1b75edf (before p2-reset) | 0.511–0.529 s | >30 s, 5 of 5 timed out | not measured |
| e740fb1 (main) | 0.513–0.550 s | 0.573–0.598 s | 0.640–0.724 s |
| 18732e6 (this branch) | — | 0.511–0.542 s | 0.552–0.598 s |
| QEMU 11.1.2, firmware paint | 1.09–1.40 s | | |

Findings:

- The failing vmlab e2e (`record.up-from-boot`, first lit at 0.9 s) ran a
  binary built before eccc0a1. vmlab keeps VARS on disk, and per-word
  fdatasync made paint late and variable there.
- QEMU passes vmlab's 0.7 s check only because it shows a host placeholder,
  "Display output is not active.", from t=0 while paused. Its firmware paint is
  roughly 2x later than ours. vmlab-hv shows black until the guest paints.
  - The open decision: either vmlab-hv draws a QEMU-style placeholder, or vmlab
    changes its check. A placeholder would weaken the S1 and KVM tests that
    treat "non-black" as "guest painted" (`wait_after_reset`, kvm_tests "VGA
    scanout stayed blank"); those would need to exclude it. Not implemented.
- The remaining ~0.5 s is firmware-bound:
  - PS/2 `KeyboardRead` takes a fixed 65 ms timeout.
  - PM-timer stalls.
  - About 9k NOR program writes.
  - Serving VARS reads from a read-only memslot removed about 33k exits but did
    not move first paint, so it was reverted.
- docs/reset.md's first-paint section has not been updated with these numbers.

## Part 1: Windows halt (not fixed)

### What was reproduced

Soaks used the acceptance tests with `VMLAB_HV_RESET_CYCLES` /
`VMLAB_HV_SNAPSHOT_CYCLES`. Host: two VMs at a time, with other sessions also
running VMs.

- **Reset cycles.** Release builds passed 20/20 in place, and the first reset
  passed in 12 single-reset runs.
- **A: silent hang after the setup reboot** (the process's first in-place
  reset). Seen once.
  - All 4 vCPUs are HLTed at one RIP, TPR 0, IRR and ISR empty.
  - STIMER0 on vCPU 0 is periodic (`0x3000b`, count 156250, SINT3, vector
    0xd1), the SIMP slot is empty, and no crash MSR report came.
  - It matches the earlier 0x5C `HalpTimerInitializeClock` reports: both gate
    tests do a setup reboot first.
- **B: freeze shortly after a snapshot load.** Seen in 2 of about 160 loads.
  - The agent handshakes, then exec times out.
  - vCPUs 0, 1 and 3 are at TPR 0xf0 on one RIP. vCPU 0 has a timer message in
    SINT3 and vector 0xd1 pending, blocked by TPR.
  - vCPU 2 is at TPR 0x20 with vector 0xa1 pending.
- **Slow loads.** About a third of loads take about 8 s before the agent
  answers. The 20 ms time trace from the last run is saved in this worktree at
  `.scratch/samp-snap-stderr.log` (not committed).
  - It shows vCPU 0 spinning with IF=0, TPR 0xf0, on the same RIP as B, for
    seconds after a load.
  - 0xd1 stays pending throughout.
  - B is likely the same mechanism, not recovering: the CPUs are frozen at
    HIGH_LEVEL waiting on another CPU, as in a freeze or IPI rendezvous.
- **D: one load returned a zero-filled file** instead of "before". Seen once,
  not investigated; it may be a disk or snapshot consistency issue.
- **Harness artifacts, not guest bugs, now fixed in the harness.**
  - OVMF sometimes resets twice while booting.
  - The stale `reset` event then made the harness think Windows had rebooted.
    That produced exit 1115 (shutdown in progress) and exec timeouts.

### What was ruled out

- The time base is consistent. A 20 ms sampler of KVM's reference counter, the
  guest TSC page and KVM_GET_CLOCK showed no jumps or skew across about 10
  resets or across loads.
- Reset restores all SynIC and STIMER MSRs to power-on values: sampled right
  after reset, they are 0.
- KVM's `stimer_cleanup` zeroes `exp_time`, so no stale expiry carries over.

### Current hypothesis

After a snapshot load, and possibly after the first reset, one vCPU stops
responding to an IPI or freeze request. The others then spin at HIGH_LEVEL with
IF=0, which also blocks the STIMER0 clock interrupt on vCPU 0.

Suspects:

- AVIC with KVM_SET_LAPIC and KVM_SET_VCPU_EVENTS ordering on restore.
- A pending IPI or `events.interrupt` lost or duplicated across restore.
- PV-EOI / VP-assist state, which KVM does not migrate.
- IOAPIC `irq_states`: line levels per source are not part of
  KVM_SET_IRQCHIP, so a level-triggered line can stay stuck.

AVIC is enabled on this host (`hyperv: [... "avic"]`).

### Exact next steps

1. Get the guest's own view.
   - Run a snapshot soak with `VMLAB_HV_GUEST_CRASH_MSRS=1`; `soak.sh` already
     sets it, and a bugcheck prints `vcpu N: guest crash [...]` in stderr.log.
   - Leave the frozen guest about 60 s before quitting, so that any
     CLOCK_WATCHDOG_TIMEOUT (0x101) is reported.
   - Extend `diagnostics::dump_timer_state` with RFLAGS and every vCPU's RIP.
2. A/B AVIC: rerun the snapshot soak with `"avic"` removed from the machine's
   `hyperv` list (acceptance.rs adds it when `host_apic_virtualization()`).
   - If the 8 s stalls and B disappear, investigate the AVIC restore order:
     set the LAPIC before the MSRs, and set events before the LAPIC.
   - Also try restoring each vCPU's LAPIC and events only after every vCPU's
     MSRs are set.
3. Check `kvm_vcpu_events` across save and restore for a lost injected or
   pending interrupt or NMI on the vCPU that does not freeze (vCPU 2 in B).
4. For A, rerun `first-reset.sh` loops for about 50 runs. They live in the
   session scratchpad and are reproduced below.

Loop script:

```sh
export VMLAB_HV_RESET_CYCLES=0 VMLAB_HV_GUEST_CRASH_MSRS=1 VMLAB_HV_ACCEPTANCE_KEEP=1 \
  VMLAB_HV_BINARY=$PWD/target/release/vmlab-hv VMLAB_HV_TEMPLATE_TIMEOUT=300 TMPDIR=$PWD/.scratch/vms
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- cargo test -p vmlab-hv \
  --bin vmlab-hv -- --exact acceptance::windows_10_agent_reboot_keeps_process_control_and_display --nocapture
```

For snapshot soaks, use `acceptance::windows_10_ovmf_agent_and_snapshot` with
`VMLAB_HV_SNAPSHOT_CYCLES=20`. Optionally add `VMLAB_HV_DIAG_TIME_SAMPLE_MS=20`,
which writes `time trace` lines to the VM's stderr.log for 12 s after each load.

5. After the fix, run the proof: 20 reset cycles and 20 snapshot loads passing,
   then prune the WIP diagnostics. Keep the harness cycle variables, the
   event-drain fix and the 1115 acceptance.

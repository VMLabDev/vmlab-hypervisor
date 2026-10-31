# Phase-2 performance gate (HV-D28)

**Verdict: NOT passed. Eight of ten bars meet D28; G1 and G3 fail.**
The VM default must remain unchanged.

## Final full run

These are medians of three repeats per configuration on 2026-10-31, from
`bench/history/x86_64.jsonl`. Both backends ran the full Ubuntu suite,
Windows boot suite, and guest-to-guest suite through `just bench-run-kvm`
and `just bench-run-g2g`. `just bench-report x86_64` reports the same
verdicts. The bars remain boot <= QEMU and throughput >= 80% of QEMU.

| Bar | Guest | Workload | QEMU | vmlab-hv | hv/qemu | Bar | Verdict |
|---|---|---|---|---|---|---|---|
| G1 | ubuntu-26.04 | boot to agent ready (s) | 6.73 | 12.23 | 1.817 | <= 1.000 | FAILS |
| G2 | windows-10 | boot to agent ready (s) | 171.86 | 119.78 | 0.697 | <= 1.000 | meets |
| G3 | ubuntu-26.04 | sequential read (MiB/s) | 19541.84 | 9560.08 | 0.489 | >= 0.800 | FAILS |
| G4 | ubuntu-26.04 | sequential write (MiB/s) | 495.25 | 3654.79 | 7.380 | >= 0.800 | meets |
| G5 | ubuntu-26.04 | random read 4 KiB (IOPS) | 50386.00 | 62425.70 | 1.239 | >= 0.800 | meets |
| G6 | ubuntu-26.04 | random write 4 KiB (IOPS) | 10475.10 | 22621.30 | 2.160 | >= 0.800 | meets |
| G7 | ubuntu-26.04 | UDP TX (Mbit/s) | 36398.85 | 31162.01 | 0.856 | >= 0.800 | meets |
| G8 | ubuntu-26.04 | UDP RX (Mbit/s) | 86691.53 | 75662.23 | 0.873 | >= 0.800 | meets |
| G9 | ubuntu-26.04 | guest TCP A to B (Mbit/s) | 26312.27 | 21313.36 | 0.810 | >= 0.800 | meets |
| G10 | ubuntu-26.04 | guest TCP B to A (Mbit/s) | 26602.37 | 21445.48 | 0.806 | >= 0.800 | meets |

Idle CPU has no D28 bar: QEMU 1.00% and vmlab-hv 2.60% of one core.

Windows and writes still vary widely on a quiet host. Windows samples are
108.44, 188.60, 171.86 seconds for QEMU and 188.91, 103.39, 119.78 seconds
for vmlab-hv. Sequential writes range from 328.32 to 1736.24 MiB/s for
QEMU and 1684.48 to 3947.11 MiB/s for vmlab-hv. All samples are retained.

The guest-to-guest margins are small. G9 passes at 0.810x and G10 at
0.806x; one G10 sample is below 80% of QEMU's median. These are median
verdicts, not a claim that every sample passes.

### Host and reproducibility

The measured tree is `251187d41` on `wp/p2-perf`. The recorded dirty
marker comes from appended benchmark history; Rust sources did not change
during the final run. QEMU is 11.1.2 and vmlab-hv reports 0.1.0.

The host has 32 logical CPUs and the performance governor. Single VMs use
pin 16-23; the guest pair uses 16-19 and 20-23. Templates are Ubuntu
26.04.3 and Windows 19045.2006.9, standard variant. Ubuntu uses 2 vCPUs
and 4 GiB; Windows uses 4 vCPUs and 8 GiB. The Ubuntu NIC MTU is 65520.

Other project sessions deferred VM boots and heavy work for the final
window. Before each configuration, both existing test-slot locks were
held and no project VM was running. Each launch also checked the
host-wide two-VM limit. The guest pair used exactly two slots.
Every VM ran under `MemoryMax=24G` and `MemorySwapMax=0`. The owner's
Android emulator was excluded from the VM count and left untouched.

The table records one-minute load averages at each repeat's start and end.
The history retains all three load averages at both endpoints. The higher
guest-pair load includes the two measured VMs.

| Run | Configuration | Repeat 1 | Repeat 2 | Repeat 3 |
|---|---|---|---|---|
| E1 | qemu Ubuntu | 1.68 to 0.68 | 0.68 to 0.56 | 0.56 to 0.55 |
| E2 | vmlab-hv Ubuntu | 0.59 to 0.57 | 0.57 to 0.57 | 0.57 to 0.71 |
| E3 | qemu Windows | 0.71 to 1.59 | 1.59 to 1.06 | 1.06 to 1.08 |
| E4 | vmlab-hv Windows | 0.99 to 1.31 | 1.31 to 1.92 | 1.92 to 1.69 |
| E5 | qemu guest pair | 1.69 to 2.82 | 2.82 to 3.36 | 3.36 to 3.03 |
| E6 | vmlab-hv guest pair | 3.03 to 2.58 | 2.58 to 2.50 | 2.50 to 2.35 |

### QEMU cache mode

The benchmark uses vmlab's blockdev argv without a cache override.
Black-box QMP `query-block` on this argv reports
`writeback=true, direct=false, no-flush=false`. Thus the host page cache
is enabled and guest flush requests are honored. The disk crate retains
the same buffered-I/O and flush semantics. No unsafe cache mode was used.

## What moved the bars

The following before/after figures are single-repeat diagnostics on a
shared host, not the final gate evidence above.

- C1. `4036c5c` skips unchanged chipset IRQ source levels before scanning
  the wired-OR inputs. Ubuntu boot moved from 16.22 to 12.19 seconds.
  The corrected block profile's `Source::set_level` share moved from
  40.13% to 10.56%. Shared-line delivery semantics are unchanged.
- C2. `8047d0d` advertises 254 data segments, indirect descriptors,
  and a 256-entry block queue. Installed QEMU device help confirms the
  same queue default and indirect-descriptor support. Sequential reads
  moved from 275.14 to 9846.36 MiB/s. Device and transport tests cover
  scatter-gather data, status, queue validation, snapshot, and reset.
- C3. `0b75d36` parses network payloads with slice copies, removes
  pending-frame clones, and avoids redundant TX copies. UDP RX moved
  from 11574.96 to 74564.50 Mbit/s. A new fragmented/jumbo/empty-frame
  test also reproduces and fixes the previous empty-frame panic.
- C4. `c2f6977` coalesces adjacent qcow2 mappings and uses positional
  I/O; `d47c0d9` reuses block request scratch. Neither demonstrated an
  isolated sequential-read throughput gain. The final read profile
  explains why further disk mapping work alone cannot close G3.

The first current-branch Windows diagnostic was already 119.42 seconds,
rather than the stale 307.12-second result. Reset-in-place predates this
work; it is not credited to these changes. Metadata caches also already
existed. No refcount batching or durability relaxation was added to
obtain the random-write result.

The benchmark client now allocates distinct channel IDs across execs and
reconnects. This fixes a reproduced QEMU run failure after readiness.
Admission failures preserve their original error and reap peers and
partially started pairs. Guest-to-guest labels now identify the actual
sender and receiver; historical records retain their old, swapped keys.
New records for both backends use the corrected labels.

## Remaining failures and profiles

The sampler is committed as `bench/ipsample.c`. It resolves ELF load
segments and symbol sizes, supports dynamic DSO symbols, requests 1 kHz
sampling, and requests kernel/guest exclusion. PIE and non-PIE
symbol-resolution tests pass. Unresolved samples remain visible and are
not assigned to a nearby symbol.

The old sampler used mapping start minus file offset as the load bias.
This executable's text segment differs by 4096 bytes, which displaced
symbol attribution. In particular, the old `fire_config` diagnosis was
wrong. Source inspection confirms that config notifications occur on link
transitions, not per packet. Notification semantics were left intact.

### G1: Ubuntu boot

The final medians are 12.23 seconds versus 6.73 seconds, a 5.50-second
gap. Kernel serial output in the diagnostic boots reports a kvm-clock
startup offset near 6.0 seconds under vmlab-hv and 1.4 seconds under
QEMU. Much of the remaining gap precedes the kernel's normal boot log.

`bench/profiles/phase2-20261031-boot.txt` retains 1825 samples from an
approximately six-second window after control-socket resume. Of those,
496 are unresolved. The largest resolved leaves are `run_vcpu` at
12.60%, `should_run` at 5.10%, and chipset clock advancement at 4.05%.
Condition-variable waiting, notification, and lock-bucket operations are
also visible. This is distributed startup/exit and synchronization cost;
the leaf profile does not establish one sufficient, safe fix.

G1 remains unfinished. Removing mapping quiescence or timer
synchronization without replacement would risk reset, SMM, and snapshot
correctness. No such shortcut was made.

### G3: sequential block reads

The final median is 9560.08 MiB/s. The required rate is 15633.47 MiB/s,
80% of QEMU's 19541.84 MiB/s.

`bench/profiles/phase2-20261031-seqread.txt` retains 5584 samples while
repeating the same 8 GiB direct guest read for a long enough profile.
This diagnostic does not append gate results. There are 2598 unresolved
samples. Two guest-memory slice iterator functions account for 13.14%
and 10.01%; descriptor-chain iteration accounts for 3.99%, guest-memory
`read_slice` for 3.98%, and writer construction for 2.63%.
Qcow2 `read_bytes` accounts for 0.50%.

The remaining read path spends substantially more resolved samples on
descriptor/address handling than on qcow2 reads. G3 needs further work
on that request path. Coalescing mappings and reusing scratch did not
meet the bar. Guest-memory validation and snapshot tracking were not
bypassed to raise the number.

These are leaf-IP histograms, not call counts or wall-time breakdowns.
Unresolved code limits the attribution. No claim assigns those samples
to allocation, copying, or storage I/O without evidence.

## Correctness and verification

`just ci::check` passed with `RUST_TEST_THREADS=1`, inside the 24 GiB
scope. This includes firmware acceptance, S1 snapshot/reset and network
restore checks, and snapshot during fsync writes. All 23 active S2 tests
passed against qemu-img; the normally ignored 1 GiB S2 integrity and
throughput test was also run explicitly and passed. All 333 device unit
tests and all 67 benchmark tests passed.

Guest flushes still reach `DiskImage::flush` and durable host sync.
Metadata cache, refcount flush, and snapshot ordering were not weakened.
No shared Rust trait or control protocol type changed. No scoped
`todo!()` or `unimplemented!()` remains. No host PCI/USB binding or reset
operation was performed. Measurement VMs, peers, and profiler processes
were reaped. No merge, rebase, default flip, or push was performed.

Work stops with G1 and G3 measured and profiled, as the task's failed-bar
condition requires. D28 remains the gate for any future default change.

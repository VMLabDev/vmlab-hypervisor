# Phase-2 performance gate (HV-D28)

**Verdict: NOT passed. The round-5 full-suite window met six of ten core bars.
The round-6 focused window now passes G4, G7, G9, and G10; other bars were
not rerun.** The VM default must remain unchanged.

## Round 6 owner decisions (2026-11-02)

The owner accepts G1 at parity: Ubuntu boot at 6.61 seconds versus QEMU's
6.59 seconds is within noise. Round 6 does not pursue further boot changes.
The round-5 table below retains its strict numerical verdict for traceability.

UDP TX and sequential-write regressions must be investigated before the
network optimizations. If TCP B to A does not recover with UDP TX, the owner
accepts its round-5 0.7974x result as noise-level. This exception does not
change the UDP TX or sequential-write bars. The default remains unchanged
until the remaining work and the authorized quiet-window comparison finish.

## Round 6 development checkpoint (2026-11-02)

This is shared-host evidence, not a quiet-window verdict. G4 remains
unresolved. No default, workload size, flush policy, or D28 threshold changed.
The owner scheduled the full merge gate after the focused quiet window.

### Historical commit comparisons

The quiet-window ratio changes do not establish raw HV slowdowns. Between
rounds 4 and 5, HV UDP TX rises from 23.38 to 24.50 Gbit/s and sequential
write rises from 3843.93 to 3986.11 MiB/s. QEMU rises more: 30.94 to 38.83
Gbit/s and 4793.65 to 5337.45 MiB/s. The lower ratios remain failed bars;
they do not, by themselves, identify a regression caused by an HV commit.

Frozen release binaries cover round 4, queue-wake coalescing, firmware
configuration I/O, mergeable receive buffers, and round 5. Each workload
runs alone in three rotated cohorts. UDP uses three 12-second sends per
fresh overlay. Writes use seven fresh 512 MiB files per overlay, each with
the unchanged O_DIRECT writes and timed final fdatasync.

The write column below uses only the first write in each fresh overlay.
Later writes have warmer mappings and must not conceal the cold-write gap.
Every sample, including the later writes, is retained in the profile.

| Revision | UDP TX median (Gbit/s) | First sequential write median (MiB/s) |
|---|---:|---:|
| round 4, `41e53e4` | 24.21 | 3776.81 |
| wake coalescing, `e9333a9` | 23.89 | - |
| firmware I/O, `2bec7a2` | - | 3650.99 |
| mergeable RX, `a3ddb7b` | 24.25 | 3790.71 |
| round 5, `95ea79d` | 28.01 | 3571.90 |

UDP's round-5 regression does not reproduce in this cohort. Cold writes
remain 5.4% below round 4. The parent/child samples around block interrupt
suppression overlap and reverse direction in one cohort. These runs do not
establish either suspect as the cause. No speculative block or firmware
revert was made. This is an unresolved performance finding, not a claimed
fix or an owner-approved exception for G4.

### Retained network changes

N1 receive DMA reads the frame length separately and readv fills validated
guest spans. A short body is captured in the existing owned decoder state.
The header and used entries are published only after the complete packet.
Three rotated RX cohorts improve the median from 79.03 to 104.43 Gbit/s.
The RX-only confirmation measures 77.03 to 98.81 Gbit/s. The per-packet
direct-TX candidate is rejected: 24.42 versus 31.35 Gbit/s.

N2 batches transmit spans in writev, keeps descriptor ownership until the
unsent suffix is owned, and uses 512 KiB buffered RX chunks. UDP TX rises
from 23.22 to 25.20 Gbit/s across three rotated cohorts. A paired RX check
measures 74.86 to 100.78 Gbit/s. EVENT_IDX, POLLOUT continuation, ring wrap,
bounded staging, and legacy interrupt suppression remain covered by tests.

N5 requests 4 MiB SO_SNDBUF/SO_RCVBUF. Its UDP median falls from 26.66 to
25.05 Gbit/s, so the change is removed. Host socket sysctls are unchanged.

N6 offers CSUM and GUEST_CSUM. TX completes the seeded checksum during its
owned payload copy; the wire still carries bare Ethernet. Buffered RX
combines checksum calculation and placement. Direct RX validates its DMA
spans before setting DATA_VALID. Invalid checksums, unsupported formats,
and fragments requiring reassembly retain flags=0. No TSO, USO, or MQ is
offered. IPv4/IPv6, TCP/UDP, VLAN, odd splits, malformed metadata, and
short-write ownership have device coverage.

| N6 copy-path prototype | Before (Gbit/s) | After (Gbit/s) |
|---|---:|---:|
| UDP TX | 27.32 | 27.36 |
| TCP A to B | 25.17 | 34.71 |
| TCP B to A | 23.55 | 33.22 |

These are three interleaved cohorts. A later two-byte writev checksum-patch
candidate is also tested and rejected: UDP falls from 29.82 to 27.63 Gbit/s
in all three paired comparisons. The final source retains the copy path.
The prototype and final rebuild have distinct hashes; the final rebuild
has its own correctness and focused-runner smoke checks. Prototype timings
are not labelled as final-binary quiet measurements.

Linux ethtool reports generic TX checksum offload enabled under N6 and
fixed off before it. ethtool -S is captured but exposes no checksum counters.
Separate, untimed 32 MiB random TCP transfers match SHA-256 in both
directions. Guest probes then show TX checksum-helper calls disappearing.
A mixed QEMU/HV check also matches bytes and digests: QEMU records 348 TX
checksum-helper calls and 633 RX GRO checksum calls; N6 records zero for
both. Probes are installed only after timed transfers.

### Correctness and focused-window preparation

The final copy-based rebuild passes all 42 NIC tests and all 15 core S1
tests. S1 asserts both checksum features, validates nonzero on-wire UDP
checksums through the test switch, compares exact echo bytes, and snapshots
with UDP traffic active. The reset check passes four reboot methods, two
resets each, preserving the NIC stream and control connection. All 23 active
S2 tests and the explicit 1 GiB S2 check pass. Formatting, changed-crate
Clippy with warnings denied, and all 103 benchmark tests pass. Full
`just ci::check` remains for the owner's post-window merge gate.

`bench/phase2_window.py` prepares exactly G4, G7, G9, and G10 on both
backends, interleaved for three repeats. It performs no builds, boot bars,
read/random/RX/idle workloads, or CI. Preflight checks prepared assets;
the run reserves four admission locks and monitors VM ownership, memory
limits, and load. Only a complete cohort appends the eight target history
records. Its single-VM and guest-pair paths pass live smoke checks on both
backends without appending history. Allow 6-8 minutes for the authorized
window.

The raw diagnostics, source drivers, binary hashes, feature queries,
counter probes, negative control, rejected candidates, and verification
receipts are in `bench/profiles/phase2-round6-20261102.json`. Development
does not append benchmark history. Every VM uses the shared host count
check and a 24 GiB, zero-swap scope. Android and physical PCI/USB devices
are untouched. Temporary VM directories are removed after each run.

Runtime traits, the control protocol, switch framing, and VLNET v1 snapshot
layout are unchanged. The test-only SwitchStats adds udp_echoes. The default
is unchanged. The focused-window results below supersede the round-5 G4, G7,
G9, and G10 measurements; other bars retain their last full-suite verdicts.

### Round 6 authorized focused quiet window

The owner opened the window with other project work paused and no project VMs
running. The focused runner completed all three interleaved repeats on both
backends in 5m42s. It ran only sequential write (G4), UDP TX (G7), and TCP
guest-to-guest in both directions (G9/G10). No full suite, build, or CI ran.
The tested HV binary is the final copy-based N6 build, SHA-256
`93a214688c3289c59dd25be6893fc414e46b3883ebb0d54f7d538afc2322e818`.

| Bar | Workload | QEMU median | vmlab-hv median | hv/qemu | Required | Verdict |
|---|---|---:|---:|---:|---:|---|
| G4 | sequential write (MiB/s) | 4769.18 | 3859.40 | 0.8092 | >= 0.800 | PASS |
| G7 | UDP TX (Mbit/s) | 29361.04 | 25417.98 | 0.8657 | >= 0.800 | PASS |
| G9 | TCP A to B (Mbit/s) | 28519.17 | 32916.90 | 1.1542 | >= 0.800 | PASS |
| G10 | TCP B to A (Mbit/s) | 26809.29 | 33511.79 | 1.2500 | >= 0.800 | PASS |

Samples are retained in `bench/history/x86_64.jsonl`; the runner's complete
JSON and logs remain under `bench/run/`. Sender and receiver byte counts
matched for every TCP transfer. The three UDP sends completed without sender
errors. The host had 32 logical CPUs, performance governor, 24 GiB VM memory
caps with swap disabled, and the unchanged Ubuntu 26.04.3 template. Each load
entry is the host's 1-, 5-, and 15-minute load average at run start and end.

| Suite | Repeat | Backend | Sequential write | UDP TX | TCP A to B | TCP B to A | Load start -> end |
|---|---:|---|---:|---:|---:|---:|---|
| single | 1 | QEMU | 4769.18 | 40983.75 | - | - | 1.48, 3.38, 7.01 -> 1.72, 3.28, 6.88 |
| single | 1 | vmlab-hv | 3859.40 | 34597.70 | - | - | 1.72, 3.28, 6.88 -> 1.74, 3.18, 6.77 |
| single | 2 | vmlab-hv | 3683.14 | 25417.98 | - | - | 1.74, 3.18, 6.77 -> 1.46, 3.03, 6.64 |
| single | 2 | QEMU | 4731.02 | 29361.04 | - | - | 1.46, 3.03, 6.64 -> 1.41, 2.91, 6.53 |
| single | 3 | QEMU | 4865.58 | 28812.22 | - | - | 1.41, 2.91, 6.53 -> 1.58, 2.85, 6.43 |
| single | 3 | vmlab-hv | 3889.63 | 25042.77 | - | - | 1.58, 2.85, 6.43 -> 1.70, 2.80, 6.33 |
| g2g | 1 | QEMU | - | - | 31138.51 | 26809.29 | 1.70, 2.80, 6.33 -> 2.54, 2.88, 6.23 |
| g2g | 1 | vmlab-hv | - | - | 31204.22 | 32486.28 | 2.54, 2.88, 6.23 -> 3.31, 3.04, 6.14 |
| g2g | 2 | vmlab-hv | - | - | 32916.90 | 33511.79 | 3.31, 3.04, 6.14 -> 3.33, 3.07, 6.04 |
| g2g | 2 | QEMU | - | - | 28519.17 | 27443.33 | 3.33, 3.07, 6.04 -> 3.46, 3.11, 5.94 |
| g2g | 3 | QEMU | - | - | 26436.00 | 26630.34 | 3.46, 3.11, 5.94 -> 2.95, 3.03, 5.79 |
| g2g | 3 | vmlab-hv | - | - | 33636.92 | 35103.53 | 2.95, 3.03, 5.79 -> 3.08, 3.06, 5.70 |

The 15-minute load average declined from 7.01 to 5.70 over the window.
Each backend's result remains the median of its three interleaved samples;
no repeat was discarded. The complete result JSON also records template and
binary hashes, backend versions, workload counts, and per-second VM/load
monitoring.

## Round 5 authorized final quiet bench window (2026-11-02)

The final measurement passes six of ten core bars. G1, G4, G7, and G10 fail.
The default remains unchanged. This window supersedes the round-4 verdict
and the development sections' pending-window statements below.

The owner authorized the window with the other project SIGSTOPped, every
other session idle, no project VMs, and 68 GiB free in /tmp. Both backends
ran from source `a1eb3ec`. The tested HV binary's SHA-256 is
`eae67f98cdf14d89a3d19b86a02b808e07fff4dc9dcaa5c573021d641d8d388f`.

Both backends ran the actual `just bench-run-kvm` recipes for Ubuntu
26.04 and Windows 10, plus `just bench-run-g2g` for Ubuntu. Each has three
fresh-overlay repeats, alternating QEMU then vmlab-hv per repeat. Recipe
dependencies finish before either backend starts its first VM. Runtime
source, firmware, workload sizes, flushes, readiness, repeat counts, and
D28 thresholds stay unchanged.

All six benchmark commands and `just bench-report x86_64` exit 0. The
report prints the failed bars; its exit status does not enforce D28.
All 22 new aggregate records and their raw samples are appended to
`bench/history/x86_64.jsonl`. The earlier supplementary Server 2025
result is historical and is not included in these ten bars.

### Full bar table

Values are medians. Boot requires hv/qemu <= 1.0000. Throughput requires
hv/qemu >= 0.8000. Neither rounding nor spread changes the verdict.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Result |
|---|---|---:|---:|---:|---|
| G1 | Ubuntu boot (s) | 6.59 | 6.61 | 1.0030 | FAIL |
| G2 | Windows 10 boot (s) | 85.46 | 79.13 | 0.9259 | PASS |
| G3 | Sequential read (MiB/s) | 17197.94 | 14990.46 | 0.8716 | PASS |
| G4 | Sequential write (MiB/s) | 5337.45 | 3986.11 | 0.7468 | FAIL |
| G5 | Random read 4 KiB (iops) | 49844.60 | 68271.20 | 1.3697 | PASS |
| G6 | Random write 4 KiB (iops) | 30947.90 | 36018.80 | 1.1639 | PASS |
| G7 | UDP TX (Mbit/s) | 38828.77 | 24502.28 | 0.6310 | FAIL |
| G8 | UDP RX (Mbit/s) | 86589.76 | 82292.74 | 0.9504 | PASS |
| G9 | TCP A to B (Mbit/s) | 26395.45 | 21429.40 | 0.8119 | PASS |
| G10 | TCP B to A (Mbit/s) | 26695.35 | 21286.79 | 0.7974 | FAIL |

G2 and G9 now pass. G1 remains 0.02 seconds slower. G10 is below the floor
at 0.7974, while G4 and G7 have larger deficits. These measurements do not
identify the cause of the remaining gaps. Idle host CPU has no D28 bar:
QEMU measures 1.03% and vmlab-hv 2.57% of one core.

### Windows readiness and TCP checks

Windows 10 samples are 86.16, 85.46, and 85.30 seconds for QEMU, and
81.17, 79.13, and 78.84 for vmlab-hv. The unchanged marker, OOBE, AppX,
and guest-reboot readiness sequence completes. Existing boot-ID probes
change from `0x9` to `0xa` across all six requested reboots. Capturing
those return values adds no guest commands.

All 12 TCP transfers have matching sender and receiver byte counts.
Payload hashes were not collected. All 12 single-VM repeats exit cleanly;
the g2g teardown also stops every owned VM and peer.

### Load averages per run

Each triple contains the one-, five-, and fifteen-minute load averages.
Start observations are taken after the interleaving wait. Rolling averages
include preceding benchmark work. The original history environment's
pre-wait observations are retained separately.

| Run | Start load [1, 5, 15 min] | End load [1, 5, 15 min] |
|---|---|---|
| ubuntu qemu r1 | [0.34, 2.24, 3.79] | [0.55, 1.85, 3.49] |
| ubuntu vmlab-hv r1 | [0.55, 1.85, 3.49] | [0.52, 1.50, 3.19] |
| ubuntu qemu r2 | [0.52, 1.50, 3.19] | [0.57, 1.28, 2.94] |
| ubuntu vmlab-hv r2 | [0.57, 1.28, 2.94] | [0.48, 1.11, 2.70] |
| ubuntu qemu r3 | [0.48, 1.11, 2.70] | [0.62, 1.06, 2.53] |
| ubuntu vmlab-hv r3 | [0.62, 1.06, 2.53] | [0.60, 0.97, 2.34] |
| windows10 qemu r1 | [0.60, 0.97, 2.34] | [1.34, 1.15, 2.27] |
| windows10 vmlab-hv r1 | [1.34, 1.15, 2.27] | [1.85, 1.36, 2.25] |
| windows10 qemu r2 | [1.85, 1.36, 2.25] | [1.43, 1.36, 2.17] |
| windows10 vmlab-hv r2 | [1.43, 1.36, 2.17] | [2.05, 1.57, 2.17] |
| windows10 qemu r3 | [2.05, 1.57, 2.17] | [2.48, 1.79, 2.19] |
| windows10 vmlab-hv r3 | [2.48, 1.79, 2.19] | [1.75, 1.72, 2.13] |
| g2g qemu r1 | [1.75, 1.72, 2.13] | [2.36, 1.88, 2.17] |
| g2g vmlab-hv r1 | [2.36, 1.88, 2.17] | [3.05, 2.11, 2.23] |
| g2g qemu r2 | [3.05, 2.11, 2.23] | [3.05, 2.23, 2.27] |
| g2g vmlab-hv r2 | [3.05, 2.23, 2.27] | [3.02, 2.33, 2.30] |
| g2g qemu r3 | [3.02, 2.33, 2.30] | [3.12, 2.45, 2.34] |
| g2g vmlab-hv r3 | [3.12, 2.45, 2.34] | [2.90, 2.48, 2.36] |

The one-second monitor records 1361 observations, a one-minute load range
of 0.31 to 3.41, at most two owned VMs, and no foreign project VM. Android
is excluded. Every observed VM has memory.max 25769803776 and
memory.swap.max 0. The coordinator holds all four shared admission locks;
the harness also checks capacity before every launch.

Single VMs use CPUs 16-23, and g2g uses 16-19 plus 20-23. The governor is
performance. The existing Ananicy policy remains unchanged: QEMU is moved
to nice 9 while vmlab-hv inherits nice -4. No profiler or exit-recorder
override is active.

### Evidence and cleanup

`bench/profiles/phase2-round5-final-20261102.json` contains the complete
bar table, all records and samples, load triples, monitor, command logs,
firmware and binary hashes, existing boot-ID probes, TCP counters, and
coordinator sources. The measurement job has a clean exit-0 receipt.
There are no discarded attempts or resource failures in this window.

Every owned temporary VM directory is removed as its run finishes, with
logs retained on disk. The minimum observed /tmp free space is
71086792704 bytes, about 66.2 GiB. The empty temporary root and the
measurement job are removed after completion. No project VM remains.

`just ci::check` is deliberately not run in this window, as instructed.
The orchestrator owns the subsequent integration gate. Later history
dirty flags reflect appended history, not runtime edits. The window changes
no default, threshold, shared interface, or snapshot format. Nothing is
merged or pushed.

## Round 5 development diagnostics (2026-11-02)

These are shared-host diagnostics on main `41e53e4`, not a new D28 verdict.
The round-4 quiet-window table remains the gate result. No history record,
threshold, firmware, guest workload, or default changed.

### Windows readiness across reset

The large Windows 10 gap includes a benchmark-client defect. Sysprep can
reboot after the specialize marker appears. The host's agent socket stays
open, but an outstanding command belongs to the previous guest boot. The
old client waits its full 90-second exec timeout before reconnecting.
Windows' own OOBE log finishes much earlier than the host's next probe.

An unchanged main binary reproduced a 156.74-second boot. A later QEMU
boot reproduced the same failure at 157.36 seconds, including an explicitly
recorded 90.09-second OOBE-probe timeout. Other boots of both binaries avoid
the race. This does not identify a VMM regression between `6cceef4` and main.

The runner now observes QMP or S1 reset events and cancels commands from the
previous agent generation. It reconnects and repeats the same probe. The
marker, OOBE, AppX, final guest reboot, and boot-ID checks remain. A marker
already observed is remembered if reset interrupts its deletion, so retrying
the deletion cannot lose the readiness proof.

Interleaved fresh-overlay boots used frozen VMM binaries. The old and fixed
columns are complete readiness times, not Hello-only measurements.

| Backend | Old readiness samples (s) | Fixed readiness samples (s) |
|---|---|---|
| QEMU | 157.36 | 92.92 |
| vmlab-hv main | 86.15, 85.67 | 83.83, 83.00 |

The old main samples in this cohort avoided the race; the earlier 156.74
capture remains separate. A controlled S1 reproduction on one Windows VM
scheduled a reboot during the same long command. The old client timed out
after 90.09 seconds; the fixed client cancelled after 3.36 seconds. S1 reset
events and BootId transitions `0xa -> 0xb -> 0xc` prove both guest reboots.
Six socket and readiness regressions pass, including both control protocols.
The reset-cancellation and marker-deletion regressions failed before their
corrections.

Server 2025 also completed the unchanged readiness sequence with the fixed
client: QEMU 76.98 seconds and vmlab-hv 71.18 seconds. Those samples shared
the host with other diagnostics and do not replace its round-4 result.

### Windows hardware and phase evidence

| ID | Hypothesis | Evidence |
|---|---|---|
| H1 | Coarse userspace HLT or synthetic timers | Both use KVM's irqchip, LAPIC, halt handling, and Hyper-V timers. Host halt polling is 200 us. PID-filtered HLT and scheduler traces record runnable-to-scheduled latency separately from the intended halt duration. |
| H2 | Missing Hyper-V enlightenments | Installed CPUID matches across QEMU, `6cceef4`, and main: feature leaf `0xaff/0x30/0/0x108`, recommendations `0xe24`, spinlock limit `0x1fff`. TLB flush, IPI, SynIC, timers, and AVIC are present. Neither reference launch enables stimer-direct. |
| H3 | Expensive reset or firmware rerun | Typical final reset-to-agent intervals are about eight seconds on both current backends. OVMF BDS, guest kernel-start events, marker, OOBE, AppX, and agent timestamps are retained for each captured boot. |
| H5 | Trapped framebuffer writes | Both register an 8 MiB dirty-logged mapping at `0x80000000`. Complete MMIO-page histograms in the second observer cohort contain no exits in that mapping. Legacy VGA-aperture exits are separate. |

Windows 10 and Server 2025 use the same configured hardware: four vCPUs,
8192 MiB, installed OVMF, enlightenments, virtio-blk, virtio-vga, tablet,
and no NIC. Their images and provisioning work differ.

Guest kernel-start events are calibrated against a post-boot guest/host
clock observation, with the RPC interval retained as uncertainty. These
bound boot-manager and kernel/service startup phases; they do not measure
kernel-only CPU time. The initial all-exit main trace overran by 166526
records and is marked incomplete. The replacement HLT-filtered captures
have no overruns. Raw captures are retained as lossless gzip files.

One complete 157.36-second QEMU capture contains 483609 HLT exits; a complete
83.00-second fixed-main capture contains 282281. Their HLT-blocked vCPU
runnable-to-scheduled p95 is 3 us on both backends, with p99 of 12 and 8 us.
The different capture durations prevent interpreting those totals as an
exit-rate improvement. This scheduler interval is not timer-expiration to
the first resumed guest instruction.

`bench/profiles/phase2-round5-windows-20261102.json` contains the complete
samples, failed probes, guest logs, phase markers, installed CPUID and
memory-registration evidence, trace summaries, losses, compressed-capture
hashes, and diagnostic sources. Credential-related guest log lines are
omitted from the committed copy; original local diagnostic logs remain.

### Network queue service and G1

H4, the offload hypothesis, was checked inside the guests. CSUM,
GUEST_CSUM, HOST_TSO4/6, and GUEST_TSO4/6 are disabled on both backends with
the stream-socket attachment. QEMU removes those capabilities when its
peer has no virtio header. MRG_RXBUF is offered by QEMU and remains absent
from vmlab-hv; this pre-existing difference was not identified as the TX
cause. Negotiated bits and complete ethtool output are retained.

The NIC now coalesces worker wakes, rearms before processing work, and
services both queues without enqueuing a command for each kick. This
removes duplicate queue walks. Lifecycle barriers remain ordered, and
backpressure, POLLOUT continuation, EVENT_IDX, and snapshot bytes remain
unchanged. Coalescing socket writes alone did not improve UDP and was not
retained as a standalone fix.

The following are medians of three fresh-overlay boots per configuration.
UDP uses the median of three unchanged 12-second sends within each boot.
Each TCP direction runs once per fresh pair. The cohorts alternate QEMU,
baseline, and candidate with rotating order.

| Workload | QEMU (Mbit/s) | Baseline (Mbit/s) | Coalesced queue service (Mbit/s) |
|---|---:|---:|---:|
| UDP TX | 28113.15 | 22724.03 | 25099.14 |
| TCP A to B | 24552.06 | 13616.11 | 17382.59 |
| TCP B to A | 29395.08 | 16006.86 | 18886.95 |

The distributions overlap. UDP VM-process CPU seconds per GiB increased;
no CPU-efficiency gain is claimed. TCP remains below 80% of QEMU in this
cohort, and G9 is not closed. All 18 TCP transfers have matching sender and
receiver byte counts. The new wake regression fails before the fix with
200 socket bytes instead of one. All 24 network unit tests and the S1 NIC
test pass, including all 900 echoes across save and restore.

UDP used CPUs 8-15 while Windows diagnostics used 16-23. TCP used 16-19 and
20-23 and overlapped the final UDP cohort and CI. Every VM used the shared
four-slot admission checks and a 24 GiB, zero-swap scope. The host's
Ananicy rule moves QEMU to nice 9 and best-effort I/O priority 7; vmlab-hv
inherits nice -4. This policy is recorded, not changed. These conditions
exclude a quiet-host verdict.

G1 remains unresolved. A startup profile pair reaches first kernel output
at 1.81 seconds under QEMU and 2.35 under baseline vmlab-hv. The requested
one-second leaf samples contain 329 and 626 samples, with 176 and 170
unresolved. Run-loop synchronization, mutex contention, and platform clock
service remain distributed costs. This does not establish one sufficient
safe change, and no boot-time fix is claimed. Memory-map quiescence and
timer synchronization were preserved.

`bench/profiles/phase2-round5-network-20261102.json` retains every sample,
CPU counter, feature query, failed setup attempt, discarded wake-only
candidate, startup profile, TCP byte check, and driver source. Rust shared
traits, the control protocol, and snapshot formats are unchanged. The
benchmark agent client adds an optional reset monitor; other callers keep
their existing behavior.

### Initial correctness gate

`just ci::check` passed after the fixes: formatting, clippy with warnings
denied, and 998 tests, including S1 and S2 coverage. Six slow, manual, or
live-session diagnostics remain ignored. All 89 benchmark-harness tests passed
separately. Physical USB and VFIO opt-ins were unset; real-device paths
were not exercised. Owned temporary VM overlays were removed; diagnostic
logs and compressed traces remain available.

The initial pass leaves G1 and G9 open. G2 and G7 need confirmation in the
next authorized quiet window. The default remains unchanged. READY FOR BENCH WINDOW.

### Owner-requested G1/G9 follow-up

The owner requested another focused pass before the quiet window. The
following measurements remain shared-host diagnostics. The strict D28
verdict and the default are unchanged.

F1, ECAM service during SeaBIOS and GRUB boot, accounts for a large part
of the pre-kernel gap. vmlab-hv completes firmware/ROM loading, ACPI
construction, and vCPU creation within about 0.1 seconds. QEMU also reaches
its first KVM exit in about 0.1 seconds. Captured fw_cfg DMA service is
about 0.1 ms in vmlab-hv. Neither accounts for the original gap. The disk's
ECAM page receives about 44771 accesses. Its observed userspace service
interval totals 0.516 seconds before the first correction and 0.100 after.
Those intervals include scheduling and observer overhead, not only device
CPU time.

ECAM reads now call each configuration dword once, including unaligned
reads, instead of repeating a side-effecting read for every returned byte.
Writes through virtio's indirect configuration window no longer stop every
other vCPU, recheck VGA mappings, or resynchronize platform timers and PIRQs for
each access. The exception covers only the captured net, block, and
console identities and bytes 0x88 through 0x97. All other configuration
writes retain the memory-update barrier. PAM, SMRAM, VGA, A20, and VFIO
mapping rules are preserved. Both new regressions failed before the fix.

The initial interleaved candidate reduces the median launch-to-first-kernel
interval from 2.251 to 1.851 seconds; QEMU measures 1.614. Full readiness
changes only from 8.16 to 8.12 seconds in that cohort. Unmapped ROM reads
remain visible in the observer. QEMU registers ioeventfds, but the large
firmware disk cost appears in configuration-window accesses; it is not a
measurement of Linux's steady-state BAR notification path.

F2, console input before guest open, explains the remaining large readiness
penalty. vmlab-hv previously consumed host bytes when receive buffers
existed, even if the guest had not opened the named port. The kernel can
discard that early Hello. QEMU's `virtio_serial_guest_ready` gates input on
guest-open state. The device now applies that backpressure and delivers
staged data when the guest opens the port. The socket regression fails
before the correction. No benchmark timing or readiness rule changed.

| Console cohort | Full readiness median (s) | Samples (s) |
|---|---:|---|
| QEMU | 6.80 | 6.84, 6.80, 6.69 |
| Before guest-open correction | 8.13 | 8.13, 8.12, 8.13 |
| After guest-open correction | 6.90 | 6.92, 6.90, 6.80 |

F3, mergeable network receive buffers, closes the remaining observed
feature difference relevant to RX allocation. CSUM, GUEST_CSUM,
HOST_TSO4/6, and GUEST_TSO4/6 remain off on both stream backends. MRG_RXBUF
is now offered and negotiated, as in QEMU. One header describes each
packet; all receive chains are copied before one used-index publication.
If capacity is insufficient, the packet and original available position
remain pending, while the refill event targets the end of the inspected
buffers. This avoids repeatedly retrying the same insufficient buffers.

Tests cover split headers, multi-buffer packets, no DMA or partial
publication before refill, short and inaccessible buffers, ring wrap,
and interrupt events crossing the whole batch. The first two mergeable
RX regressions failed before implementation. The S1 snapshot test now
asserts MRG_RXBUF and EVENT_IDX negotiation and passes all 900 normal and
60 unfragmented jumbo echoes across save and restore.

| Three-pair TCP cohort | QEMU (Mbit/s) | Before MRG_RXBUF | After MRG_RXBUF | After/QEMU |
|---|---:|---:|---:|---:|
| A to B | 26029.85 | 20914.20 | 22752.00 | 0.8741 |
| B to A | 26578.61 | 22166.90 | 23007.16 | 0.8656 |

All 18 cohort transfers and four later combined-build transfers have
matching sender/receiver byte counts. Payload hashes were not collected.
The quiet-window G9 result remains unmeasured. TCP CPU efficiency was not
measured in this cohort.

F4, legacy block interrupt suppression, removes unnecessary completion
interrupts while firmware polls the ring. Without EVENT_IDX, the block
device previously ignored the driver's NO_INTERRUPT flag. It now respects
that flag; EVENT_IDX still takes precedence when negotiated. The polling
regression fails before the fix. The observer records 22290 pre-kernel
KVM_IRQ_LINE calls before this change and six after; QEMU records zero.

| Final interleaved boot cohort | First kernel median (s) | Full readiness median (s) | Full readiness samples (s) |
|---|---:|---:|---|
| QEMU | 1.636 | 6.75 | 6.76, 6.75, 6.73 |
| Before block interrupt correction | 1.871 | 6.81 | 6.80, 6.81, 6.89 |
| All corrections | 1.792 | 6.76 | 6.72, 6.78, 6.76 |

The final full-readiness ratio is 1.0015. G1 is still above the strict bar
in this shared-host cohort; neither rounding nor a noise allowance makes
it pass. First-kernel output is sampled every 5 ms. Host setup, firmware
execution, first kernel output, and full readiness remain separate metrics.
The benchmark clock includes the systemd scope launch. The observer also
timestamps VMM exec through its loader constructor, vCPU creation, and
the first KVM exits.

Boot diagnostics use CPUs 8-15; TCP uses 16-19 and 20-23. Some cohorts
overlap on those separate physical-core groups, with brief builds and
unit checks also recorded. Full CI starts after every timed cohort ends.
The existing Ananicy priority difference remains unchanged. Each VM uses
the shared four-slot admission checks and its own 24 GiB, zero-swap scope.

`bench/profiles/phase2-round5-followup-20261102.json` retains all cohorts,
feature queries, observer records, source drivers, binary hashes, failures,
and validation receipts. The first observer lacked mmap64 interception;
two early captures stopped on UART setup rather than kernel text. These
captures are marked unsuitable for port-cost conclusions. A wrapped Herdr
exit marker also prevented its build-status parser from reading one
successful build; the displayed EXIT=0 and build log are retained.

No Rust trait, control protocol, switch framing, or snapshot layout changed.
The guest-visible change is MRG_RXBUF support, matching the reference.
The runtime changes are ready for the authorized quiet-window comparison.

The final `just ci::check` passes formatting, clippy with warnings denied,
and 1007 Rust tests, including S1 and S2. Six slow, manual, or live-session
diagnostics remain ignored. All 89 benchmark tests and the final release
build pass. Physical USB and VFIO opt-ins remain unset; no real devices
were exercised. Owned temporary benchmark VM directories were removed.

READY FOR BENCH WINDOW. G1 still needs a strict quiet-host result; G9's
shared-host improvement does not change the existing gate verdict.

## Round 4 authorized quiet bench window (2026-11-02)

CI was deliberately not run in this window, as instructed by the owner.
The orchestrator reserves the combined merge gate. The benchmark harness's
83 unit tests passed before timed measurements began.

The owner reported other projects SIGSTOPped, no project VMs running, and
every other session idle. Measurements use branch source `55301ef`, which
contains main `a8fdf05` through merge `c551327` and the test-only fixture fix
`19676ee`. The only new harness change registers Server 2025 as a boot-only
Windows guest. Runtime code, thresholds, readiness phases, and workload
sizes stayed fixed throughout the measurements.

Both backends ran the actual `just bench-run-kvm` recipes for Ubuntu,
Windows 10, and Server 2025, plus `just bench-run-g2g` for Ubuntu. Each
configuration has three fresh-overlay repeats. A scratch coordinator
alternated QEMU then vmlab-hv for each repeat, while preserving the original
recipes and three-sample aggregation. All eight benchmark commands exited
0. `just bench-report x86_64` also exited 0 and applied the unchanged D28
bounds. All 24 aggregate records and their samples are appended to
`bench/history/x86_64.jsonl`.

### Full bar table

Values are medians. Boot requires hv/qemu <= 1.0000; throughput requires
hv/qemu >= 0.8000. S25 is supplementary and does not replace Windows 10 G2.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Result |
|---|---|---:|---:|---:|---|
| G1 | Ubuntu boot (s) | 6.54 | 8.12 | 1.2416 | FAIL |
| G2 | Windows 10 boot (s) | 85.31 | 157.22 | 1.8429 | FAIL |
| G3 | sequential read (MiB/s) | 19671.17 | 16572.80 | 0.8425 | PASS |
| G4 | sequential write (MiB/s) | 4793.65 | 3843.93 | 0.8019 | PASS |
| G5 | random read 4 KiB (IOPS) | 50002.90 | 69281.10 | 1.3855 | PASS |
| G6 | random write 4 KiB (IOPS) | 30354.80 | 35546.80 | 1.1710 | PASS |
| G7 | UDP TX (Mbit/s) | 30936.27 | 23378.52 | 0.7557 | FAIL |
| G8 | UDP RX (Mbit/s) | 86595.75 | 91521.51 | 1.0569 | PASS |
| G9 | guest TCP A to B (Mbit/s) | 27822.22 | 21686.65 | 0.7795 | FAIL |
| G10 | guest TCP B to A (Mbit/s) | 26524.78 | 21818.77 | 0.8226 | PASS |
| S25 | Server 2025 boot (s) | 59.09 | 54.91 | 0.9293 | PASS |

G4 has little margin above the floor. No spread or noise allowance changes
the verdict. Idle host CPU, which has no D28 bar, was 0.95% for QEMU and
2.55% for vmlab-hv, measured as a percentage of one core.

### Server 2025 boot

The shipped template was `26100.1742.8`. Both backends used q35, 4 vCPUs,
8192 MiB, the same installed OVMF pair, Hyper-V enlightenments, virtio-blk,
virtio-vga, USB tablet, and no network interface. Readiness uses the existing
Windows specialize-marker, user-OOBE, AppX-settle, and in-guest reboot
sequence. It is not an early Hello-only measurement. The existing boot-ID
probes changed from 8 to 9 across all six requested reboots; retaining those
return values added no guest probes.

| Backend | Repeat 1 (s) | Repeat 2 (s) | Repeat 3 (s) | Median (s) |
|---|---:|---:|---:|---:|
| QEMU | 60.46 | 59.09 | 58.44 | 59.09 |
| vmlab-hv | 54.99 | 54.91 | 54.86 | 54.91 |

This profile does not reproduce the reported loaded-soak gap of 674 s
versus 418 s. That earlier observation is context, not a matched baseline
for this cohort. These measurements do not establish that any difference
from the soak was caused solely by host load or by a particular patch.

### Per-run host load and limits

The table records one-minute load at the actual start and end of each
repeat, after its interleaving wait. Full one-, five-, and fifteen-minute
values are retained per run in the evidence JSON. Rolling averages include
the preceding benchmark's own work.

| Configuration | Repeat 1 start->end | Repeat 2 start->end | Repeat 3 start->end |
|---|---|---|---|
| Ubuntu QEMU | 0.43->0.52 | 0.61->0.75 | 0.63->0.87 |
| Ubuntu vmlab-hv | 0.52->0.61 | 0.75->0.63 | 0.87->0.84 |
| Windows 10 QEMU | 0.20->1.76 | 1.41->1.27 | 1.80->2.51 |
| Windows 10 vmlab-hv | 1.76->1.41 | 1.27->1.80 | 2.51->1.55 |
| g2g QEMU | 1.55->2.36 | 2.20->2.50 | 2.95->3.34 |
| g2g vmlab-hv | 2.36->2.20 | 2.50->2.95 | 3.34->3.51 |
| Server 2025 QEMU | 3.51->3.00 | 2.79->2.22 | 2.40->2.35 |
| Server 2025 vmlab-hv | 3.00->2.79 | 2.22->2.40 | 2.35->2.19 |

The one-second monitor has 1949 observations for the successful cohorts,
with one-minute load ranging from 0.20 to 3.82. It observed at most two
project VMs, only for g2g, and no foreign project VM. Every observed VM had
memory.max 25769803776 and memory.swap.max 0. The coordinator reserved all
four shared admission locks; the original harness still checked actual
host capacity before every launch. Android was excluded from the count.
Single VMs used CPUs 16-23; pairs used 16-19 and 20-23. The governor was
performance. No debug recorder override or LD_PRELOAD profiler was active.
All 18 single-VM repeats exited cleanly, and every g2g transfer had exact
sender/receiver byte equality.

### Interrupted attempts and evidence

An initial recipe dependency rebuilt the HV binary for 3.62 s while the
first QEMU boot was starting. That attempt was interrupted before any
aggregate records and retained as invalid. The replacement cohort used a
prebuilt binary. Its SHA-256 is
`ec76f5838098ff2db24c51bf3c736337785bc1494047477e8d046aadac546764`.

After Ubuntu completed, the 94 GiB /tmp tmpfs filled during the first HV
Windows 10 repeat. The monitor hit ENOSPC and left a truncated final line.
That entire Windows comparison was invalidated before aggregate records.
The orchestrator removed 239 stale, unopened scratch directories from
finished sessions and restored 68 GiB free. Windows 10 was restarted with
three fresh repeats per backend, followed by g2g and Server 2025 on /tmp.
The complete Ubuntu cohort predates the resource failure and was retained.
The failed-attempt images and logs were not manually deleted or moved;
subsequent single-VM runs used the harness's normal overlay cleanup.

Herdr could not recover the final completed pane marker, and its waiter
raised a no-marker error. Individual command receipts show all eight
benchmarks and the report exited 0. The complete dataset, clean single-VM
exits, reboot checks, and memory caps were validated independently. This
runner issue is retained in the execution record, not presented as a clean
top-level receipt.

`bench/profiles/phase2-round4-final-20261102.json` contains the full bar
table, all samples and aggregate records, per-repeat load triples, monitor,
command logs, firmware and binary hashes, coordinator sources, and both
interrupted attempts. The g2g environment's pre-wait load is retained;
the coordinator observations supply its actual per-run start load. Dirty
flags in later history records reflect the appended history file, not
runtime source edits. All owned VMs, peers, and measurement jobs were
stopped after recording the results. Nothing was pushed or merged into
main. No threshold, default, flush requirement, or repeat count changed.

The earlier sections below are historical. This cohort supersedes their
gate verdicts and pending-window statements.

## Round 4 main integration before the bench window

The first post-merge CI attempt exited 101 on a pre-existing CPU-policy
fixture race. The test started a guest, immediately paused it, then changed
CPUID. KVM rejects changed CPUID after the first KVM_RUN. An isolated rerun
passed, but a 150 ms scheduling delay reproduced EINVAL at SET_CPUID2.
Commit `19676ee` starts that fixture paused, retains the delay, and asserts
the initial paused state. All original CPUID, MSR-log, TSC rollback, and
changed-frequency rejection assertions remain. Production code is unchanged
by that fix. All 202 active VMM unit tests passed before the full rerun.

The full `just ci::check` rerun exited 0 on `19676ee`: 987 tests passed,
zero failed, and six manual diagnostics remained ignored across 65 result
groups. Formatting and workspace clippy with `-D warnings` passed. The
suite ran with RUST_TEST_THREADS=1 and the 1800-second child-process
deadline, inside the 24 GiB, zero-swap scope. The ignored diagnostics were
not run explicitly. Physical USB/VFIO opt-ins remained unset.

The requested main `a8fdf05` was merged as `c551327`. Its one conflict was
in the vCPU post-KVM_RUN hooks. The resolution records opted-in exit
metadata first, then drains coalesced VGA writes before dispatch or
parking. VGA delivery remains on machine-events, while rendering stays
on the incoming display-frames worker. The halted-guest VGA S1 check also
passed with the exit recorder enabled. All 83 benchmark unit tests passed.

The release binary was rebuilt from `c551327`. The only source change
between that runtime and validated `19676ee` is the snapshot test fixture.
`bench/profiles/phase2-round4-main-a8fdf05-20261101.json` records both
commits, the release binary hash, complete validation logs, the initial
failure and its reproduction, and the kernel reference for the CPUID rule.

No post-merge D28 cohort has run. The timings below and the original
round-4 JSON describe the pre-merge runtime and remain historical evidence.
The display/audio-clock change and diagnostic hooks require fresh QEMU
and vmlab-hv measurements in the owner's authorized quiet bench window.
No threshold or default changed, and nothing was pushed.

## Round 4 development diagnostics

The round-4 changes start at `379c1ca`. These are shared-host diagnostics,
not final D28 measurements. The round-2 quiet-host verdict above remains
unchanged. Final validation waits for the owner's authorized bench window.
No threshold, readiness phase, workload, default, or firmware was changed.

### G7 historical comparison and notification progress

The first complete interleaved cohort ran QEMU, historical `8bcbf1b`,
baseline `379c1ca`, and two notification candidates three times each.
QEMU's UDP TX median was 30553.57 Mbit/s. Historical and baseline medians
were 18557.61 and 15294.84 Mbit/s, with overlapping ranges. Source review
identified added per-exit locks, but the measurements assign no regression
commit. Host load exceeded 230 during this investigation.

The initial TX-suppression and EVENT_IDX candidates timed out. One saved
guest remained responsive with its sender blocked in `sock_alloc_send_pskb`.
Source review found a lost continuation: an independent wakeup flush could
empty staging after TX left guest kicks suppressed. The next poll then had
neither POLLOUT interest nor a kick to service pending descriptors. The
extracted wakeup regression failed before the correction and passed after
TX service was added after that flush. The precise interleaving was not
observed inside the saved timed-out VM.

The corrected cohort repeated every backend and candidate, interleaved,
with fresh overlays. All twelve runs completed with validated peer bytes.

| Candidate | UDP TX samples, Mbit/s | Median |
|---|---|---:|
| QEMU | 30982.15, 35051.78, 37601.39 | 35051.78 |
| Baseline | 22710.89, 26522.83, 23471.66 | 23471.66 |
| Corrected net notifications | 22554.35, 22573.95, 19050.46 | 22554.35 |
| Corrected net plus PCI decode cache | 22528.34, 28737.59, 22731.14 | 22731.14 |

These distributions do not establish a UDP speedup or close G7.

### Retained changes and profiles

The read-only QEMU comparison covers `hw/net/virtio-net.c`, notification
ioeventfds in `hw/virtio/virtio-pci.c`, block iovecs and merging in
`hw/block/virtio-blk.c`, VGA coalescing, PCI ROM backing, and flash ROMD.
The evidence JSON records the reference revision and each conclusion.
IOeventfd, direct ROM mapping, and VARS ROMD remain unimplemented candidates,
not established causes or claimed fixes. No QEMU code was copied or translated.

- F1. The NIC suppresses kicks while draining, rearms with the rust-vmm
  recheck protocol, and offers EVENT_IDX. Backpressure retains a POLLOUT
  continuation, including after wakeup flushes. Malformed available indices
  terminate processing instead of spinning. RX rearms on buffer shortage
  and retries an already-published buffer after a short descriptor.
- F2. Four exact-address PCI MMIO decode entries avoid rescanning ROMs,
  BARs, and command registers on repeated notifications. Configuration,
  topology, ROM attachment, and restore invalidate the entries. Values
  remain device reads and writes. In the baseline UDP leaf profile,
  `find_rom`, `find_bar`, and virtio PCI `config_read` accounted for 7.37%,
  5.46%, and 5.23% of samples. None appears in the cached profile's top 40.
  The unit reproduction changes 34 configuration reads to 2.
- F3. Modern machines batch legacy VGA writes through KVM's coalesced
  MMIO ring. Consumers serialize delivery before slot release, drain before
  register accesses and vCPU parking, and poll for writes from halted
  guests. Software-A20 machines keep their original access path. The ring
  drains into ordinary VGA state before snapshot, so the snapshot format
  is unchanged.

The profiles retain unresolved samples and DSO metadata. The baseline net
profile has 11853 samples, 2960 unresolved; the cached profile has 11979,
2936 unresolved. These are leaf IP counts, not call counts or elapsed-time
fractions. QEMU, baseline, and corrected cache candidates also have raw
`strace -c -f` tables. Tracing heavily changes throughput, so traced rates
are not gate results or an isolated cache comparison.

Three fresh-pair TCP repeats completed for every backend, with exact
sender and receiver byte equality. The combined net/cache candidate has
A-to-B and B-to-A medians of 16020.84 and 16941.49 Mbit/s. Baseline medians
are 9973.36 and 9470.04; QEMU medians are 20022.21 and 23453.85. The
combined ratios are 0.800 and 0.722. The comparison cannot separate the
net policy from the cache, and does not close G10.

### Boot and block evidence

The three-repeat interleaved Ubuntu cohort compares QEMU, corrected
net/cache, and the same candidate plus VGA batching. First-kernel medians
are 1.811, 2.686, and 2.486 seconds. Agent-ready medians are 8.20, 12.13,
and 8.27 seconds. Four-second readiness variation remains visible in the
raw samples, so these medians do not establish a quiet-host G1 pass.

The exit-counter observer initially mistook the additional ring mapping
for vCPU 0's `kvm_run` mapping. That coalesced profile is invalid and
retained as such. After correcting the offset filter, all three backends
were repeated. Legacy VGA MMIO exits were 18014 for QEMU, 138448 before
batching, and 14172 after. Total MMIO exits fell from 221958 to 96937.
The corrected single-repeat agent-ready times were 8.17, 8.18, and 8.29
seconds. Exit reduction is established; its isolated readiness gain is not.

Windows received an exit-count comparison on unchanged installed OVMF,
with QEMU at 175.87 seconds and baseline at 133.75 seconds. Some small
compilation overlapped this diagnostic. OVMF VARS reads remain a candidate
for future investigation, not a demonstrated G2 cause or a change here.

Block profiles and syscall tables compare the existing 8 GiB direct-read
path in both hypervisors. Profiled medians were 9751.86 MiB/s for QEMU
and 13923.05 for baseline. The traced medians were 10574.68 and 9502.56.
Their disagreement reinforces the measurement limit. No disk worker,
flush, memory-validation, or cache-policy change was justified by this
evidence. G3 and G9 margin still need the final cohort.

### Method and retained failures

Single VMs used CPUs 16-23; pairs used 16-19 and 20-23. Every launch
checked host capacity and used the shared slot locks. All VMs ran inside
24 GiB, zero-swap scopes, with at most four project VMs host-wide and
the owner's Android emulator excluded. The requested boot hold was
honored until `GATE WINDOW CLOSED`. Other sessions' processes were not
stopped. Diagnostic runs do not append records to benchmark history.

The first baseline executable was renamed, which prevented PID discovery.
It was reaped and the comparison restarted with the counted `vmlab-hv`
basename. The initial strace driver treated its own requested SIGINT
detach, exit -2, as failure. Those command failures and tables remain in
the evidence. Corrected runs accept only an explicitly requested detach.
The rejected candidate's UDP timeout is separate from that driver error.

`bench/profiles/phase2-round4-20261101.json` retains every diagnostic
record, failed candidate, cohort, binary and firmware hash, raw profile,
syscall table, exit counter, and driver source. It explicitly rejects a
quiet-host claim. G1, G2, G7, and G10 remain open pending the final cohort;
the historical G7 regression commit remains unassigned.

### Pre-merge correctness and completion

`just ci::check` exited 0 on the pre-merge runtime, inside the 24 GiB,
zero-swap scope with RUST_TEST_THREADS=1 and a 1800-second child-process
deadline. Its 64 result groups report 945 passing tests, zero failures,
and three ignored tests. Formatting and workspace clippy with `-D warnings`
passed. The expanded S1 NIC check negotiated EVENT_IDX and delivered all
900 echoes before save, after save, and after load. The halted-guest VGA
S1 check and all four ring tests passed, including concurrent consumers.
Firmware, reset, snapshot, and snapshot-during-fsync checks passed.
All 23 active S2 checks passed against qemu-img.

The three ignored tests were the 1 GiB S2 throughput baseline, the 4 GiB
zero-fill diagnostic, and live PipeWire loopback. They were not run
explicitly this round. All 83 benchmark unit tests also passed; their
existing mock ResourceWarnings remain. Physical USB/VFIO opt-ins were
unset, so real-device paths were not exercised. No host device was claimed,
rebound, or reset.

No scoped `todo!()` or `unimplemented!()` remains. No shared Rust trait,
control protocol, or snapshot-format signature changed. The net feature
offer now includes EVENT_IDX. Runtime fixes are separate commits on
`wp/p2-perf4`; the performance branch was not merged into main or pushed.
All owned measurement VMs,
peers, profilers, and CI jobs were reaped after recording their results.
Final quiet-host D28 validation remains unfinished by the task's explicit
bench-window instruction. The default stays unchanged.

## Round 3 authorized bench window

The requested runs completed, but external compiler work prevents a
quiet-host claim. The owner opened the window with no other project VMs
running and other sessions holding new boots. Main `fb507f3` was merged
as `4f5984de636a193930f7da123a131a67915cffe9` before building and testing.
That commit is the measured runtime source; no runtime code changed
during the cohort. The imported harness and benchmark default permits
four project VMs. The timed cohort reserved all four shared slot locks
and used at most two VMs, only for guest-to-guest transfers.

All six benchmark configurations exited 0: both backends' full Ubuntu
suite, Windows boot suite, and Ubuntu guest-to-guest suite, with three
fresh-overlay repeats each. All 22 aggregate records and every sample
are appended to `bench/history/x86_64.jsonl`. No bar, repeat, readiness
phase, flush requirement, or workload was dropped or relaxed.

The following are **observed medians under interference**, not a new
quiet-host D28 verdict. Three meet their numerical thresholds. The
round 2 cohort remains the latest valid quiet-host comparison.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Required | Observed result |
|---|---|---:|---:|---:|---|---|
| G1 | Ubuntu boot (s) | 6.74 | 8.29 | 1.230 | <= 1.000 | FAIL |
| G2 | Windows boot (s) | 90.71 | 151.37 | 1.669 | <= 1.000 | FAIL |
| G3 | sequential read (MiB/s) | 16993.81 | 12824.73 | 0.755 | >= 0.800 | FAIL |
| G4 | sequential write (MiB/s) | 5159.52 | 3390.91 | 0.657 | >= 0.800 | FAIL |
| G5 | random read 4 KiB (IOPS) | 48261.50 | 64689.30 | 1.340 | >= 0.800 | PASS |
| G6 | random write 4 KiB (IOPS) | 31083.10 | 33009.30 | 1.062 | >= 0.800 | PASS |
| G7 | UDP TX (Mbit/s) | 38711.78 | 14207.56 | 0.367 | >= 0.800 | FAIL |
| G8 | UDP RX (Mbit/s) | 73887.71 | 66092.34 | 0.894 | >= 0.800 | PASS |
| G9 | guest TCP A to B (Mbit/s) | 26025.66 | 20705.88 | 0.796 | >= 0.800 | FAIL |
| G10 | guest TCP B to A (Mbit/s) | 26502.41 | 19761.46 | 0.746 | >= 0.800 | FAIL |

### Host observations and validity

The VM-count monitor observed only this cohort's VMs, at most two at
once. Every observed VM had memory.max 25769803776 and memory.swap.max
0. The governor was `performance`; single VMs used CPUs 16-23 and
pairs used 16-19 plus 20-23. Ubuntu remained 2 vCPUs/4 GiB and Windows
4 vCPUs/8 GiB, with the existing firmware, templates, and MTU 65520.

During vmlab-hv's Ubuntu samples, the one-minute host load reached
30.42. Live rustc, rust-lld, and clippy-driver processes were observed;
a saved process/cwd check identified builds in `/home/wil/dev/wcl` and
`/home/wil/dev/opticlang-wt`. They were not this worktree's processes
and were not stopped. A request to pause external builds and restart
the full cohort was sent while measurements continued; no answer had
arrived when the cohort finished. This invalidates a quiet-host claim,
not the recorded byte counters or timestamps. The data cannot establish
an isolated regression or gain from the net changes.

The low samples remain: vmlab-hv UDP TX was 22029.34, 10737.13,
14207.56 Mbit/s; its TCP B-to-A samples were 19761.46, 7816.78,
20344.47 Mbit/s. Windows readiness was 98.52, 151.37, 151.37 seconds.
A rerun needs other CPU-heavy builds and tests held, as well as new VM
boots. No selective rerun has replaced these measurements.

### Verification and execution records

`just ci::check` exited 0 on the merged source, inside the 24 GiB,
zero-swap scope with RUST_TEST_THREADS=1 and a 1800-second harness
process deadline. It took 1627.20 seconds and reported 896 passing
tests, zero failures, and three ignored tests. Formatting and workspace
clippy with `-D warnings` passed. CI finished before timed benchmarks
began; no acceptance/test VM overlapped a timed benchmark VM.

The ignored tests were the 1 GiB S2 throughput baseline, the 4 GiB
zero-fill diagnostic, and live PipeWire loopback. They were not run
explicitly in this window. Physical USB/VFIO opt-ins were unset, so
those real-device paths were not exercised. No host device was claimed,
rebound, or reset. All 83 benchmark unit tests also passed; their
existing mock ResourceWarnings remain.

Two setup failures are retained. An unsupported `--version` metadata
probe stopped the first runner before any VM launch. The next attempt
used retained paths longer than Unix sockets accept; all six benchmark
commands failed without aggregate records, then its full CI command
passed. The corrected cohort used a short `/tmp/vp3-final-*` root and
reran every benchmark configuration. Its command exit codes are all 0;
the earlier batch's nonzero status is not treated as success.

`bench/profiles/phase2-round3-final-20261101.json` contains the full bar
table, all 22 records, command statuses, logs, scope observations,
firmware/binary hashes, CI counts, and external-build evidence. It marks
`quiet_host_claim_supported: false`. Full runtime artifacts remain in
`/tmp/vp3-final-3oxwzw1w`; driver records are under
`.scratch/perf3/final-quiet.k6FYYL` and
`.scratch/perf3/final-quiet.fhsPvS`. All owned VMs and jobs were stopped
after recording results. No shared protocol, trait, or snapshot-format
signature changed. Quiet-host D28 validation, G7's regression commit,
and G10's cause remain unfinished; the thresholds stay unchanged.

## Round 3 development diagnostics

These are shared-host diagnostics on 2026-11-01, not a new D28 verdict.
The round 2 final cohort below remains the latest gate result. No gate
threshold, workload, CPU pin, template, cache policy, or readiness condition
has changed. A final quiet-host run requires the owner's bench window.

### G7 investigation

Historical binaries were built from isolated source trees and target
directories, with the same installed firmware and rebuilt iPXE ROM.
Each diagnostic boot used a fresh overlay and the normal Ubuntu profile:
2 vCPUs, 4 GiB, CPUs 16-23, MTU 65520, 65000-byte UDP payloads, and
12-second sends. All samples, delivery counters, load averages, binary
hashes, command statuses, and incomplete attempts are retained in
`bench/profiles/phase2-round3-20261101.json`.

The initial three-send medians were 28882.25 Mbit/s at `8bcbf1b`,
23083.97 at `fe0915b`, and 22586.79 at `6cceef4`. This initially placed
the loss before the phase-4a merge. Repeated five-send cohorts did not
support a reliable bisect: `8bcbf1b` fell to 22030.51, `6e1a493` ranged
from 18229.06 to 30122.47 across boots, and `6cceef4` reached 27932.50.
The adjacent `6e1a493` and `27a43c8` distributions also overlap. No
regression commit is assigned from these measurements.

Four network changes have independent correctness or allocation evidence.
The table records consecutive diagnostic cohorts, not isolated causal
estimates. Each before/after cell is the median of five sends in one boot.

| Change | Commit | Before / after UDP TX (Mbit/s) | Evidence |
|---|---|---|---|
| C11: avoid empty-queue interrupts | `ba17a4e` | 22281.22 / 23303.73 | Empty TX and unavailable RX-buffer regressions fail before and pass after |
| C12: offer indirect descriptors | `ccc4288` | 23303.73 / 23630.37 | A two-leg indirect TX chain produces the exact framed packet and used completion |
| C13: remove TX staging copies | `b3704f5` | 24618.83 / 23593.81 | A backpressured socket preserves every byte and unsent suffix across save/restore |
| C14: honor classic interrupt suppression | `4448937` | 22313.87 / 22758.85 | Suppressed TX/RX completions raised an IRQ before the fix; suppression and reenable now pass |

The sample ranges overlap, and C13's measured median decreased. None of
these cohorts proves an isolated throughput gain. In the C14 comparison,
QEMU's median was 28470.70 Mbit/s and the C14 binary's was 22758.85;
their shared-host ratio is not a final gate result. Delivery counters
validate the traffic rather than accepting guest send rates alone.

The device's previous notification helper did not implement
`VRING_AVAIL_F_NO_INTERRUPT` when EVENT_IDX was disabled. The fix observes
the driver's flag after used-ring publication and preserves EVENT_IDX
semantics. Empty drains no longer produce completion interrupts. The
socket worker still bounds queued output and retains unsent data; queue
reset, quiescence, framing, and snapshot layouts are unchanged.

### Boot investigation

An installed-firmware pair reached first kernel output at 1.70 seconds
under QEMU and 2.98 under the C14 binary. Agent readiness was 6.97 and
12.22 seconds. A separate diagnostic SeaBIOS build reached the boot
sector at 0.83 seconds under vmlab-hv and first kernel output at 2.80.
That debug firmware is not used for gate measurements.

A three-second host instruction sampler found repeated platform timer
updates and PCI/VGA synchronization in the boot path. Leaf instruction
samples are not wall-time measurements; unresolved samples remain in
the evidence. A targeted regression reproduced an unrelated MMIO read
waiting for the RTC lock. An experimental clock change avoided platform
timer synchronization for unrelated MMIO while retaining HPET and audio
updates, periodic timer service, and pause/resume behavior.

The installed-firmware comparison retained all three boots of each
configuration. Readiness was 12.22, 8.18, 8.17 seconds before the clock
change; 8.21, 8.30, 12.12 afterward; and 6.73, 6.80, 6.95 for QEMU.
The medians, 8.18 / 8.30 / 6.80 seconds, show no boot-time improvement
from this change and do not meet G1. Early auxiliary NIC-feature probes
matched no sysfs entry because they assumed a particular hexadecimal
width. Later probes compare numeric device IDs; an empty early field
does not mean no features were negotiated.

The experiment's UDP TX medians were 23695.32 before and 22686.61 Mbit/s
afterward; QEMU measured 31736.33 in that cohort. It is not retained:
neither timing diagnostic improved, and
there is no reason to change clock synchronization for this gate without
evidence of a gain. Its patch, tests, binary hash, and measurements remain
in the evidence as the `mmio` candidate. The final code keeps the existing
platform-clock behavior.

The Windows diagnostic retained two boots of each vmlab-hv candidate:
93.32 and 94.75 seconds with C14, 113.51 and 85.76 with the discarded
clock experiment. One QEMU boot took 167.57 seconds. These include the
unchanged specialize, OOBE, AppX-settle, and in-guest reboot sequence.
The small, noisy shared-host set is not a G2 median-of-three verdict.
No Windows-specific fix is claimed.

### Verification and remaining work

All 15 network tests, the release build, workspace clippy with
`-D warnings`, and 83 benchmark tests pass. The S1 NIC test passes through
traffic before snapshot, after save, and after restore with all four net
changes. All 23 active S2 integration tests pass; the ignored 1 GiB test
has not been rerun in round 3. Benchmark tests still print their existing
mock socket/file ResourceWarnings.

The MMIO lock regression fails before the candidate and passes afterward.
HPET progress, pause/resume freezing, and audio pre-access service pass
their positive controls. The S1 fixed-base clock and both clock snapshot
roundtrips pass with the candidate binary, including SMP CMOS restore.
Two later diagnostic sequences were rejected by the per-launch VM-count
guard before starting their next VM. Their nonzero command statuses and
completed samples are retained; the scratch runners now wait and retry
only this admission error. An initial historical build failed because a
shared target directory reused incompatible dependency metadata. Isolated
target directories fixed that build setup without changing historical
sources. The initial interrupt-mask test also needed a borrow-check
correction before its intended failing assertion could run.

Every diagnostic VM uses the shared host admission checks and a
24 GiB, zero-swap scope. Live cgroup limits and CPU topology are recorded.
CPUs 16-23 are one SMT sibling of each of eight physical cores, not eight
complete physical cores. The pinning is unchanged. No host PCI or USB
device was claimed, rebound, or reset. Real-device opt-in tests remain
for the owner to run with an explicitly designated device.

QEMU's userspace virtio-net, stream socket, block submission, and x86
firmware paths were read as behavioral references. The reference checkout
was read-only; no QEMU code was copied or translated. No shared trait,
control protocol, or snapshot-format signature has changed.

The G10 diagnostic waited for both host VM slots without reserving an
idle slot. It produced no transfer samples and was stopped before any
pair launch when preparing the required bench-window handoff. G10 has
not been remeasured or diagnosed in this round.

At the development handoff, G7's regression commit, G10's cause, the
final G1/G2/G7/G10 verdicts, and full CI were unresolved. The authorized
window above subsequently supplied full CI and all requested measurements.
Quiet-host validation remains unresolved because of external CPU work.

## Round 2 final run

The final window opened with the owner's confirmation that no other
project VM was running and every other session was holding new boots.
Local main `a0b61f4` (including phase 4a) was merged in `a99edda` before
building and measuring. The measured commit is `872fab1d147233a46851cdc817ee32510c64f6ab`.
The later dirty markers reflect appended benchmark history; runtime
sources did not change during this cohort.

Both backends completed three repeats of the full Ubuntu suite, Windows
boot suite, and guest-to-guest suite on 2026-11-01 (Australia/Brisbane).
The 22 aggregate records in `bench/history/x86_64.jsonl` retain every
sample and load average. `just bench-report x86_64` gives the same
verdicts. No bar, workload, or repeat was dropped or relaxed.

| Bar | Workload | QEMU | vmlab-hv | hv/qemu | Required | Verdict |
|---|---|---|---|---|---|---|
| G1 | Ubuntu boot to agent ready (s) | 6.70 | 8.29 | 1.237 | <= 1.000 | FAILS |
| G2 | Windows boot to agent ready (s) | 108.11 | 109.68 | 1.015 | <= 1.000 | FAILS |
| G3 | sequential read (MiB/s) | 17579.97 | 15061.59 | 0.857 | >= 0.800 | meets |
| G4 | sequential write (MiB/s) | 815.32 | 1921.45 | 2.357 | >= 0.800 | meets |
| G5 | random read 4 KiB (IOPS) | 50640.50 | 67013.70 | 1.323 | >= 0.800 | meets |
| G6 | random write 4 KiB (IOPS) | 20150.00 | 19088.00 | 0.947 | >= 0.800 | meets |
| G7 | UDP TX (Mbit/s) | 41107.46 | 23870.07 | 0.581 | >= 0.800 | FAILS |
| G8 | UDP RX (Mbit/s) | 86108.37 | 75773.06 | 0.880 | >= 0.800 | meets |
| G9 | guest TCP A to B (Mbit/s) | 26011.68 | 20866.66 | 0.802 | >= 0.800 | meets |
| G10 | guest TCP B to A (Mbit/s) | 26610.76 | 20640.87 | 0.776 | >= 0.800 | FAILS |

All throughput workloads use Ubuntu 26.04.3; Windows is 19045.2006.9.
Idle CPU has no D28 bar: QEMU 1.13%, vmlab-hv 2.85% of one core.

G3 now meets the read bar. G1 improves from round 1's 12.23 seconds to
8.29 seconds, but still misses QEMU by 1.59 seconds. G2 fails its strict
median bar despite overlapping samples: QEMU 179.54, 108.11, 106.33;
vmlab-hv 195.25, 104.69, 109.68 seconds. This is not a statistically
established Windows regression relative to round 1's different baseline.

UDP TX samples are QEMU 37211.44, 41107.46, 43560.06 and vmlab-hv
29595.84, 23870.07, 23343.36 Mbit/s. G10's vmlab-hv samples are
11028.92, 20640.87, 21526.57 Mbit/s. The slow first sample is retained;
no cause is assigned without a profile. G9's 0.802x median leaves little
margin, and its first sample is below 80% of QEMU's median. These are
median verdicts, not claims that every repeat passes.

### Final-window environment

The host is helios, with 32 logical CPUs, the performance governor,
QEMU 11.1.2, and vmlab-hv 0.1.0. Single VMs use CPUs 16-23; pairs use
16-19 and 20-23. Ubuntu uses 2 vCPUs and 4 GiB; Windows uses 4 vCPUs
and 8 GiB. The Ubuntu NIC MTU is 65520. Templates and disk cache/flush
policy are unchanged from round 1.

Both host-wide slot locks were reserved for the benchmark cohort. No
project VM was present before any configuration, and the per-launch
count check remained enabled. Each VM ran in a scope with MemoryMax=24G
and MemorySwapMax=0. A live QEMU scope readback confirmed memory.max
25769803776 and memory.swap.max 0. The owner's Android emulator was
excluded and untouched. No physical PCI/USB device was claimed.

The one-minute load averages below include the measured VMs. Five- and
fifteen-minute averages, including decaying build load, remain in history.
No competing project VM was observed. A process CPU sample before the
fresh cohort found no heavy external CPU consumer.

| Run | Configuration | Repeat 1 | Repeat 2 | Repeat 3 |
|---|---|---|---|---|
| E1 | qemu Ubuntu | 1.55 to 0.97 | 0.97 to 0.72 | 0.72 to 4.29 |
| E2 | vmlab-hv Ubuntu | 4.29 to 1.85 | 1.85 to 0.73 | 0.73 to 0.90 |
| E3 | qemu Windows | 0.90 to 1.21 | 1.21 to 3.03 | 3.03 to 2.48 |
| E4 | vmlab-hv Windows | 2.48 to 1.22 | 1.22 to 2.07 | 2.07 to 2.39 |
| E5 | qemu guest pair | 2.39 to 2.82 | 2.82 to 2.94 | 2.94 to 3.14 |
| E6 | vmlab-hv guest pair | 3.14 to 6.42 | 6.42 to 5.36 | 5.36 to 5.09 |

An earlier attempt was rejected before appending results: QEMU remained
in QMP prelaunch despite the client reporting resume. The client had
treated socket chunks as complete replies and had not correlated command
IDs or checked errors. Commit `872fab1` reads buffered JSON lines,
matches IDs, rejects command errors, and verifies running state. Three
socketpair regressions fail before the fix and pass afterward. The
complete cohort above was restarted after that fix and a cooldown.

### Final-window verification

The timed suites and report completed with exit 0. `just ci::check`
also exited 0 with RUST_TEST_THREADS=1 inside the 24 GiB, zero-swap
scope: formatting, workspace clippy with -D warnings, and 890 reported
passing tests. This includes all 36 binary acceptance tests, all 379
active device tests, all 23 active S2 tests, all 14 core S1 tests, and
both filesystem restore tests. Ubuntu and Windows snapshot/reset,
Windows Server TPM, legacy-PC acceptance, and snapshot during fsync
writes passed. Guest-write, flush, and snapshot ordering were not
weakened for throughput.

CI normally ignores three tests. The 1 GiB S2 integrity/throughput test
and the zero-fill byte-verification diagnostic were run explicitly and
passed. The live PipeWire loopback test remains ignored. Physical USB
and VFIO testing was not enabled; their opt-in variables were unset.
Optional Sail-oracle feature tests were not enabled.

All 83 benchmark unit tests pass, including the QMP regressions and
main's additional coverage. Existing mock socket/file ResourceWarnings
remain. PIE and non-PIE sampler symbol-resolution self-tests pass with
compiler warnings treated as errors. No scoped todo!() or
unimplemented!() remains.

Herdr lost the outer completion marker for the main job and its wait
helper raised a TypeError. The runner independently saved each child
exit status and complete logs: CI exited 0 after 1597.48 seconds, and no
project VM remained. An auxiliary job had the same tracking issue;
those two explicit tests were rerun with normal job arguments and a
confirmed job exit 0. No test failure was hidden by the wrapper issue.

`bench/profiles/phase2-round2-final-20261101.json` records the command
statuses, timestamps, scope readback, firmware hashes, and auxiliary
checks. Full logs remain under `.scratch/perf2/final-quiet/`. QEMU C
source was not read. Only the authorized local-main merge was made;
there was no push or default change. G1, G2, G7, and G10 remain
unfinished, and work stops with evidence under the brief's failed-bar
condition instead of changing the bars.

## Round 2 development diagnostics (historical)

| Change | Commit | Diagnostic evidence |
|---|---|---|
| C5: iPXE POST timeout | `06b3f3c` | Agent readiness 12.31 to 8.19 s with the original VMM |
| C6: direct guest-buffer reads | `6e1a493` | Read-path series 9583.51 to 13998.15 MiB/s before the final table cache |
| C7: unchanged IRQ-level cache | `27a43c8` | Sampled Source share 6.08% to 1.26%; no isolated wall-rate claim |
| C8: test-hypervisor VM counting | `5cbbd87` | Reproduced omission; regression passes with the count fix |
| C9: direct sparse-zero fills | `422b67a` | Development zero probe 18052.31 to 47731.18 MiB/s; every byte verified |
| C10: correlated QMP resume | `872fab1` | Three regressions fail before and pass after; complete fresh cohort retained |

The initial round-2 CI run failed waiting 600 seconds for a shared VM slot
in Windows Server TPM acceptance. The VMM never launched for that test.
The swtpm helper's watchdog also expired during the wait. Ubuntu snapshot
and in-process reset acceptance, and Windows 10 snapshot and reset
acceptance, passed earlier in that run. Full CI must be rerun on the final
code; the failed run stopped before the remaining test crates.

At this development checkpoint, the quiet-host gate was still pending.
The round-1 table below is historical; the final result is above. No
diagnostic appends benchmark history or changes a D28 bar. Raw timings,
samples, and load averages are in
`bench/profiles/phase2-round2-20261031.json`.

The Ubuntu 26.04.3 template explicitly selects SeaBIOS. File observations
at 2 ms intervals give this process-spawn timeline. Process exec was
observed after 20-26 ms, and both backends acknowledged resume after
about 0.12 seconds.

| Diagnostic | First kernel message (s) | Agent ready (s) |
|---|---|---|
| QEMU baseline | 1.60 | 6.69 |
| vmlab-hv baseline | 6.40 | 12.31 |
| vmlab-hv with the iPXE POST delay removed | 2.51 | 8.19 |

The installed firmware emits no debugcon output. A separate SeaBIOS
rel-1.17.0 build with DEBUG_LEVEL=8 and DEBUG_IO enabled, used identically
on both backends, isolates a 3.92-second pause in the legacy iPXE POST
handler. Its default ROM_BANNER_TIMEOUT is four seconds. The firmware fix
sets that POST timeout to zero, while retaining the network-boot shell
timeout and EFI_DOWNGRADE_UX for OVMF's native network-boot entries.
It does not remove the NIC ROM or skip device initialization.

Restricting PCI quiescence did not improve agent readiness in its isolated
diagnostic, so that candidate was removed. Existing PAM, SMM, framebuffer,
and snapshot synchronization remains in place. A thin-LTO probe also
showed no material gain and did not change the release profile.

The block-read diagnostics moved from a 9583.51 MiB/s median to
13998.15 MiB/s before the final descriptor-table cache. Coalescing alone
reached about 10 GiB/s. Cached guest-region lookup reached about 11.6 GiB/s.
The next gain came from reading file extents directly into validated
guest buffers, with bounded descriptor walking and fixed-size preadv
batches. These are shared-host samples, not a G3 pass verdict.

The first 8 GiB contains 5.895 GiB of zero extents. Filling those validated
guest spans in place raises the last diagnostic read median to
14443.48 MiB/s. A separate 4 GiB guest-memory zero-fill probe measures
18052.31 MiB/s through the buffered target and 47731.18 MiB/s in place,
with every byte verified. Its test is normally ignored and was run
explicitly as `zero_fill_throughput` in the devices crate's release tests.

An IRQ-source cache avoids taking the shared hub lock for unchanged
levels. Aliases share the cache, and transitions still perform serialized
wired-OR delivery before publishing the cached level. Its regression
fails before the change and passes afterward. The source's sampled share
falls from 6.08% to 1.26%; differing host loads prevent an isolated
throughput claim.

The last development diagnostic boot takes 12.23 seconds at a starting
one-minute host load of 5.16. All samples are retained. This was not a
gate verdict. The final comparison above reruns both backends instead
of comparing development samples with round 1.

The disk seam adds `ReadTarget` and the default `DiskImage::read_into` method.
Raw images, qcow2, and `SharedDisk` implement or forward the direct path.
Buffered callers remain unchanged. S2 checks compare both read paths
across backing, compressed, zero, unaligned, and snapshot reads. The
normally ignored 1 GiB S2 test also passes. Guest writes and flush handling
are unchanged. Final-window verification is recorded above.

Final-code formatting, workspace clippy with `-D warnings`, all 24 active
block tests, and all 67 benchmark tests pass. The zero-fill diagnostic
passes when explicitly enabled. The benchmark tests still emit socket
and file ResourceWarnings from unchanged test code. ELF sampler
self-tests pass. No scoped `todo!()` or `unimplemented!()` remains.

The benchmark's process-count check now includes `vmlab-hv-test-*`, the
executables the Rust harness uses, matching its VM admission check. A
regression reproduces the previous omission while retaining the Android
emulator exclusion. Development diagnostics also held a shared test-slot
lock, checked host capacity, and launched every VM in the 24 GiB scope
with swap disabled. No host PCI or USB device was claimed. No merge,
push, or default change was performed.

## Round 1 full run

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

## Round 1 changes

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

## Round 1 failures and profiles (historical)

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

## Round 1 correctness and verification (historical)

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

Round 1 stopped with G1 and G3 measured and profiled. The round-2 result
above supersedes that verdict. D28 remains the gate for any future
default change.

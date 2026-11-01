# vmlab-hv benchmarks (D2, D28)

The continuous benchmark suite that holds the phase gates: on TCG arches
(aarch64, riscv64) `vmlab-hv` with `accel: tcg` must reach at least 100 %
of QEMU TCG's speed on this host (D1, `notes/04-speed-bar.md`); on x86_64
KVM it must hold HV-D28 before the VM default flips (`notes/29-performance.md`):
boot to agent ready no slower than QEMU, virtio-blk and virtio-net at
least 80 % of QEMU's throughput. Tracked by a result history and a
regression check (D2). TCG baseline method and first numbers:
`notes/03-architecture-baseline.md`; the phase-2 gate verdicts live in
`bench/PHASE2-GATE.md`.

## Recipes

| Recipe | Does |
|---|---|
| `just bench-run backend='qemu' arch='all' vcpus='1,4' repeats='3' guest='debian-13'` | run the TCG suite, append to the history |
| `just bench-run-kvm backend='qemu' guest='ubuntu-26.04' repeats='3'` | run the x86_64 KVM suite (guest `ubuntu-26.04` or `windows-10`; vcpus defaults to the template's) |
| `just bench-run-g2g backend='qemu'` | the x86_64 guest-to-guest net pair (two VMs at once) |
| `just bench-run qemu riscv64 1 3 flat-rv64gc` | run the flat-image QEMU TCG baseline for the two warm interpreter workloads |
| `just bench-report` | print the per-workload trend, and hv-vs-qemu against the phase's bar |
| `just bench-check` | fail (exit 1) if the latest run of a backend regressed beyond measured noise |
| `just bench-test` | the suite's own unit tests |

`bench-run` flags map 1:1 to `python3 bench/run.py` flags:
`--cpus PIN` overrides the taskset window, `--timeout-scale N` widens every
ceiling for a slow backend, `--keep` preserves per-run scratch, `--dry-run`
prints the plan without booting anything.

For an interpreter boot-time measurement without the SHA256 and xz workloads:

```sh
python3 bench/run.py --backend vmlab-hv --arch riscv64 --vcpus 1 --repeats 1 --suite boot --timeout-scale 20 --keep
```

This uses the same spawn-to-agent-Hello clock and history format as the full
suite. It appends only `boot_to_agent_ready`; it does not establish the full
TCG performance gate. `--suite boot` is restricted to TCG guests.

## Workloads

### Bounded RISC-V OS mechanisms

```sh
python3 bench/run.py --backend interpreter-specialized --arch riscv64 --vcpus 1 --repeats 3 --suite os
```

This suite starts no VM or worker thread. It runs the generated CPU through
virt's production RAM adapter, MMIO bus, PCI root, and CLINT with a deterministic
external clock. The same run records the existing warm scalar and CoreMark
controls under `flat-rv64gc`, then the following cases under `os-rv64gc`:

| Case | Measured work per round |
|---|---|
| `ram`, `ram-after-mmio` | 24,576 steps; the latter first disables the CPU-only code-epoch shortcut through a PCI configuration write |
| `same-page-data`, `self-modifying` | 128 writes beside code or alternating its first instruction, each followed by 192 steps |
| `sv39-tlb`, `sv48-tlb` | 128 mapped data pages, 192 steps per page, exceeding the model's 64-entry TLB |
| `m-traps`, `s-traps` | 128 ECALL, handler, and return cycles |
| `clint-timer`, `clint-software` | 128 real CLINT interrupt/acknowledgment/return cycles |
| `mmio` | 8,192 reads of the real CLINT time register in 24,576 steps |
| `large-code` | Four instructions at each of 4,097 distinct sites, 65,552 bytes of code |
| `fresh-code` | The same sites loaded and executed without translation warmup |
| `cache-pressure` | Four instructions at 257 sites with an explicit 128,000-unit metadata budget, forcing capacity eviction |
| `store-heavy`, `store-heavy-after-mmio` | 1,024 guest loops of 16 increments, 16 RAM stores, and a jump; the latter disables the code-epoch shortcut first |

RAM cases use 1,024 rounds. Self-modifying, large-code, fresh-code, and cache-pressure cases
use one round; the other cases use eight. Fixture setup and two warmup rounds
are excluded, except `fresh-code`, which has no warmup. Each case checks its
register result; paged loads also check every page's value and return PC.

Raw history retains retired instructions, CPU steps, elapsed seconds, source
operations, native steps, recordings, evictions, metadata usage, event count,
and checksum. Interrupt entry and faulting ECALL steps are not counted as
retired instructions. Workload keys include the version and round count, so
`just bench-check` compares like configurations. These bounded cases do not
establish S1 boot acceptance or a whole-OS performance gate.

Each OS case also reports its retired-instruction rate divided by the same
run's median flat CoreMark rate. A ratio below 0.5 fails the investigation's
speed target. Measurements are still retained when that target is missed;
`bench-check` separately checks historical regressions.

An optional `--replay-fixture DIRECTORY` adds an early-edk2 snapshot slice.
The directory contains `manifest.json`, a serialized `CpuState`, sparse RAM,
and the two captured pflash banks and command states. The manifest supplies
RAM data runs, ROM addresses, file paths, and a SHA-256 digest. The replay
verifies that digest, uses readonly flash files, and refuses uncaptured MMIO
or flash writes. It checks the endpoint's complete CPU state and every dirty
RAM page against source execution before measuring 128 repetitions of the
same 8,192-step interval. Reset, hashing, and validation time are excluded.
Every repetition must reach the same endpoint without WFI or an outstanding
LR/SC reservation. This is a frozen-clock single-hart component replay, not
an SMP boot. The fixture digest forms part of its history key.

Per guest arch (aarch64, riscv64), per vCPU count (1 and 4), N repeats,
from the local vmlab templates (`~/.local/share/vmlab/templates/<arch>/`;
newest version dir; a fresh qcow2 overlay per repeat — the template is never
written):

1. **boot_to_agent_ready** — process spawn to the guest agent answering its
   Hello on `vmlab.agent.0`, dialed fresh every second exactly like vmlab's
   readiness poller (an early persistent connection's Hello is never
   answered; notes/03 §1).
2. **sha256_256M** — `sha256sum` over 256 MiB written by `dd` from
   `/dev/zero`; timed host-side through the agent's `open_exec`. The input is
   fixed, so the digest is asserted identical across repeats.
3. **xz_9_32M** — `xz -9 -T1` over 32 MiB of repeated text, timed the same
   way.


Every repeat also verifies the xz roundtrip (`cmp`) and that the guest
really sees the configured CPU count (`nproc`).

## x86_64 KVM workloads (phase 2, HV-D28)

Per guest template (ubuntu-26.04, windows-10 — the template's own vCPU
count and memory), N repeats, fresh qcow2 overlay per repeat:

1. **boot_to_agent_ready** — process spawn to vmlab's own readiness: the
   agent's answered Hello plus, for the sysprep-generalized windows
   template, both setup phases cleared and the first-boot reboot done
   (the gate vmlab's provision script applies). Both current backends
   reboot in-process. The runner retains a respawn path for older
   vmlab-hv binaries that exit on reset. The clock starts at the first spawn.
2. **blk_seq_read_mbps** — 1-MiB O_DIRECT reads over the template-backed
   overlay's allocated extent (`/dev/vda`), via bench/randblke.c copied in
   through the agent (the template ships no fio, and GNU dd's
   `iflag=direct` buffer is refused with EINVAL by the virtio-blk device).
3. **blk_seq_write_mbps** — 512 MiB of 1-MiB O_DIRECT writes with a final
   `fdatasync` inside the timed window, to a fresh file on the overlay.
4. **blk_rand_read_4k_iops** / **blk_rand_write_4k_iops** — 4-KiB O_DIRECT
   ops at fixed-seed random offsets (reads over the allocated extent,
   writes over a preallocated 1-GiB file).
5. **net_tx_udp_mbps** — the guest blasts 65 000-byte UDP payloads at the
   host peer for 12 s; the rate is what the peer received: guest →
   virtio-net → the backend's stream netdev → vmlab's 4-byte-big-endian
   framing → the unix socket. Delivery below 90 % of what the guest sent
   fails the repeat instead of recording a broken number.
6. **net_rx_udp_mbps** — the peer blasts the guest for 12 s; the rate is
   the guest interface's own `rx_bytes` delta (kernel-counted, immune to
   userspace starvation under the flood).
7. **net_g2g_a2b_tcp_mbps** / **net_g2g_b2a_tcp_mbps** (`--suite g2g`) —
   two VMs on one two-port hub (raw byte pump, the framing is
   transparent), real TCP between the guest stacks, both directions,
   receiver-side bytes.
8. **idle_cpu_pct** — host CPU (utime+stime of the whole VM process, all
   threads) over a 60 s idle window, as a percentage of one core.
   Reported against QEMU; D28 sets no bar for it.

Both sides measure ethernet-level bytes, and both carry the one forced
deviation: `host_mtu=65520` (jumbo frames keep the stdlib guest tools and
the framing peer off the measured ceiling). The windows-10 template runs
boot_to_agent_ready only.

## Backends (x86_64)

- **qemu** — vmlab's exact argv for the profile shape
  (`src/qemu/cmdline.rs build_args`, mirrored black-box in `docs/hwref`):
  `q35`/`kvm`, `host,vmx=off,svm=off` (plus the Hyper-V enlightenments and
  the kvm-pit global for windows on this AVIC host), SeaBIOS or the OVMF
  pflash pair, virtio-serial `vmlab.agent.0`, one qcow2 overlay on
  virtio-blk (SeaBIOS bootindex), one stream-netdev virtio-net NIC,
  virtio-vga, `-usb` + usb-tablet, `-S` then QMP `cont`.
- **vmlab-hv** — the same machine as a machine description, every device
  at the PCI function QEMU auto-assigns for that argv (00:01.0 console,
  00:02.0 blk, 00:03.0 net, 00:04.0 vga, 00:05.0 the xHCI that replaces
  QEMU's ICH9 USB set, D6; AHCI at 00:1f.2 as on q35), launched `--paused`
  and released with a control-socket `resume`. The NIC carries the
  separately built iPXE ROM (D16) where QEMU loads its own combo ROM.

The host side of the segment is `bench/netpeer.c` (`netpeer peer`:
  ARP responder, byte counter, on-command blaster; `netpeer hub`:
  two-port pump), built on demand into `bench/run/bin/`.

## Noise control

The interpreter micro-benchmarks use flat RV64 images rather than booting a VM:

- **interpreter** selects indexed generated IR with guarded memoization.
- **interpreter-reference** selects named frames without memoization.
- **interpreter-specialized** selects warm shared scalar blocks. It excludes
  training time and records that time separately.

Run `just bench-run interpreter-specialized riscv64 1 3` for three pinned
samples of the 30-million-instruction scalar loop and 1,000 iterations of the
CRC-checked CoreMark algorithm mix. The longer mix reduces the timing noise and
fixed startup contribution of the earlier one-iteration workload; it has a
distinct history key. Reference and indexed backends retain one iteration.
These workloads do not establish Linux boot performance or an
official CoreMark score. Measurements include guest instruction counts,
execution and initialization time, native and training instructions, slow
continuations, and cache evictions. Backend selection clears inherited debug
and execution flags. Guest failures and CRC mismatches fail the run.

### QEMU TCG on the flat images

`just bench-run qemu riscv64 1 3 flat-rv64gc` records the `qemu` backend
for the same two warm workload keys the `interpreter-specialized` backend
owns. The interpreter binaries run unchanged: QEMU boots each ELF on the
minimal `spike` machine (`-bios none`, one `rv64` hart, TCG, no devices
the images touch), whose HTIF device honours the images' existing `tohost`
exit store — `bench/rv64/start.S` only grew the `fromhost` symbol and the
8-byte symbol sizes QEMU's discovery needs, symbol-table changes that leave
the executed stream and every instruction count identical (the run asserts
the loop's 3N+overhead count against the interpreter). The scalar loop
exists as an image too (`bench/rv64/loop.S`): the three encodings
`examples/micro.rs` writes in `loop` mode, behind a minimal `li x2` stub at
the common 0x80000000 entry. A one-iteration build measures QEMU's own
spawn-plus-teardown; its median is subtracted from every sample and both
numbers land in the record. MIPS uses the instruction count of the same
flat image measured once under the interpreter, so interpreter and QEMU
lines share workload keys, binaries, stop condition and unit, and
`bench-report` prints the D1 ratio directly. QEMU TCG translates as it
executes; its numbers cannot exclude a training phase the way the warm
interpreter numbers do, and the records say so (`warmup_excluded: false`).

- **Pinning**: the VM process tree runs under `taskset -c` on a fixed window
  of logical CPUs in the top half of the topology (smp1 → e.g. `16-19`, smp4 →
  `16-23` on the 32-thread host this was written on; `--cpus` overrides).
  Every backend gets the same window. The g2g pair pins one VM per half
  (`16-19` + `20-23`) so they use disjoint logical CPUs.
  On helios, CPUs 16-23 are one SMT thread each on cores 0-7. Their siblings
  0-7 remain outside the window. Affinity does not reserve either thread
  against other host work.
- **Recorded per run**: cpufreq governor, load average (start and end of
  each repeat), pin window, memory cap, template version, backend version,
  host CPU count, git commit (with dirty marker), and for x86_64 the APIC
  virtualisation fact, the NIC MTU and the block tool actually used.
- **Repeats**: N per configuration (default 3); the history stores the
  samples, the median, and the max-min spread.
- **Safety**: at most two VMs at a time (only the g2g pair), always inside
  `systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0`, always
  reaped (group kill plus a run-path sweep) even on failure.

## History

One JSON line per (backend, arch, guest, vcpus, workload) per run, appended
to `bench/history/<arch>.jsonl`; the file is committed, so the trend
survives. Fields:

| Field | Meaning |
|---|---|
| `recorded` | ISO timestamp of the aggregated entry |
| `commit`, `dirty` | worktree commit the suite ran from |
| `host` | hostname |
| `backend` | `qemu` or `vmlab-hv` |
| `arch`, `guest`, `vcpus`, `workload`, `unit`, `direction` | the configuration; `direction` says which way is a regression (`lower` for seconds and CPU %, `higher` for rates) |
| `repeats`, `samples` | N and the raw values |
| `median`, `spread` | median and max-min of the samples |
| `qemu_baseline` | on `vmlab-hv` lines: the newest same-config QEMU record (median, spread, commit, recorded); `null` on `qemu` lines |
| `env` | the noise-control record above |

`bench-check` compares, per configuration, the latest entry with the
previous one **of the same backend**, and fails when the median moved by
more than the noise **in the workload's worse direction**: the larger of
the two spreads, or 5 % of the previous median. Cross-backend comparison
is the phase bar; `bench-report` prints the hv/qemu ratio against it
(TCG ≥ 1.00x, x86_64 boot ≤ 1.00x and throughput ≥ 0.80x) rather than
gating on it, because the bar gates flipping a default, not every commit.

## Profiling hooks

The suite runs backends with the ambient environment, so the emulator's
switches (interpreter-only, block counts, JIT perf maps) reach the VM by
setting them before `bench-run`; each run directory under `bench/run/`
(gitignored) keeps `serial.log`, `stderr.log` and `result.json` with every
raw timing, and `--keep` also preserves the overlay and vars so a run can be
inspected post mortem.

The leaf-IP sampler is `bench/ipsample.c`, built with
`cc -O2 -o bench/run/bin/ipsample bench/ipsample.c` after the benchmark
tools directory exists. Its invocation is `bench/run/bin/ipsample PID SECONDS`.
It resolves runtime addresses through ELF load segments and symbol sizes;
`--self-test` checks its own symbol resolution. Sampling is requested at
1 kHz per existing thread with kernel and guest exclusion. Unresolved
instruction pointers remain visible. These histograms identify sampled
instructions, not call counts or wall-time percentages.

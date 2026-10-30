# vmlab-hv benchmarks (D2)

The continuous benchmark suite that holds phase 3's speed bar: `vmlab-hv`
with `accel: tcg` must reach at least 100 % of QEMU TCG's speed on this host
(D1, `notes/04-speed-bar.md`), tracked by a result history and a regression
check (D2). Spec: `.scratch/plans/phase-3-tcg/SPEC.md`, section
*Benchmarks*. Baseline method and first numbers:
`notes/03-architecture-baseline.md`.

## Recipes

| Recipe | Does |
|---|---|
| `just bench-run backend='qemu' arch='all' vcpus='1,4' repeats='3' guest='debian-13'` | run the suite, append to the history |
| `just bench-report` | print the per-workload trend, and hv-vs-qemu against D1 |
| `just bench-check` | fail (exit 1) if the latest run of a backend regressed beyond measured noise |
| `just bench-test` | the suite's own unit tests |

`bench-run` flags map 1:1 to `python3 bench/run.py` flags:
`--cpus PIN` overrides the taskset window, `--timeout-scale N` widens every
ceiling for a slow backend, `--keep` preserves per-run scratch, `--dry-run`
prints the plan without booting anything.

## Workloads

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

## Backends

- **qemu** — vmlab's exact QEMU argv for a linux-modern profile VM on the
  `virt` machine, as captured in notes/03 (`src/qemu/cmdline.rs:427`
  `build_args`): `-machine virt[acpi=off] -accel tcg -cpu max`, the edk2
  pflash pair vmlab's firmware picker chooses, virtio-serial with
  `vmlab.agent.0`, a qcow2 overlay on virtio-blk, `-S` then QMP `cont`.
  Harness deviations, both forced: `-nic none` (no network segment) and no
  TPM/GPU. This backend works today and holds the baseline.
- **vmlab-hv** — the same guest as a `vmlab-hv-proto` machine description
  with `accel: tcg`, launched `--paused` and released with a control-socket
  `resume`. Until phase 3 lands foreign-arch TCG support, `vmlab-hv` exits
  at launch with "is not supported"; the runner reports that reason as a
  skip, records nothing, and exits 0. The plumbing (overlay, control client,
  agent wait, workloads, shutdown) is complete, so the same command starts
  measuring the day support lands — then also pass `--timeout-scale` until
  the JIT exists.

## Noise control

- **Pinning**: the VM process tree runs under `taskset -c` on full physical
  cores in the top half of the topology (smp1 → e.g. `16-19`, smp4 →
  `16-23` on the 32-thread host this was written on; `--cpus` overrides).
  Every backend gets the same window.
- **Recorded per run**: cpufreq governor, load average (start and end of
  each repeat), pin window, memory cap, template version, backend version,
  host CPU count, git commit (with dirty marker).
- **Repeats**: N per configuration (default 3); the history stores the
  samples, the median, and the max-min spread.
- **Safety**: one VM at a time, always inside
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
| `arch`, `guest`, `vcpus`, `workload`, `unit` | the configuration |
| `repeats`, `samples` | N and the raw seconds |
| `median`, `spread` | median and max-min of the samples |
| `qemu_baseline` | on `vmlab-hv` lines: the newest same-config QEMU record (median, spread, commit, recorded); `null` on `qemu` lines |
| `env` | the noise-control record above |

`bench-check` compares, per configuration, the latest entry with the
previous one **of the same backend**, and fails when the median grew by more
than the noise: the larger of the two spreads, or 5 % of the previous
median. Cross-backend comparison is D1's bar; `bench-report` prints that
ratio (`hv/qemu`, bar ≥ 1.00x) rather than gating on it, because the bar
gates flipping a TCG default, not every commit.

## Profiling hooks

The suite runs backends with the ambient environment, so the emulator's
switches (interpreter-only, block counts, JIT perf maps) reach the VM by
setting them before `bench-run`; each run directory under `bench/run/`
(gitignored) keeps `serial.log`, `stderr.log` and `result.json` with every
raw timing, and `--keep` also preserves the overlay and vars so a run can be
inspected post mortem.

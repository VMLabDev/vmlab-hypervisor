# Phase 3b status at owner-requested hold

Date: 2026-11-01 UTC. Branch: `wp/p3b-virt`.
The owner put phase 3 on hold. All owned VMs and long jobs are stopped.
Phase 3b is incomplete; this branch is not an acceptance or parity pass.

## What works and what is verified

- V1: The RISC-V virt machine, firmware/direct-kernel paths, CLINT/PLIC,
  PCI devices, device tree, CPU-state snapshots, and control protocol are
  implemented. The current release build passes after merging main
  `25c3c82` in branch commit `c8c0112`.
- V2: Fresh Sail lockstep on the merged branch passes exactly 133/133 distinct
  programs, including both FP cases. The four part logs are
  `target/tcg/p3b-acceptance-lockstep-{0,1,2,3}.log`.
- V3: The initial device-tree comparison has ten explained D7 ISA-subset
  differences and zero unexplained differences. The hwref Python suite
  passes 62 tests, with two live-recapture tests skipped. Evidence:
  `target/p3b-acceptance-fdt-diff.json` and
  `target/p3b-acceptance-hwref-unit.log`.
- V4: S1 paused snapshot save/load/delete passes. Direct-kernel poweroff and
  exit-on-reset tests pass in the latest CI attempt. Reset-in-process was
  interrupted. Evidence: `target/p3b-acceptance-paused-snapshot.log` and
  `target/p3b-full-acceptance-ci.log`.
- V5: `just tcg-generate-check` reproduces 81,607 Rust bytes and 2,085,721
  compressed IR bytes. Latest CI passes formatting and workspace clippy
  with warnings denied, but its workspace tests were interrupted.
- V6: Sixteen bounded OS-like workloads and a checked early-edk2 replay are
  available through `bench/run.py --suite os`. Runtime fixes and measured
  limits are recorded in `bench/OS-THROUGHPUT.md`. Before the latest merge,
  TCG/IR tests, 32 virt tests, and 92 benchmark tests passed.

## Unverified acceptance

The fresh Debian template boot used two harts, 2 GiB RAM, a fresh overlay,
and the release binary from `c8c0112`. It reached OpenSBI and executed edk2
code, but did not reach the edk2 banner, Linux login, or agent-ready.
The last progress report was at 2,255.7 seconds, with 409,189,888 hart-0
steps, 19,472 recordings, and 16,708 evictions. Cache use was
31,993,542 of 32,000,000 units. The owner stopped the attempt; it did not
exhaust its 7,200-second deadline. Artifacts remain in
`/var/tmp/hv-riscv-PZm8Ys/` and `target/p3b-linux-acceptance.log`.

Boot-to-agent time, Linux hardware capture/diff, booted-guest snapshot
rollback, Fedora acceptance, and a complete `just ci::check` remain unverified.
Both stopped drivers return 101 because they were explicitly killed for the
hold. Those exits are not evidence of a guest failure or a completed CI run.

## Open problems and evidence

- F1: Live execution differs sharply from the early replay. The replay reaches
  about 10 MIPS, while the live boot averages about 181,000 hart-0 steps/s
  through its last report. The replay repeats a frozen-clock, single-hart,
  RAM/ROM-only interval with warm translations. It does not reproduce the
  live cache footprint or establish the cause of the live slowdown.
- F2: The requested wall-time split between execution, WFI/parking, and MMIO
  was not measured before the hold. Timer/interrupt delivery and slow device
  polling remain hypotheses, not findings. No timer-related fix was made.
  Live GDB attachment was denied by host ptrace policy; the policy was not changed.
- F3: The OS-like speed target is unmet. Paged state handling, cold recording,
  cache pressure, and conservative validation after possible untracked DMA
  remain expensive. Detailed before/after numbers are in
  `bench/OS-THROUGHPUT.md`.
- F4: The targeted `just bench-check riscv64 interpreter-specialized` passes.
  The all-history check fails on an imported QEMU/x86 idle-CPU record,
  1.13% to 1.78%, beyond measured noise. History and thresholds were retained.
  Evidence: `target/p3b-acceptance-{riscv-bench-check,bench-check}.log`.

## Next steps when the owner resumes phase 3

1. A1: Instrument per-hart execution, WFI/park, lock wait, and MMIO wall time.
   Compare live timing against a matched replay. Check CLINT rate against
   DT `timebase-frequency`, compare scheduling, and interrupt delivery latency.
2. A2: Reproduce the identified defect in a bounded non-VM test, fix it in a
   separate commit, and repeat lockstep and benchmark guards before another boot.
3. A3: Complete Debian and Fedora readiness, Linux hardware comparison,
   booted snapshot rollback, boot-time history, and full CI. These are the
   remaining 3b acceptance obligations before a 3c JIT readiness decision.

Logs and captured fixtures under `target/`, `.scratch/`, and `/var/tmp/`
are local evidence and are not included in the pushed branch.

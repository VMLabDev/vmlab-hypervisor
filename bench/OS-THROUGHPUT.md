# RISC-V OS throughput investigation

The 0.5x-CoreMark target is not met. Linux boot acceptance and full
`just ci::check` remain unfinished; neither was run during this speed phase.
No whole-OS speedup is established by these component measurements.

## Method and results

`--suite os` uses the production virt memory adapter and CLINT without VM
workers. Three repeats run on CPU 16. Rates count retired instructions;
interrupt-entry steps are reported separately. Setup and warmup are excluded.
The matched CoreMark median is 46.49 MIPS, giving a 23.25 MIPS target.
All samples, including failures and intermediate runs, remain in
`bench/history/riscv64.jsonl`.

The synthetic baseline is `759398d`, which already includes the nested-fragment
retention fix. The following post-fix medians are from runtime `18953e7`.

| Workload | Before MIPS | After MIPS | Meets target |
|---|---:|---:|---|
| RAM loop | 38.65 | 822.42 | yes |
| RAM after possible DMA | 32.69 | 640.95 | yes |
| Data writes beside code | 0.757 | 72.14 | yes |
| Alternating instruction images | 0.00300 | 46.70 | yes |
| Sv39 replacement-TLB traffic | 0.0305 | 0.413 | no |
| Sv48 replacement-TLB traffic | 0.0250 | 0.402 | no |
| M-mode ECALL/return | 0.0306 | 1.864 | no |
| S-mode ECALL/return | 0.0309 | 1.711 | no |
| CLINT timer interrupts | 0.0154 | 0.525 | no |
| CLINT software interrupts | 0.0153 | 0.545 | no |
| CLINT MMIO reads | 0.0663 | 17.56 | no |
| 4,097 code sites | 0.635 | 0.643 | no |
| First execution of those sites | 0.000255 | 0.000266 | no |
| Forced capacity pressure | n/a | 0.000288 | no |
| Guest store-heavy loop | 2.995 | 26.10 | yes |
| Same loop after possible DMA | 2.495 | 2.550 | no |

The store-loop baseline is `a3f5da0`, before scoped epochs. Other synthetic
baselines are `759398d`. These suite runs share a workstation, not a quiet host.

These are mechanism diagnostics, not equivalent application mixes. Interrupt
cases deliver an interrupt after three retired handler/foreground instructions.
The code-site cases make a host call every four guest instructions. Capacity
pressure deliberately reduces the metadata budget to 128,000 units for 257
sites; each measured pass records and evicts 257 translations.

## Captured firmware and scalar controls

The retained early-edk2 checkpoint supplies a strict RAM/ROM-only, frozen-clock,
single-hart replay. Each 8,192-step interval checks complete CPU state and all
six dirty RAM pages against source execution. The benchmark times 128 identical
intervals per repeat. It refuses uncaptured IO, flash writes, WFI, and reservation
boundaries. The fixture digest and endpoint digests are retained in raw history.
There is no kernel capture because Linux has not booted successfully.

This early interval already runs around 10.8 MIPS on `5d40efe`; it does not
reproduce the later whole-boot collapse. A paired comparison exposed extra
pflash locking in the new epoch path. That regression led to the atomic flash
generation-clock fix rather than removal of validation.

Three final alternating pairs on CPU 18 compare `5d40efe` with `18953e7`.
Median wall rates are 10.35 and 10.02 MIPS; thread-CPU rates are 10.37 and
10.03 MIPS. Both produce identical CPU and RAM digests. The fix does not
establish a speedup on this interval, and its rate remains below the target.

The earlier short scalar guard failure remains recorded. Five alternating
old/current pairs of the same 300-million-instruction loop on CPU 18 gave
0.192275286 s for `5d40efe` and 0.188502310 s for `a3f5da0`. Both execute the
same native steps, 2,074 source operations, and two continuations. This matched
longer control did not reproduce the regression.

## Fixes

- C1: `7876a6b` removes the residual four-fragment eviction cap while retaining metadata budgets.
- C2: `b348b56` records guarded constant context transitions. Sixteen warmed M interrupt cycles drop from 34,032 source operations to zero; S transitions also match complete source state.
- C3: `c59e0a8` reuses exact recurring instruction-image identities within a globally bounded history. Warm alternating code stops recording new traces.
- C4: `5d1d56e` rejects incompatible inputs and entry predicates before copying aggregate snapshots; parent projections borrow captured values.
- C5: `b50d4cf` removes startup-sized register assumptions for platform CPUs. Crossing 65,535 no longer retrains the OS PC working set.
- C6: `eba26d3` adds optional `Memory::code_epoch`, with a conservative `None` default. An unchanged 4,096-step VMM loop goes from 35 additional window queries to zero; stores, DMA, mapping changes, and restore remain checked.
- C7: `a3f5da0` caches wide global snapshots by slot version and refreshes on mutation. The regression checks reuse of a 4,096-element snapshot and observes changed values.
- C8: `c0670e2` makes flash epoch readers use an atomic generation clock. Writers publish changes before mutation; individual guards still reject non-array execution. A held-device-lock regression and real command-mode execution test cover this path.
- C9: `18953e7` validates merged physical code ranges with scoped epochs after a coarse epoch changes. Unrelated tracked RAM stores no longer recheck every instruction window. The store-heavy case rises from 2.995 to 26.10 MIPS. RAM with possible untracked DMA keeps conservative validation; flash retains its independently tracked clock.

The shared interface changes are the additive `Memory::code_epoch` and
`Memory::code_epoch_for` methods, both with conservative defaults.
No control protocol, machine-description, device trait, or snapshot format changed.

## Remaining limits

- F1: After possible untracked DMA, post-store code validation still dominates: 76/80 stack samples include `Trace::code_valid`, and 74/80 include `PhysicalMemory::code_stamp`. The loop has zero source operations, recordings, and evictions. C9 fixes tracked off-page writes, but cannot safely skip RAM validation after untracked DMA.
- F2: Paged workloads remain expensive with zero source fallback. The final Sv39 profile includes binding work in 23/80 samples and value/vector cloning in 20/80. The software TLB is still model-level aggregate state with fragment selection, projection, and publication costs. Versioned snapshots do not provide a compact native TLB access path.
- F3: Cold specialization remains hundreds of instructions/s. The 16,388-instruction first-touch case executes 60,737,856 source operations. Capacity misses can therefore collapse throughput even when eviction itself is cheap. Larger constants do not remove first-touch cost.
- F4: The early replay cannot establish behavior after the live boot fills its 64-million-unit cache. A later slice is unavailable without another boot. The diagnostics isolate relevant costs, but they do not attribute every second of the stopped boot.

QEMU reference reading found context-selected tag/addend RAM fast paths,
backing-code-address TB lookup, tracked code-page writes, reversible chaining,
and synchronized cache turnover. No QEMU implementation was copied or translated.
The local research report and source inventory are under
`.scratch/research/qemu-tcg-mechanisms-and-vmlab-runtime-costs-for-/20261101-132805-bbf9/`.

Further work needs tracked DMA writes at the guest-memory seam, compact native MMU state,
and a faster or reusable translation frontend. Linux acceptance remains deferred.

## Verification artifacts

`target/tcg/p3b-scoped-lockstep-summary.json` records exact 133/133 manifest coverage
on `18953e7`. `target/p3b-range-final-tcg.log` records passing TCG/IR release tests.
`target/p3b-range-final-virt.log` records 32 passing virt tests, including the
adapter, flash, and replay boundaries. Workspace all-target clippy with
`vmlab-hv-vmm/tcg-bench` and `-D warnings` passes in
`target/p3b-scoped-epoch-lint.log`. The Python bench suite passes 92 tests.
Profiles are in `target/p3b-store-profile.log` and `target/p3b-final-tlb-profile.log`.
The captured fixture and paired scalar control live under `.scratch/prototype/`.

The full suite initially failed four historical guards. Three alternating
pre-scoped/current pairs on CPU 18 did not reproduce those regressions.
Before/after median seconds are 0.002463483/0.002479604 for adjacent data,
0.000293732/0.000302008 for alternating code, 0.438300914/0.432442134 for
Sv39, and 0.440552611/0.432301220 for Sv48. Raw pairs remain in history and
`target/p3b-paired-os-regressions.json`; failed samples were not removed.
`just bench-check` then passes without changing its thresholds, as recorded in
`target/p3b-final-bench-check.log`.
Full S1/Linux acceptance and full CI are not replaced by these checks.

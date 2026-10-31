# Interpreter performance

## Result and target

The 50 MIPS target is not met on the realistic mix. Exploratory warm runs
reached 361.00 MIPS on the scalar loop and 19.71 MIPS on the CoreMark mix.
These are single trials, not pinned benchmark medians. They do not establish
that Linux can boot in minutes. The pruned-interpreter checkpoint below
predates whole-block specialization.

All results below use release builds on helios. Model initialization is
outside the timed region; fetch, protection checks, translation, decode,
execution, traps, retirement, and generated clock ticks remain inside it.
Early diagnostic runs were not CPU-pinned and overlapped lockstep validation.
They are single trials, not noise-controlled benchmark medians.

## Changes and measurements

The scalar loop executes 3,000 guest instructions: increment, decrement,
and a backward branch. Its final registers and PC are checked.

| Change | Seconds | MIPS |
| --- | ---: | ---: |
| Named frames, original execution, operation profiling enabled | 7.226403 | 0.000415 |
| Memoize pure generated calls; versioned globals | 4.782523 | 0.000627 |
| Indexed dependency versions and unchanged-value stores | 4.694903 | 0.000639 |
| Also cache generated state checks whose dependencies stay unchanged | 0.984263 | 0.003048 |
| Indexed local slots, literal pool, borrowed aggregate access | 0.898176 | 0.003340 |
| Prune dead builder locals; omit replay of unchanged guarded writes | 0.330917 | 0.009066 |

The first four runs use `examples/loop`; their operation counter also
includes initialization. The last uses `examples/micro`, whose counter
excludes initialization. Profiling itself adds overhead. Compare the pinned
benchmark history for reproducible within-backend regression measurements.

The unchanged CoreMark algorithms execute 316,515 guest instructions with
the fixed input below:

| Executor | Seconds | MIPS |
| --- | ---: | ---: |
| Guarded memoization with named frames | 264.319697 | 0.001197 |
| Guarded memoization with indexed frames | 234.596872 | 0.001349 |
| Also prune locals and eliminate unchanged-write replay, under perf | 130.698031 | 0.002422 |

Both runs passed the list, matrix, and state CRC checks. The ELF also passed
the same checks under black-box `qemu-riscv64`. Its fixed stack initially
failed in QEMU userspace; the linker script now maps that stack and separates
ELF segment permissions at page boundaries. The flat-memory interpreter had
already provided the stack RAM.

## Profiling

Initial `perf record -e cycles:u --call-graph dwarf` and userspace
software-event attempts failed with `perf_event_paranoid=2`. The owner later
enabled per-process perf access with `perf_event_paranoid=1`. Call-graph
collection then succeeded without sudo. No agent changed a host sysctl.
The local perf package was unpacked under `target/tools`; it is not a runtime
or build dependency.

The release binaries used `CARGO_PROFILE_RELEASE_DEBUG=1` in isolated target
directories. Profiles contain user cycles and DWARF call graphs. The reference
and indexed scalar profiles each execute 15,000 guest instructions; they
contain 19,251 and 1,824 samples respectively, with no lost samples. The
CoreMark indexed profile contains 19,586 samples. Rates were 499 Hz for the
loop and 99 Hz for CoreMark.

| Profile | Exclusive sampled cost |
| --- | --- |
| Reference loop | BTreeMap aggregate cloning 15.74%; `eval` 5.18%; `cfree` 4.73%; `assign` 4.38% |
| Indexed loop | `run_cached` 11.31%; slot-vector initialization 8.02%; two libc `memcmp` sites 26.31% |
| Indexed CoreMark | aggregate cloning 13.04% plus another monomorphization at 3.33%; `run_cached` 8.56%; slot initialization 5.05% |

Caller graphs attribute the hot loop comparisons to aggregate equality.
The two libc addresses were resolved by inspecting the installed ELF's
`__memcmp_evex_movbe` symbol. Replaying possible global writes on every
memo hit copied and compared unchanged trigger aggregates. It is unnecessary:
all possible writes are guarded inputs, and insertion requires that none
changed. The cache now returns only its result.

Profiling also exposed a specialization defect: the I-type builder retained
23,269 dispatcher locals despite having only 44 reachable declarations.
Its indexed frame allocated and initialized every retained slot. A regression
first failed with `23269 != 44`; pruning locals in constructor and scalar
argument specialization made that same test pass. Indexing independently
uses reachable declarations instead of stale local metadata.

One longer profiling attempt failed its mmap allocation. Retrying with
`perf record -m 64` bounds the profiling ring without changing host limits.
Raw profiles and reports are under `target/tcg/perf/`.

The longer post-pruning loop profile has 7,286 samples over 150,000 guest
instructions. `run_cached` is now 17.59% exclusive; argument-vector collection
and its iterator account for 6.24% and 5.24%; `call` is 4.67%; `Value::clone`
is 3.77%. This is the remaining generic executor and frame/argument traffic.
The profile does not support treating bignum arithmetic alone as the root
cause. The next stage must remove those calls and values from the hot path.

Before optimization, 3,000 guest instructions executed about 30.25 million
IR operations. The largest exclusive operation counts were:

| Generated function | Exclusive operations |
| --- | ---: |
| `zpmpReadAddrReg` | 5,394,000 |
| `zcurrentlyEnabled` | 3,087,762 |
| `zpmpCheck` | 1,794,000 |
| `z_get_Pmpcfg_ent_A` | 1,692,000 |
| `zmatch_memory_triggers` | 1,620,000 |

Guarded memoization removes repeated evaluation of unchanged PMP,
extension, and trigger state. It does not remove these architectural checks.
After both optimizations, the scalar loop still executes 4,940,850 IR
operations, about 1,647 per guest instruction. CoreMark executes about
3,040 IR operations per guest instruction. Expression evaluation and native
intrinsic work are additional costs, not separate operations in this counter.

The largest remaining loop counts are `zchecked_mem_read`, `zupdate_mip`,
`zrun_hart_active`, `zsail_mem_read`, `zfetch`, and `ztry_step`. These are operation
counts, not CPU-time samples. They do not prove which allocator or arithmetic
routine consumes the most host cycles.

## What changed

`vmlab-hv-tcg-ir::effects` computes transitive global reads, writes, and
external effects to a fixed point, including recursive calls. Unknown
intrinsics and callees, memory, references, barriers, reservations, and
platform operations prevent memoization. Enum values and Sail exception
bookkeeping participate in dependency tracking.

The runtime checks versions of all read and written globals. A miss is
cached only if those versions stay unchanged throughout the generated call.
This permits repeated idempotent checks while rejecting changing-state
calls. Entries retain only their result; unchanged guarded writes need no
replay. Each function
holds at most 128 argument entries. Architectural writes, including external
`Globals::insert` and `Globals::get_mut` calls, invalidate dependencies.
Mutable access invalidates conservatively; inserting an equal value does not.

`vmlab-hv-tcg-ir::indexed` assigns stable local slots and separates literals
from operations. The runtime binds literal values once per compiled builder.
It retains branch indices, reference behavior, global lookup, exceptions,
and the same memory and intrinsic implementations. Borrowed field and union
access avoid copying entire aggregates to select a leaf. Constructor and CSR
specializations have separate executable cache identities.

The named executor remains available with `Interpreter::indexed = false`;
memoization can independently be disabled with `Interpreter::memoize = false`.
These switches support comparisons without adding instruction-specific
semantic overrides.

## Benchmark workloads

`just bench-run interpreter riscv64 1 3` records three pinned samples of the
scalar loop and the CoreMark algorithm mix. `interpreter-reference` selects
the named executor with memoization disabled. Both use the existing history,
report, noise, regression, CPU pinning, and 24 GiB memory-cap machinery.
Model initialization time and instruction counts are recorded separately.
Guest failures, missing tools, timeouts, and CRC mismatches fail the run.
Timeout and interruption cleanup reap the measurement process group.

The corpus runner `crates/vmlab-hv-tcg/scripts/lockstep-check.py` reads the
131-program manifest and includes FADD.S/FADD.D. `--prepare-only` builds the
two FP ELFs. Normal execution builds its Sail-enabled driver in an isolated
target directory, so `cargo test` cannot replace it with a non-Sail example.
An explicit `--binary` selects a preserved measurement binary. `--part` and
`--parts` divide all 133 programs deterministically without reducing coverage.

CoreMark is fetched at `1f483d5b8316753a742cbf5590caf5bd0a4e4777` under
`target/tcg/coremark`. Its upstream algorithm sources remain unchanged.
The driver calls upstream `iterate()` once with all algorithms, 2,000 bytes
of static memory, and volatile seeds `0, 0, 0x66`. It checks the published
2K performance CRCs: list `e714`, matrix `1fd7`, and state `8e3a`.
This is a compiled realistic instruction mix, not a reportable CoreMark
score: it does not use CoreMark's minimum-duration scoring driver.

For individual measurements:

```sh
cargo build --release -p vmlab-hv-tcg --example micro
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  target/release/examples/micro loop 1000
```

Set `VMLAB_TCG_PROFILE=1` for per-function operation counts.
`VMLAB_TCG_NO_MEMO=1` and `VMLAB_TCG_NO_INDEXED=1` select the reference paths.
Benchmark history retains raw JSON measurements, including initialization
seconds, guest instruction counts, IR operations, and execution seconds.

## Verified checkpoint

The memo-only, indexed, and pruned stages each matched all 133 programs
against the independent Sail C++ oracle: 452,776 instructions per stage.
The source revisions are unchanged; this checkout uses GNU RISC-V binutils
for the I/M corpus, so ELF padding and instruction totals differ from the
earlier LLVM-built emitter-package corpus. The indexed stage also passed
all 134 flat-image architectural tests. The local semantic and cache
regressions pass, including the dead-local reproduction above.

After pruning, the complete container-backed `just tcg-generate-check`
passed with byte-identical 81,607-byte Rust and 2,085,721-byte compressed IR
artifacts. The interim workspace `just ci::check` passed after the virtio-fs
and VFIO merges. That run started before the final pruning changes and USB
merge; it is not the final gate for the continuing whole-block work.

Pinned history records these execution times, excluding initialization:

| Workload | Reference seconds, one sample | Pruned median seconds, three samples | Pruned spread |
| --- | ---: | ---: | ---: |
| Scalar loop, 3,000 instructions | 7.190588 | 0.328526 | 0.003811 |
| CoreMark algorithms, 316,515 instructions | 1,029.132116 | 124.155773 | 1.906872 |

These are approximately 21.9x and 8.3x faster respectively. The reference
run has no measured spread because it has one sample. `just bench-check`
passes but has only one history entry per new configuration, so it does not
yet establish a within-backend regression trend.

## Remaining work toward 50 MIPS

At the measured scalar operation count, 50 MIPS would require more than
82 billion interpreted IR operations per second. Improving dispatch alone
cannot close that gap. Interprocedural partial evaluation must remove most
of the model-level work before execution. Calls still allocate argument
vectors and local frames; generic integer and bitvector operations still
allocate bignums, even where the typed IR records a native width.

The next compiler work is to specialize configuration and decoded operands
across calls, inline generated helpers, eliminate dead aggregate state,
and allocate native scalar temporaries across a complete instruction block.
Those transformations must preserve memory faults, interrupts, and trap
boundaries and remain available to the JIT. No hand-written per-instruction
shortcut or performance claim substitutes for that work.

## Whole-block specialization measurements

The experimental `Riscv64::specialize` path records generated Sail execution
into scalar SSA, ordered memory events, guards, and exact source continuations.
It does not contain instruction-specific implementations. The shared IR crate
owns constant folding, range proofs, scalar scheduling, and host-word lowering.
The named interpreter remains the continuation path.

The first attempted trace gate matched all 133 programs and 452,776 instructions,
but the adapter reset silently cleared `specialize`. That run checked the
reference path. The log `target/tcg/trace-lockstep.log` is not evidence for
native traces. Reset now preserves the execution options, and the driver
asserts that specialization stays enabled and that native instructions run.
The corrected gate passed 133/133 and matched 452,776 instructions: 4,392 ran
natively, 447,840 were initial recordings, and 544 used slow continuations.
The unrolled architecture tests have few repeated PCs. The logs are
`target/tcg/verified-lockstep-{0,1,2}.log`. Later optimizer changes still need
a fresh full gate.

The following exploratory results use release builds on this host. They are
not pinned bench-history comparisons. The cold measurements include recording
inside the instruction timer. Warm measurements exclude a separate training run.

| Change | Workload | Instructions | Seconds | MIPS |
| --- | --- | ---: | ---: | ---: |
| Initial single-step traces | Scalar loop, cold | 3,000 | 0.146914 | 0.020420 |
| Batched traces with interned recipes | Scalar loop, cold | 300,000 | 1.186580 | 0.252828 |
| Same path, longer profile | Scalar loop, cold | 3,000,000 | 2.575892 | 1.164645 |
| Version guards, scalar scratch reuse | Scalar loop, cold | 3,000,000 | 1.299769 | 2.308103 |
| Same path, warm | Scalar loop | 30,000,000 | 2.645523 | 11.339914 |
| Elide constant fetch guards and unused checkpoints | Scalar loop, warm | 30,000,000 | 2.201573 | 13.626623 |
| Scheduled host-word operations | Scalar loop, warm | 30,000,000 | 1.550403 | 19.349804 |
| 256-instruction traces, complete warmup | Scalar loop, warm | 30,000,000 | 1.207700 | 24.840614 |
| Combine decrements, fold modular comparisons, fuse guards | Scalar loop, warm | 30,000,000 | 0.285927 | 104.921789 |
| Initial single-step traces | CoreMark algorithms, cold | 316,515 | 33.490258 | 0.009451 |

Perf used hardware cycles, DWARF call graphs, and `-m 64`. The loop profiles
used 499 Hz. CoreMark used 99 Hz. No samples were lost. Kernel symbol addresses
remain restricted, but userspace call graphs resolve. Raw profiles and text
reports are under `target/tcg/perf/`.

The cold batched profile attributed about 26% of samples to libc comparison
sites. Its caller stacks included context-value equality. Global version slots
now avoid comparing unchanged configuration aggregates. The warm profile
`warm-loop.data` excludes the first two seconds. It attributes 44.84% to recursive
scalar evaluation, 23.02% to trace dispatch, and 4.92% to scalar binary evaluation.
This evidence motivated the scheduled host-word form.

CoreMark's first trace run had 233,431 specialized hits and 81,627 continuation
exits. The main stop was `tmod_int` in generated address-alignment checks.
Lowering nonnegative remainder by a power of two removes that generic path.
The next run exposed constant-vector updates in byte packing and dynamic Sail
assertions. Both need generated-IR lowering rather than ISA shortcuts.

The 50 MIPS goal remains unmet. Larger blocks alone do not prove a win:
training must cover every cached entry PC before a warm timer starts, and
CoreMark must retain its CRC checks and exact stop at the tohost store.

The scalar loop clears 50 MIPS in the exploratory run above. The mix does not.
The pre-pruning bounded cache regressed CoreMark to 805.250616 seconds cold,
with 3,707 recordings and 3,513 evictions. A later one-million-unit cache run
took 688.890950 seconds after a 684.418262-second warmup. These failures remain
evidence against oversized snapshots and repeated specialization. A
256-instruction experiment was interrupted after its cache grew beyond 10 GiB.
The host's 24 GiB process cap was not increased.

The compiler now shares source functions and identical local/global binding
lists. One measured trace held 96,059 local bindings before interning, versus
2,582 value recipes. Liveness removes dead values while a compact declaration
bitmap preserves local shadowing on a continuation. DCE removes unused recipes
and orphaned exits without discarding ordered reads. The weighted cache budget
is eight million units, with LRU eviction and at most four entry variants per
PC. It is a metadata budget, not a byte count. Promotion is limited by estimated
trace size, and an oversized candidate cannot evict the existing cache.

Failed comparisons can now compile native alternative fragments. Their scalar
inputs bind to the completed prefix's SSA values. Tests require repeated
alternatives to run without Sail operations and without repeating stores.
The mix measurements and the full gate must be repeated for these changes.

`interpreter-specialized` is the bench backend for warm scalar blocks. It runs
30 million scalar-loop instructions and the CRC-checked CoreMark image. JSON
records separate warmup time, native instructions, training instructions, slow
continuations, and evictions. The backend rejects measurements with no native
instructions. Reference and indexed measurements explicitly clear inherited
specialization and profiling flags.

### Bounded traces and native global words

The binding-list interning experiment still thrashed the eight-million-unit
cache. Its timed CoreMark pass took 527.291465 seconds, with 1,068 recordings
and 880 evictions after a separate warmup. Increasing block length before
bounding path variation made execution slower.

Starting with one guest instruction and promoting incrementally removed this
failure. Promotion grows by at most fourfold and requires a small closed loop.
The 316,515-instruction warm pass then ran with no recordings or evictions.
It used 316,512 native instructions and three source continuations.

| Change | CoreMark seconds | MIPS |
| --- | ---: | ---: |
| Single-instruction initial traces | 0.633981 | 0.499250 |
| Retain scalar global words across traces | 0.523350 | 0.604786 |
| Prebind global slots and watch configuration revisions | 0.242350 | 1.306022 |

Native scalar words remain private to a `run` or `step` call. The runtime
publishes them before source recording, a source continuation, or return to
the caller, including errors. Legacy writes supersede pending native words.
Context guards watch their global slots. Unrelated register writes no longer
force every configuration guard to be scanned on each trace entry.

The native-word profile used hardware cycles at 999 Hz with DWARF call graphs.
It recorded 11,919 samples with none lost. The final warm time slice contains
436 samples: named native-word reads account for 20.65%, writes for 8.74%,
and entry validation for 9.16%. These samples motivated prebound slots and
the configuration revision counter. Profiles including warmup instead show
string formatting and cloning in recording. Dynamic register names are now
constructed once rather than formatted repeatedly for each model global.

The new nested-alternative regression changes both branch decisions after an
MMIO read. Each invocation performs exactly one read. Once both alternatives
are trained, repeated invocations execute no source IR operations. Existing
batched tests compare every Sail global, including wide RVFI state, at API
boundaries and check retired-instruction accounting on a memory fault.

The full corpus, generation check, workspace gate, and pinned benchmark history
must be repeated for these changes. The realistic-mix target remains open.

### Shared scalar lowering checkpoint

These exploratory release measurements pin the scalar process to CPU 18 and
the mix to CPU 16. Concurrent oracle validation can affect host frequency and
scheduling; these are individual trials, not benchmark-history medians.

| Change | Workload | Instructions | Seconds | MIPS |
| --- | --- | ---: | ---: | ---: |
| Page guards, range refinement, modular folding | CoreMark, 100 iterations | 30,437,270 | 1.973903 | 15.4198 |
| Skip unused checkpoints during full-block execution | CoreMark, 100 iterations | 30,437,270 | 1.860883 | 16.3564 |
| Bind scalar inputs directly to physical slots | CoreMark, 100 iterations | 30,437,270 | 1.753713 | 17.3559 |
| Same compiler and input binding | Scalar loop | 30,000,000 | 0.083102 | 361.0022 |

The last scalar run records no new blocks and executes 29,999,998 instructions
natively, with two slow continuations and 117,189 root trace calls. Its warmup
takes 5.449440 seconds and retires 65,536 instructions. The mix uses two full
warm passes, restoring data and architectural state between passes. Its timed
pass has 30,437,209 native instructions, 54 training instructions, seven slow
continuations, and no evictions. Warmup takes 51.289180 seconds. The port's
`ITERATIONS` macro controls upstream `iterate()`; algorithms remain unchanged.
The 100-iteration ELF also exits successfully under black-box `qemu-riscv64`.
Published first-iteration CRC checks remain enabled. This is not a CoreMark score.

The shared IR now propagates guarded numeric ranges, preserves logical widths
through scalar views, folds modular arithmetic and idempotent bitfield updates,
and coalesces view nodes into physical slots. Ordered effects and exact source
continuations remain explicit. Scalar inputs write those slots at entry, so
scheduled execution no longer dispatches input-copy operations. Code guards
share page stamps; `Memory::code_stamp` now requires page-wide invalidation.
The new optional `Memory::ram_bounds` supplies guarded numeric hints, not memory
access permission. A failed hint relaxes later variants at that PC.

Several attempted changes failed and were reverted or corrected:

- Promoting general paths beyond closed loops produced 0.52, 1.38, and 7.29
  MIPS, despite longer warmups. Promotion remains limited to compact loops.
- Treating every RAM-valued register as a permanently bounded address caused
  repeated recordings and eviction. Per-input relaxation removes that thrashing.
- Range folding initially discarded Sail result widths; CoreMark reached its
  instruction limit instead of completing. Typed scalar views fixed the defect.
  A second regression caught literal extension folding losing constants.
- Estimating every promotion at 256 instructions stalled smaller loop growth
  and allowed expensive recordings near the caller's final boundary. The budget
  now uses the next growth step and requires enough caller instructions. The
  warm-tail regression requires zero new recordings and zero source operations.

Hardware-cycle profiles with DWARF call graphs motivated these changes.
`latest.data` contains 24,808 samples with none lost. Its warm slice attributes
17.58% to `run_captured` (including inlined execution), 10.82% to scalar
evaluation, 10.67% to linear execution, 5.87% to binding publication, and 4.21%
to input evaluation. The later `loops.data` profile contains 19,103 samples
but loses one chunk containing four samples. Its warm slice attributes 16.85%
to `run_captured`, 13.67% to linear execution, 9.07% to binding publication,
6.32% to entry handling, and 6.04% to code stamps. These profiles precede the
direct input binding measurement. An earlier oversized `symbol.data` loses
four chunks; a short stripped-binary attachment has mostly unresolved symbols.
Neither supports precise attribution of the current hot path.

The direct-input profile `input.data` loses ten chunks (330 samples), so it
does not provide precise percentages. Repeating at 99 Hz with 4 KiB call
stacks produces `input-low.data`: 5,742 samples and zero lost samples. The
timed region contains only 201 samples. It attributes approximately 32% to
`run_captured`, 8% to scalar evaluation, 6% to input evaluation, and 4% each
to binding publication and native global writes. `run_captured` includes
inlined event and scalar dispatch. These coarse samples suggest residual
execution and per-block state traffic, rather than compilation, dominate the
warm region. The profiled mix runs at 15.10 MIPS; its 1,587,501 root trace
calls average about 19 guest instructions each. Further work must reduce
residual operations or block-boundary cost without reviving path explosion.

At this checkpoint, targeted release tests, all-target clippy, and the full
generation check pass. Generation verifies 81,607 Rust bytes and 2,085,721
compressed IR bytes unchanged. The fresh oracle gate passes all 133 programs
and 452,776 instructions: 4,392 native, 447,840 recording, and 544 slow exits.
Logs are `target/tcg/current-native-{0,1,2}.log`. Most architectural tests have
unique PCs, so their initial recording dominates this per-step gate; the batch
regressions and warm mix separately exercise native execution.
The workspace gate and pinned benchmark history still need repeating. The
realistic-mix target remains below 50 MIPS.

### Compact physical instructions

Packing scheduled scalar operations into 16-byte records reduces their former
64-byte stride. Operands use checked 32-bit physical indices; logical types,
widths, and full shift counts are retained. Constants and inputs remain bound
outside dispatch. Packed arithmetic tests cover wide values, signed overflow,
logical widths, oversized shifts, slices, and concatenation. Guard matching is
inlined across the IR/runtime crate boundary.

The first pinned CPU-16 CoreMark-100 trial takes 1.543990 seconds, or 19.713389
MIPS, with the same 30,437,270 instructions, CRC checks, native/training counts,
and zero evictions. Warmup takes 46.471776 seconds. This is an exploratory
single-trial improvement over the 17.355898 MIPS input-binding measurement.
It does not meet the mix target.

Packed dispatch passes targeted release tests, all-target clippy, and a fresh
133/133 oracle comparison over 452,776 instructions, including 4,392 native
instructions. Logs are `target/tcg/packed-native-{0,1,2}.log`. The container
generation check also passes with unchanged artifacts. The workspace gate
and pinned benchmark-history medians remain required before completion.

`Riscv64::trace_profile` reports retained root-trace call counts and residual
sizes without a counter increment beyond the existing promotion counter.
`VMLAB_TCG_PROFILE=1` prints the 30 most called roots in the micro-benchmark.
Counts include warmup and saturate at `u32::MAX`; residual sizes describe the
trained path, not work completed before an alternate exit. The census finds
a hot 128-instruction trace with 408 scalar operations and 424 guards. Its
loaded pointers regain a full 64-bit range and retain generated alignment,
address-width, and protection checks. Guarded specialization of those values
is the next experiment; none of those checks may simply be discarded.

### Rejected load-result speculation

Guarding every observed RAM-valued 64-bit load result regresses the mix to
0.780465 MIPS: 38.998913 seconds timed after 148.158178 seconds of warmup.
It introduces 286,550 slow continuations and executes 271,658,313 source IR
operations in the timed pass. CRC checks still pass, and the targeted
continuation test confirms a failed hint never repeats the completed read.
The optimization is reverted. A loaded number is not sufficient evidence of
address use. The continuing experiment derives candidate address arguments
through shared interprocedural IR data flow and guards them before generated
address checks. Memory-read results do not inherit their address's provenance.

### Address arguments and flat dispatch experiments

Shared argument-flow analysis follows assignments, helper return values, and
calls into physical memory address operands. It selects speculation sites;
runtime guards still prove numeric bounds, and generated protection checks
remain authoritative. It does not treat returned memory data as a copy of
its address. Address hints apply only to already evaluated identifier operands.
No expression or completed effect is evaluated again to obtain a hint.

| Change | CoreMark-100 seconds | MIPS |
| --- | ---: | ---: |
| Address-use hints with separate lower and upper guards | 1.693709 | 17.970781 |
| One modular interval comparison for bitvector addresses | 1.430124 | 21.282965 |
| Flat stream of packed scalar operations and guards | 1.297510 | 23.458223 |

Each trial retains CRC checks and the same native, training, and slow counts;
none evicts a cache entry. These are single CPU-16 trials, not noise-controlled
medians. Argument analysis increases initialization from about 0.2 seconds to
about 1 second. Flat-stream warmup takes 45.980667 seconds. A failed guard
retains the original event's source continuation and retired prefix. Stores
that hit an observation address switch to the full checkpoint plan immediately.
The inclusive interval comparison and its backward range refinement are tested
across all eight-bit inputs for endpoint and wraparound cases.

The longer flat-stream profile uses 1,000 iterations and 304,258,499 guest
instructions. Its ELF passes black-box QEMU userspace CRC checks. Execution
takes 10.897075 seconds (27.921116 MIPS), after 65.455994 seconds of warmup.
`stream.data` contains 7,674 samples with none lost. The warm slice attributes
34.37% to `run_captured` including inlined execution, 10.18% to binding
publication, 8.60% to scalar evaluation, 6.14% to input binding, 6.13% to native
global writes, 5.56% to input projection, and 4.04% to named native reads.
Aggregate mutation and updates account for another 2.48% and 1.76%. This shifts
the next investigation toward block-boundary state traffic. The pointer-chain
regression compares every Sail global, retirement, and error outcomes for
aligned, misaligned, and unmapped pointers, requiring exactly one first load.

This flat-stream checkpoint passes targeted release tests, all-target clippy,
and a fresh 133/133 lockstep run over 452,776 instructions, including 4,392
native instructions. Logs are `target/tcg/stream-native-{0,1,2}.log`. The full
container-backed generation check verifies unchanged artifacts. Later MIP,
publication-plan, and promotion experiments are outside that gate.

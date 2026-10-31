# Interpreter performance

## Result and target

The 50 MIPS target is not met. The pruned interpreter measured 0.009066 MIPS
on the scalar loop and 0.002422 MIPS on the CoreMark algorithm mix. These
measurements do not establish that Linux can boot in minutes.

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

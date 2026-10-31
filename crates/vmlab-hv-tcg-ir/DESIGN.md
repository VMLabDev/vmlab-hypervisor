# Sail-derived emulation IR

## Why keep an executable model in the generated crate?

The interpreter and the future trace compiler need the same instruction
semantics. `vmlab-hv-tcg-emit` parses the complete textual Jib model and lowers
it into `Program`. It commits that program as deterministic, gzip-compressed
JSON beside generated Rust decoder accessors and instruction builders.

Rust compiles the small accessors and embeds the compressed data. On first
use, `model()` decompresses and deserializes the IR once through `OnceLock`.
No Sail compiler, OCaml runtime, network access, or code generation runs during
a normal build. Generation is a separate operation.

The compressed representation keeps rustc from compiling thousands of Rust
functions containing expanded model operations. It also preserves the entire
model for diagnosis. This choice makes builds inexpensive, but it does not
make interpreted execution fast.

## Types and register layout

`Type` retains Sail's logical types. `Storage` records the representation
available to a native consumer:

| Sail type | Representation |
| --- | --- |
| `%unit` | No stored value |
| `%bool`, `%bit` | An 8-bit host value with one significant bit |
| `%bvN`, N at most 128 | The smallest of u8, u16, u32, u64, or u128 that fits |
| `%iN`, N at most 128 | The corresponding signed host integer |
| `%i` | Arbitrary-precision signed integer |
| `%bv` or wider fixed bitvectors | A width and an arbitrary-precision unsigned integer |
| Enum, struct, union, vector, list | Explicit typed aggregate |
| Reference | An explicit architectural location |

`Register.index` assigns architectural slots in declaration order.
The reference interpreter currently uses names and boxed values in maps;
it does not claim to implement the native register-array layout.
Function parameters, return values, and declared locals retain their types.
`Program.declarations` also retains signatures for platform operations.

The parser accepts real and native floating-point types. The pinned RISC-V
model represents architectural floating point as bitvectors and calls
SoftFloat. The runtime does not implement a separate Sail real-number engine.

## Specialisation

`specialize` propagates literal constants within basic blocks. It clears facts
at branch targets and calls, because a call may mutate architectural state or
a referenced value. Constant folding uses checked integer arithmetic. A
literal's textual spelling is not used to infer that two bitvectors differ.

`remove_dead_paths` follows reachable edges, selects constant branches, and
rebuilds all branch targets after removing unreachable instructions.
`specialize_constructor` selects an instruction constructor through Sail's
`zexecute` dispatcher. Jib's `is` expression is a constructor mismatch test;
it skips the body of a nonmatching case.

`lower_bitvectors` infers fixed widths within basic blocks. It lowers
width-preserving arithmetic, Boolean operations, and shifts to
`Target::Native` with a `NativeOperation` and an explicit width.
The interpreter executes those operations with u128 arithmetic and masks the
result to the logical width. Unknown widths retain the generic operation.
Signed additions reduce modulo the result width. Oversized shifts do not
use Rust's masked shift count.

`specialize_argument` binds an immutable scalar parameter to a literal. It
rejects assignments to that parameter and references that could mutate it.
Pure comparisons of literals are folded before dead-path removal. The
runtime uses this pass to cache a generated CSR accessor for each CSR ID,
so lockstep observation does not repeatedly interpret the complete CSR
dispatch chain. Tests compare these accessors with the unspecialised model.

The reference value container still uses `BigInt` and `BigUint`. Native
operations convert bounded operands to u128; this is a correctness-oriented
representation, with allocation costs that the trace compiler must avoid.

## Control flow and exceptions

`Function.code` has explicit branches, jumps, returns, and model-failure
terminators. Targets index operations, not source lines. Initializers have
implicit fallthrough in Jib; lowering appends an explicit return.
An `Arbitrary` terminator returns an undefined value so the caller can still
test Sail's exception flag and execute its catch path.

Guest traps remain Sail operations. Its `ExecutionResult` and exception
unions carry the cause, privilege, and faulting address. Generated trap
handlers update CSRs, privilege, and the next PC. Internal Sail exceptions
use `have_exception` and `current_exception`, preserving the model's own
catch paths. `Op::Raise` reports an uncaught model failure; it is not a
substitute for guest trap entry. Runtime failures include the function and
IR operation index.

## Memory, translation, and ordering

`Target::Memory` distinguishes reads, writes, instruction fetch, and exclusive
accesses. The model performs PMP and PMA checks, Sv39 and Sv48 page walks,
permission checks, A/D handling, and TLB management before reaching the
physical `Memory` trait. The runtime does not maintain a competing handwritten
page walker.

`FlatMemory` checks all ranges and increments its write generation.
`SharedMemory` serializes physical accesses. `load_reserved` captures a
generation with the read; `store_conditional` checks it and writes under one
lock. Reservations belong to each interpreter. The later Sail reservation
callback must not refresh the generation captured by the exclusive read.
Any intervening shared-memory write can conservatively invalidate a reservation.

Generated AMO and shadow-stack swap sequences hold a reentrant memory
transaction lock across their read-modify-write operations. Page walks hold
the same lock across PTE checks and A/D updates. Ordinary RAM accesses also
take that lock. Other interpreter computation can proceed on separate host
threads. Sail barriers execute a host sequentially consistent fence.

The physical trait is also the MMIO integration point for the VMM.
An unmapped physical access currently returns a host-side `Error::Memory`;
the platform must configure PMA ranges consistently with its RAM and MMIO map.

## Floating point

The runtime vendors Berkeley SoftFloat 3e with its RISC-V specialization.
The build generates a C ABI wrapper for the Sail declarations. It returns
the result bits and IEEE exception flags in the model's typed tuple.
One mutex protects SoftFloat's rounding mode and exception flags across
host threads. No host-FPU shortcut is used.

## Caches and the JIT consumer

The decoded cache contains single-instruction blocks keyed by physical fetch
address, memory write generation, instruction width and bits, and decoder
context. A call-graph walk derives that context from every global read or
written by the generated decoders and their callees. This includes privilege,
misa, FP/vector enable state, vector type, and exception bookkeeping.
Architectural state can change a decode without changing instruction memory.

Decoders with memory or external effects are not memoized. A decode that
changes its context or the memory generation is not cached either. The cache
is bounded to 4096 entries and is cleared when full. Constructor-specialised
instruction bodies are cached separately because their semantics do not
depend on guest memory.

`effects` computes transitive global reads, writes, and external effects to
a fixed point, including recursive call graphs. The runtime memoizes calls
only when every read and written global retains its version across the call.
Memory, references, unknown calls, barriers, and platform effects are excluded.
Versioned `Globals` also track mutations by platform and lockstep callers.
This avoids caching an input-state result as if it described a changed output
state. The cache bounds each function to 128 argument entries.

`indexed` lowers local names to slots and extracts a literal pool. Its branch
indices match the original IR, and it retains global names and explicit
reference destinations. The interpreter binds the literals once, executes
local frames as vectors, and borrows aggregate fields before copying their
leaf values. This form is shared compiler input; it does not yet perform
interprocedural partial evaluation or native scalar register allocation.

A trace compiler can consume the same `Function`, `Op`, `NativeOperation`,
and typed register slots. It must convert the explicit control-flow graph
to SSA, keep guest registers in host values within a trace, and lower memory
operations through the physical-memory and generated-translation contract.
State must be committed at exits that can trap. Neither a Cranelift backend
nor hot-trace scheduling is part of this emitter package.

The generic interpreter is the reference for those transformations.
The optional `lockstep` feature implements `StepCpu` for
`Riscv64<LockstepMemory>`. Reset replaces memory and CPU state, restores raw
FP bits without dirtying FS, and clears reservations and write logs. Each
step drains its ordered physical-memory writes. The adapter samples the
CSR set supplied in its initial `State`. The `sail` feature enables the
independent oracle driver in `examples/lockstep.rs`.

The deterministic step schedule calls generated `try_step` with forced
wait completion disabled, then calls generated `tick_clock`. This matches
the oracle's instruction-count clock rather than the host wall clock.

## Whole-block specialization contract

Why does removing more than 23,000 dead locals still leave only about
0.01 MIPS? The remaining path interprets roughly 1,647 model operations per
guest instruction. Most describe generic model machinery rather than the
guest's scalar work. `../vmlab-hv-tcg/PERF.md` records the measurements.
The optional `Riscv64::specialize` path implements whole-block recording and
native scalar interpretation. It assumes familiarity with `Function`, `Op`, and the
physical-memory contract above.

### Record generated execution and residualize its dynamic work

The named interpreter supplies the concrete execution while a recorder
tracks symbolic values beside its frames. Recording disables memoization,
indexed execution, and decoder memoization so every generated call and
architectural effect is visible. Constants, decoded instruction operands,
and guarded configuration values fold through all calls. GPRs, FPRs,
architectural counters, and data-memory results remain dynamic.

The residual representation has two parts:

- A scalar SSA DAG over bools, bounded integers, and bitvectors up to 128 bits.
- An ordered sequence of guards, physical-memory effects, and guest-instruction
  checkpoints. Each guard has an exact source continuation.

The `native` module supplies the scalar DAG. Range checks reject unsafe
unbounded-integer lowering. Its tests show that repeated tick additions can
combine and that byte slicing followed by concatenation can become the
original scalar again (`src/native.rs`, tests `repeated_ticks_collapse_to_one_add`,
`byte_unpack_and_repack_disappears`, and `unbounded_integer_overflow_is_not_silently_lowered`).
The JIT will consume the same residual operations; it must not rediscover
RISC-V semantics in a second instruction implementation.

Pure scalar work is scheduled only when a guard, memory event, or final
output needs it. Values needed only for a cold exit stay as DAG expressions.
This lets three counter updates combine on a complete block while retaining
the intermediate value needed to stop after its first instruction.

### A failed guard resumes; it does not restart an instruction

The named executor can resume at a specified operation with materialized
locals through `continuation`. A snapshot retains the active
Sail frames, local bindings, global writes so far, pending caller destinations,
and the next source operation. It refers to immutable function definitions.

If a guard fails, the runtime materializes that snapshot, resumes the deepest
frame, assigns its result to its caller, and continues outward. The root is
the same generated `try_step` followed by generated `tick_clock` used by
`Riscv64::step`. No host opcode implementation
replaces those functions.

Contrast two cases. Before a RAM store, restarting the current operation
has not duplicated a write. After a RAM store, restarting the instruction
could duplicate MMIO, change LR/SC state, and repeat an ordered write.
Therefore an exit after a completed memory event resumes after that event;
its result is captured and never fetched or written again. Unsupported
operations exit before the operation and use the working Sail interpreter.
These are precise slow continuations, not fabricated results.

Prediction: if a branch guard fails after a store, does the oracle observe
two writes? It must observe one. The store is already complete and the
continuation starts at the alternate branch destination. A regression must
check both the final state and the ordered write log, because final RAM
contents alone would miss a duplicated write.

### Keep the hot state scalar without losing model state

A batch executes against native scalar slots. It loads dynamic inputs once,
keeps intermediate register and counter values unboxed, and commits state at
an observation or exit. Per-step lockstep observation remains supported by
instruction checkpoints. A checkpoint can materialize values lazily without
requiring those computations on an uninterrupted batch path.

Wide RVFI packets and other aggregates remain symbolic recipes. Publication
updates their existing model containers in place and reuses bignum capacity.
Scalar globals remain in native slots across blocks inside one API call;
return, error, or source execution publishes pending values. No recipe points
at another block's reusable scratch storage.

Per-step execution records at most one instruction and publishes every model
value before returning to its observer. Batched execution records bounded
blocks and uses the same residual graph without publishing every intermediate
checkpoint. It does not retain a partially executed graph between API calls.

### Guard configuration and instruction memory

An entry is specialized for its PC, decoded instruction stream, and observed
mode/configuration values. Context guards compare versioned globals; a changed
version may be refreshed only after checking that the expected value is still
equal. Dynamic register/counter inputs also have type and range constraints.
An input that does not satisfy them takes a correct slow path.

Only memory that explicitly promises side-effect-free instruction fetch may
replace a fetch with a code-generation guard. The default memory behavior
must remain conservative. RAM implementations need page generations so a
data-page store does not invalidate every decoded block. A write that changes
the current block's code must exit after the completed write and finish the
current instruction through its continuation. MMIO fetches keep their read
and guard the returned instruction bits.

Shared-memory block execution uses the memory transaction contract so code
and PTE checks do not race other harts. Barriers, reservations, and unsupported
platform operations retain their generated ordering or exit before the
operation. Range, code, and context guards are correctness requirements;
benchmark speed never permits dropping them.

### Extend common guard alternatives instead of repeatedly deoptimizing

A loop's final conditional branch changes outcome once per traversal. A
full Sail continuation on every such change would cap mixed-workload speed.
After a cold branch exit, the recorder can specialize that source continuation
as an alternative fragment. It shares the parent SSA values and memory-event
results, so entering it does not repeat the prefix or copy whole Sail frames.
The guarded graph grows into a bounded control-flow graph. Unsupported host
operations and mode transitions can remain precise slow exits.

Recording, snapshot size, and cache memory require explicit bounds. Hitting
one stops recording at a source continuation and executes the remaining
instruction normally. It must never discard state or report an unexecuted
instruction as retired.

### Evidence required before enabling the whole-block path

The scalar DAG tests establish local algebra only. Runtime regression tests
must also prove exact mid-instruction continuation, memory-effect ordering,
code invalidation, counter ticks, mode transitions, and both step and batch
execution. The 133-program Sail comparison must exercise native instructions,
not merely succeed while all instructions use the recorder. Batch-vs-step
comparisons and native/slow instruction counts are required alongside the
existing 134 flat-image tests and generation check. Throughput measurements
must distinguish one-time specialization from warm block execution and must
retain the CRC and final-state checks.

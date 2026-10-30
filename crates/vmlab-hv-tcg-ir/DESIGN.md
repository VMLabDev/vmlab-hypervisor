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
address, memory write generation, and instruction bits. It is bounded to
4096 entries. A write makes old entries unreachable; the cache is cleared
when full. Constructor-specialised instruction bodies are cached separately
because their semantics do not depend on guest memory.

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

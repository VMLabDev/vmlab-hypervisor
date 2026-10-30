# Run instruction lockstep

`StepCpu` compares two independent CPUs after every instruction. `run` resets
both from the supplied `State` and `Memory`, checks the reset results, and
reports the first difference. Diagnostics include the instruction bytes, the
pre-step PC, and changed registers, privilege, CSRs, or memory writes.

## Build the Sail adapter

Use Linux with glibc, Python 3, C/C++ compilers, GMP, zlib, and a configured
sail-riscv CMake build with `compile_commands.json`. Generate the model with
Sail 0.20.3 and sail-riscv at `fc074f9`, as recorded in the phase-3 specification's
`notes/01-sail-toolchain.md`, full model commit
`fc074f99f578954376214c4ca9490152d35d0ec2`. The helper uses the configured build's compiler
flags and sources. It does not change that checkout.

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  python crates/vmlab-hv-lockstep/scripts/build-oracle.py \
  /path/to/sail-riscv/build target/tcg/lockstep-oracle
export VMLAB_SAIL_ORACLE="$PWD/target/tcg/lockstep-oracle/libvmlab_sail_oracle.so"
export VMLAB_SAIL_CONFIG=/path/to/sail-riscv/build/config/rv64d_v128_e64.json
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  cargo test -p vmlab-hv-lockstep --features sail -- --test-threads=1
```

Expected results include oracle self-comparison over 4,096 seeded instructions,
reset, integer arithmetic, memory callbacks, a taken branch, compressed
instructions, and SoftFloat. Default Cargo
tests exercise the runner without the external toolchain. Enabling `sail`
requires both environment variables; missing prerequisites fail explicitly.

Each oracle uses a separate `dlmopen` namespace because Sail and SoftFloat have
global state. Reuse a pair with `reset` across a corpus. Repeatedly opening new
pairs can exhaust glibc's static TLS allocation even after `dlclose`.

The adapter links upstream sources without copying them into this crate.
Generated Sail/model sources retain their upstream BSD notices. Sail runtime
and sail-riscv are BSD-2-Clause; Berkeley SoftFloat is BSD-3-Clause. Their
licence files must accompany any distributed adapter binary. The handwritten
adapter and Rust code are MIT. No QEMU implementation source is used.

## Build and run the corpora

The fetch helper pins riscv-tests and riscv-arch-test by full Git commit,
including riscv-tests' pinned environment submodule. It builds RV64I/M
physical-mode tests and writes a JSON manifest with source revisions.
It refuses dirty checkouts. Checkouts and binaries stay under `target/`.

```sh
python crates/vmlab-hv-lockstep/scripts/corpus.py target/tcg/corpus
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  cargo run -p vmlab-hv-lockstep --features sail --example corpus -- \
  target/tcg/corpus/bin/*.elf
```

The helper selects a RISC-V GCC or Clang with LLD. Both the pinned corpora build
with Clang's integrated assembler on this host. To use GNU assembler with
Clang, supply `--gnu-as /path/to/riscv64-elf-as`. Without a cross compiler, use `--fetch-only`;
the build command otherwise exits with the missing prerequisite.

The driver validates ELF64 little-endian RISC-V executables, loads PT_LOAD
segments into bounded RAM, zeroes BSS, and finds `tohost` in the symbol table.
It rejects virtual/physical relocation, overlapping segments, and missing
symbols. `tohost=1` means pass; other nonzero values fail. A test that does not
finish within one million instructions fails. The architectural-test platform
implements register assertions and writes a failure code on mismatch.
The example reuses two Sail instances for the whole corpus.

## Connect another CPU

Implement `StepCpu` in an adapter owned by the consumer. No runtime dependency
is needed in this crate. Supply a complete initial state, including the same
CSR set on both sides; `Sail::state` supplies its reset state. Register x0 must
be zero. `reset` must replace RAM and state and clear reservations, clocks, and
write logs. `step` executes one instruction and drains that instruction's
ordered physical-memory writes. Architectural traps are state transitions;
host/model failures return `CpuError`.

The current adapter compares 32 integer registers, 32 raw floating-point
registers, PC, privilege and virtualization, and the 27 RV64GC machine/supervisor
CSRs listed in `CORE_CSRS`. It does not yet compare vector registers or all
extension-specific CSRs. Reset rejects CSR IDs outside the supported set.

`random::stream(seed, count)` emits valid RV64I LUI, AUIPC, ADDI, OP, and shift
encodings. Streams have no memory accesses or branches and reproduce exactly
from the seed. Memory and control-flow coverage comes from the compiled
corpora and directed oracle tests.

The vmlab-hv-tcg adapter and its interpreter comparison require the emitter
package to land. No substitute interpreter or passing placeholder test is
included. The toolchain's `tcg-generate-check` recipe is also external to this
work package.

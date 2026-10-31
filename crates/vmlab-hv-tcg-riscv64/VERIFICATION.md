# RISC-V emitter verification

## Inputs

- Sail 0.20.3.
- isla e9b5d945394277656593a0d429466d7fa0a2b4b3.
- sail-riscv fc074f99f578954376214c4ca9490152d35d0ec2.
- Configuration `rv64d_v128_e64.json`, VLEN 128 and ELEN 64.
- riscv-tests 3df703cd52d5f0d8224e3c50bf03c6cb70efff98.
- riscv-arch-test eb66181dd27ff7847e2c3a010705b13490b0bf75.

The local generation used the research toolchain with these flags:

```text
isla-sail -dno_cast -O -Oconstant_fold -memo_z3 -c_no_main
  --isla-preserve encdec_forwards
  --project model/riscv.sail_project --all-modules
  --config build/config/rv64d_v128_e64.json -o target/tcg/riscv64
```

Both `just tcg-generate` and `just tcg-generate-check` first run the toolchain
package's `tcg-ir-generate` recipe. The former emits Rust builders and
compressed IR; the latter compares both committed artifacts without rewriting
them. Both accept an optional input path. The complete container-backed
`just tcg-generate-check` passed with byte-identical artifacts.

The generated checksum covers executable IR, excluding Jib's debug file paths
and source-location IDs. This avoids a difference between a container's
absolute source paths and a local checkout's paths.

## Parser and generated artifact

The research input contains 14,135,859 bytes; the pinned container input
contains 14,135,209 bytes. Both contain 5,734 definitions and produce identical
executable artifacts despite their different debug paths. The parser
round-trips its bytes, emits canonical Jib from the AST independently of the
original source, and reparses both forms to the same AST:

```sh
cargo run -p vmlab-hv-sail-ir --example roundtrip -- target/tcg/riscv64.ir
```

The first libFuzzer run completed 155,975 executions in 61 seconds without a
crash. The fuzz target checks successful parses for lossless round trips and
AST equality after reparsing. Parser tests also check malformed branches,
types, literals, strings, and bounded recursive nesting.
After the nesting guards were finalized, a second run completed 51,562
executions in 75 seconds without a crash.
The final fuzz target also checks canonical AST emission. It completed
108,601 executions in 31 seconds without a crash.

The generated crate has 361 instruction builders and includes 1,986 model
functions, 214 registers, and 285 initialization functions. It contains
81,607 bytes of generated Rust and 2,085,721 bytes of compressed IR.
The uncompressed JSON IR is 21,876,036 bytes. Measure with `wc -c` and
decompress `src/model.json.gz` to count the JSON bytes.

A clean release build of this generated crate and its dependencies took
3.263 seconds wall time on the development host. The measurement used a new
target directory and Bash's `time` around:

```sh
cargo build --release -p vmlab-hv-tcg-riscv64 --target-dir "$build_dir"
```

## Architectural tests

All 134 flat-image tests passed:

| Family | Passed |
| --- | ---: |
| rv64ui | 54 |
| rv64um | 13 |
| rv64ua | 19 |
| rv64uf | 11 |
| rv64ud | 12 |
| rv64uc | 1 |
| rv64si | 7 |
| rv64mi | 17 |

The runner compiles each upstream `.S` test with Clang targeting RV64GC,
links its `env/p` executable, converts it to a flat binary, and runs the
generated interpreter until the test writes `tohost`. A value of one is a
pass; another nonzero value is a failure. Each run has a wall-clock timeout,
an instruction limit, and a 24 GiB systemd memory cap with swap disabled.
Only one test image runs at a time.

LLVM rejects the upstream environment's `.weak` followed by `.global`
declarations. The runner defines the assembly-preprocessor identifiers
`global` and `globl` as `weak`. These are standalone, single-object images;
they do not need strong exported symbols. This adjustment changes linkage
directives and preserves the test's instruction logic. The first unadjusted
run had 22 assembly failures, not emulator failures.

Reproduce from the workspace root after cloning riscv-tests and its env
submodule under `target/tcg/riscv-tests`:

```sh
cargo build --release -p vmlab-hv-tcg --examples
python3 crates/vmlab-hv-tcg/scripts/riscv-tests.py target/tcg/riscv-tests
```

The runner writes individual logs and `results.json` under
`target/tcg/test-results`. A missing compiler or assembly error is reported
as a build failure, never as a skip or pass.

Generated-path tests separately prove instruction fetch and execution,
compressed PC advancement, memory operations, machine ecall trap entry,
Sv39 and Sv48 walks, a user fetch permission fault, SoftFloat flags and
canonical NaNs, self-modifying-code invalidation, cross-hart LR/SC
invalidation, and simultaneous AMO increments. The AMO test reproduced
lost updates before the transaction lock was added.

## Interpreter speed and remaining precision costs

The release interpreter executed a 3,000-instruction scalar loop in
7.027901 seconds, or 0.000427 MIPS. The loop contains an increment, a
decrement, and a backward conditional branch. Timing excludes model
initialization and includes fetch, decode, generated interrupt and retirement
logic, and the interpreter's dispatch. It executed 30,250,295 IR operations.

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  target/release/examples/loop 1000
```

This result does not meet a TCG performance gate. The reference interpreter
uses maps, boxed aggregates, and generic big integers. In the generated
model, 414 call sites lower to fixed-width native operations; the remaining
unknown-width operations keep their generic implementations.

Arbitrary precision remains necessary for the unspecialised `%i` operations
and wide RVFI packet aggregates, including the initialization concatenations
that exceed 128 bits. It is provided by `num-bigint`, without GMP. This is
not evidence that every such operation needs arbitrary precision after a
stronger whole-program specialisation pass.

## Scope of the evidence

These results establish the listed interpreter and parser tests. They do not
establish a Linux TCG boot, VMM integration, or performance parity. Those
checks belong to the machine and JIT work packages. No S1 TCG template boot
is claimed.

After merging the lockstep, GPU, toolchain, benchmark, and display packages
from local main, `just ci::check` passed with `RUST_TEST_THREADS=1` inside the
24 GiB memory cap. This runs formatting, workspace Clippy with warnings denied,
tests, and doctests. The existing slow 1 GiB disk-throughput test remains
ignored. An earlier gate timed out in the Ubuntu AHCI agent setup; the exact
isolated test passed in 36.78 seconds, and it also passed in the final full
gate without an AHCI change.

`just bench-check` passed with only one QEMU baseline entry per group. There
was no comparable TCG benchmark entry, so that result does not establish a
performance regression bound.

## Independent Sail lockstep

After merging the lockstep package from local main, the runtime's optional
adapter passed all 131 RV64I/M corpus programs against the independently
compiled Sail C++ model: 67 from `riscv-tests` and 64 from `riscv-arch-test`.
Two additional `riscv-tests` programs, FADD.S and FADD.D, also matched.
The final cache-fixed runtime matched all 133 programs across 450,726
instructions. Comparison checks integer and raw FP registers,
privilege, virtualization, the supplied CSR set, and each instruction's
ordered physical-memory writes. The oracle and corpus are built locally
under `target/tcg/lockstep-oracle` and `target/tcg/lockstep-corpus` using the
merged lockstep package's build scripts.

The first divergence was instruction 1 of `rv64ui-add`, at PC 0x80000000,
instruction 0x0540006f. CSR 0xb00, `mcycle`, was zero in the interpreter and
one in Sail. The runtime omitted the model's clock tick. Calling generated
`tick_clock` after generated `try_step` fixed the same reproduction; the
complete 509-instruction test then matched and reached `tohost=1`.

The initial architectural-test attempt exhausted the runtime's cumulative
IR-operation guard because every observation also executes generated CSR
accessors. The adapter now budgets each state snapshot and step separately.
CSR accessors are also specialised by immutable CSR ID, with a test against
their unspecialised generated twins. The 24 GiB host memory cap was unchanged.

```sh
VMLAB_SAIL_ORACLE="$PWD/target/tcg/lockstep-oracle/libvmlab_sail_oracle.so" \
VMLAB_SAIL_CONFIG=/path/to/rv64d_v128_e64.json \
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  cargo run --release -p vmlab-hv-tcg --features sail --example lockstep -- \
  target/tcg/lockstep-corpus/bin/*.elf
```

The generated model has more extensions than RV64GC. `MODEL_ISA.md` records
its enabled configuration with FP and vector state enabled. That list is not
an advertisement that every extension has been validated. In particular,
the host entropy operation needed by Zkr is not supplied by `FlatMemory`.
An attempted call fails explicitly rather than supplying fabricated entropy.

A decoder-cache regression test first executes FADD.D with FS disabled, then
enables FS and retries the same physical instruction without modifying RAM.
It reproduced an incorrect cached illegal decode, leaving f3 zero instead
of 3.75. The cache now keys on context derived from the generated decoder's
global dependencies. The same reproduction passes, and a dependency test
ensures that the pinned model uses the cache while tracking its CSR inputs.

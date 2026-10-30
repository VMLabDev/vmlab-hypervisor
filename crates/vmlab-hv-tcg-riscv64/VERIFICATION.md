# RISC-V emitter verification

## Inputs

- Sail 0.20.3.
- isla e9b5d945394277656593a0d429466d7fa0a2b4b3.
- sail-riscv fc074f99f578954376214c4ca9490152d35d0ec2.
- Configuration `rv64d_v128_e64.json`, VLEN 128 and ELEN 64.
- riscv-tests 3df703cd52d5f0d8224e3c50bf03c6cb70efff98.

The local generation used the research toolchain with these flags:

```text
isla-sail -dno_cast -O -Oconstant_fold -memo_z3 -c_no_main
  --isla-preserve encdec_forwards
  --project model/riscv.sail_project --all-modules
  --config build/config/rv64d_v128_e64.json -o target/tcg/riscv64
```

The toolchain package owns `just tcg-ir-generate`. After that recipe emits
`target/tcg/riscv64.ir`, this package's `just tcg-generate` emits the Rust
builders and compressed IR. `just tcg-generate-check` compares both artifacts
without rewriting them. Both accept an optional input path.

The generated checksum covers executable IR, excluding Jib's debug file paths
and source-location IDs. This avoids a difference between a container's
absolute source paths and a local checkout's paths.

## Parser and generated artifact

The full input contains 14,135,859 bytes and 5,734 definitions. The parser
round-trips its bytes and reparses the resulting document to the same AST:

```sh
cargo run -p vmlab-hv-sail-ir --example roundtrip -- target/tcg/riscv64.ir
```

The first libFuzzer run completed 155,975 executions in 61 seconds without a
crash. The fuzz target checks successful parses for lossless round trips and
AST equality after reparsing. Parser tests also check malformed branches,
types, literals, strings, and bounded recursive nesting.
After the nesting guards were finalized, a second run completed 51,562
executions in 75 seconds without a crash.

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
6.923160 seconds, or 0.000433 MIPS. The loop contains an increment, a
decrement, and a backward conditional branch. Timing excludes model
initialization and includes fetch, decode, generated interrupt and retirement
logic, and the interpreter's dispatch. It executed 30,034,295 IR operations.

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
establish independent Sail lockstep, riscv-arch-test coverage, a Linux boot,
VMM integration, or performance parity. Those checks belong to the separate
lockstep, machine, and JIT work packages. No S1 template boot is claimed.

The generated model has more extensions than RV64GC. `MODEL_ISA.md` records
its enabled configuration with FP and vector state enabled. That list is not
an advertisement that every extension has been validated. In particular,
the host entropy operation needed by Zkr is not supplied by `FlatMemory`.
An attempted call fails explicitly rather than supplying fabricated entropy.

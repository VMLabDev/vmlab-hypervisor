# Sail toolchain (`toolchain/sail`)

The pinned, containerised toolchain for phase 3's TCG pipeline. It produces
two artefacts the other phase-3 crates consume:

1. The **textual Jib IR** of the sail-riscv model (`just tcg-ir-generate`
   → `target/tcg/riscv64.ir`), the input to the `vmlab-hv-tcg-emit`
   generator.
2. The **lockstep oracle** (`just tcg-oracle-build` →
   `target/tcg/oracle/`): the same model compiled by Sail's C++ backend,
   wrapped in a C ABI that `vmlab-hv-lockstep` links in-process.

Everything runs inside a Docker image built from `Containerfile`, with
every network input pinned in `pins.env` (base image digest, opam binary
sha256, opam repository commit, Sail version, isla and sail-riscv
commits, and the sha256 of the Sail runtime's `rts.c` that our patch
applies to). Recipes run containers as the invoking user with a 12 GiB
memory cap, and containers write nothing outside the mounted `target/`
directory.

## Layout

- `Containerfile` — the toolchain image: Debian 12, opam 2.5.1, an
  OCaml 5.2.1 switch, sail 0.20.3, the isla-sail plugin at the pinned
  isla commit, sail-riscv at the pinned commit, and a pre-configured,
  pre-built C++ model tree in `/work/build`.
- `pins.env` — the single source of truth for versions and hashes. The
  `tcg-image-build` recipe turns it into `docker build` args; edit it to
  bump a pin.
- `patches/rts-weak-emulator-memory.patch` — marks the five
  `emulator_{read,write}_mem*` functions in the installed Sail runtime
  weak, so the oracle's strong definitions (host memory callbacks) win
  at link time. Content-checked against `SAIL_RTS_C_SHA256` before
  applying.
- `oracle/oracle.{h,cpp}` — the oracle wrapper (see below).
- `scripts/gen-ir.sh`, `scripts/build-oracle.sh` — run *inside* the
  container; they never touch the host except through `/out` (the
  recipe's mount).
- `oracle-test/` — a standalone Rust crate (outside the workspace) that
  links `liboracle.a` and steps a hand-assembled RV64 program; the proof
  that the library is linkable and architecturally alive.

## Recipes

| Recipe | Effect |
|---|---|
| `just tcg-ir-generate` | Emit `target/tcg/riscv64.ir` (+ `.sha256`, `.versions.txt`, `.log`). Deterministic: two runs are byte-identical (`just tcg-ir-repro-check` proves it). |
| `just tcg-ir-repro-check` | Generate twice into scratch directories and `cmp`. |
| `just tcg-oracle-build` | Build `target/tcg/oracle/{liboracle.a, oracle.h, riscv64-config.json, oracle.versions.txt}`. |
| `just tcg-oracle-test` | `tcg-oracle-build` + `cargo test` in `oracle-test/`. |

The IR flags are the ones measured in the phase-3 research
(notes/01): `-dno_cast -O -Oconstant_fold -memo_z3 -c_no_main
--isla-preserve encdec_forwards`, with `--project
model/riscv.sail_project --all-modules --config <rv64d_v128_e64.json>`.
`encdec_forwards` keeps the decoder functions so the emitter can
dispatch on decode. The same config file drives the oracle build, so the
IR and the oracle describe the same machine: RV64GC + H + V, VLEN 128,
ELEN 64.

Determinism: each generation runs in a fresh scratch directory (the z3
memo cache must not leak between runs) and three consecutive runs on
this host produced sha256 `19eaf0b0…` (14,135,209 bytes, 357,664 lines).

## The oracle C API surface

`oracle.h` is the contract; `liboracle.a` is self-contained apart from
`-lgmp -lz -lstdc++`. One oracle wraps one hart; at most one may exist
per process because the Sail runtime's memory seam is process-global.
The lockstep harness runs one comparison per process (or sequentially).

**Lifecycle**

- `oracle_create(config_json_path, reset_pc)` → instance at reset state:
  registers at reset values, `x10 = mhartid`, `x11 = dtb address`
  (the model's `init_boot_requirements`), PC at `reset_pc`. The config
  must be the pinned `riscv64-config.json` (copied beside the library).
- `oracle_set_mem_callbacks(o, read, write, ctx)` — required before the
  first step. The oracle holds no RAM: every instruction fetch, load,
  store, and page-table walk is served by the host, byte-wise
  little-endian, with an access kind (`ORACLE_MEM_READ`, `_IFETCH`,
  `_READ_EXCL`, `_WRITE`, `_WRITE_EXCL`). A failed *read* yields
  architecturally-safe 0xff fill for data (a failed *fetch* aborts — a
  broken memory image should be loud). A failed *write* makes the model
  raise an access-fault exception.
  **PMA note:** the model checks physical memory attributes before any
  callback fires. The pinned config maps RAM at `0x80000000..0x100000000`
  (plus a small ROM at `0x1000` and a region at `0x02000000`; see
  `memory.regions` in the config). Code or data outside those regions
  faults without reaching the host — place test programs in RAM.
- `oracle_set_observers(o, trap, term, ctx)` — optional: per-trap
  notification (`is_interrupt`, mcause-style cause) and terminal output.
- `oracle_destroy(o)`.

**Stepping and time**

- `oracle_step(o)` → `ORACLE_OK` (one instruction retired), `ORACLE_TRAP`
  (an architectural exception or interrupt was taken this step; the step
  still completes — PC is at the handler; read the cause with
  `oracle_trap_cause`), `ORACLE_WAITING` (the hart entered wfi/wrs and
  nothing retired; the next step force-exits the wait and retires the
  instruction, mirroring `sail_riscv_sim`'s `exit_wait=true`), or
  `ORACLE_ERR`.
- `oracle_tick_clock(o)` — advances model time by one tick (mcycle,
  minstret, and the built-in CLINT's mtime/mtimecmp comparison). The
  harness owns the tick policy; the oracle does not advance time behind
  your back, so runs are reproducible.

**Architectural state**

- `oracle_get/set_pc`, `oracle_get/set_x` (x0 reads 0, writes ignored),
  `oracle_get/set_f` (raw flen-bit pattern; flen follows the config).
- `oracle_get/set_csr(addr, …)` — raw read/write through the model's own
  `read_CSR`/`write_CSR` clauses (access checked as Machine-mode), so
  WARL field behaviour and legality match the model exactly. CSRs the
  config does not implement return `ORACLE_ERR` rather than faulting.
- `oracle_csr_count` / `oracle_csr_at` — enumerate the readable CSR set
  in address order, for whole-architectural-state comparison after each
  instruction.
- `oracle_xlen`, `oracle_sail_version` (sail + model commit),
  `oracle_isa_string` (the model's canonical ISA string).

**Determinism** — entropy for the scalar-cryptography extension comes
from a fixed LCG (the emulator's `std::random_device` would make oracle
runs incomparable), and print/log callbacks are silenced (output would
otherwise go to the host process's stdout).

## Regenerating from a newer model

Bump `SAIL_RISCV_COMMIT` (and, if needed, `ISLA_COMMIT` /
`SAIL_VERSION` + `SAIL_RTS_C_SHA256`) in `pins.env`, then re-run the
recipes. The IR diff (`target/tcg/riscv64.ir`) is what a reviewer reads;
`tcg-ir-repro-check` guards that the new toolchain is itself
deterministic. Note that opam-repository rewrites its history
periodically — if the `OPAM_REPO_COMMIT` pin stops fetching ("not our
ref"), re-pin to the current master tip and record it; sail and OCaml
are exact-pinned so only transitive build deps can drift.

Licences: Sail, sail-riscv and isla are BSD-2-Clause (sail-arm is
BSD-3-Clause-Clear and unused here); see `THIRD_PARTY_NOTICES`. The
generated C++ model and the IR are derivative works of those models and
keep that notice.

# AArch64 phase-3 status

Work stopped on 2026-11-01 at the owner's request. Phase 3 is on hold for
later issues. This branch is incomplete and is not ready to merge.

## Current work

- `toolchain/sail/pins.env` pins https://github.com/rems-project/sail-arm
  at `7d0581642d0e3aaca367bbc5650aca7c53057543`, selecting `arm-v9.4-a`.
  The container fetches that revision. The complete BSD-3-Clause-Clear
  notice is in `toolchain/sail/LICENSE.sail-arm` and `THIRD_PARTY_NOTICES`.
  Owner decision D14 accepts those terms, assumes no patent grant, and
  prohibits extracting ASL from Arm XML releases.
- `gen-ir-arm.sh` uses the upstream Makefile's source order and Isla
  splice, with fresh caches. `just tcg-arm-ir-generate` exposes it.
  `gen-cpp-arm.sh` attempts independent C++ oracle source generation.
- The emitter accepts `--aarch64`, retains the Arm notice, and emits an
  A64 decoder accessor. No parser or typed-IR changes were made.
- `bench/aarch64/` contains flat scalar-loop and CoreMark startup/linker
  files. The existing benchmark builders accept `arch="aarch64"`.
  Commit `46b0ae1` contains these workload changes.
- `crates/vmlab-hv-tcg/src/aarch64.rs` is an unintegrated implementation
  attempt. It is not declared by `lib.rs`, has never compiled or run,
  and refers to the missing `vmlab-hv-tcg-aarch64` crate. Its assumed
  generated names, reset configuration, and EL0/EL1 transitions need
  checking against actual model output before it is enabled.

## Verified

- The updated Sail toolchain container built successfully.
- Fresh `just tcg-generate-check` passed through that container using
  Sail 0.20.3 and isla commit
  `e9b5d945394277656593a0d429466d7fa0a2b4b3`.
  RISC-V Jib was 14,135,209 bytes; committed generated Rust remained
  81,607 bytes and compressed IR 2,085,721 bytes, byte-identical.
- Both emitter unit tests passed, including deterministic generation
  and preservation of the Arm notice. The AArch64 test uses synthetic
  Jib; it does not prove compatibility with the upstream model.
- Workspace Clippy passed with `--all-targets -- -D warnings`.
  Formatting passed during the subsequent CI gate. These checks do
  not cover the unreferenced `aarch64.rs` file.
- All 85 benchmark Python tests passed. Existing socket/file resource
  warnings appeared during the suite.
- The flat AArch64 scalar loop and one-iteration CRC-checked CoreMark
  image built with Clang/LLD and exited successfully under QEMU TCG.
  Runs used `virt`, `cortex-a53`, one CPU, 128 MiB, semihosting, the
  24 GiB systemd scope, and a host-wide VM-slot check. This is functional
  evidence only, not a throughput measurement.

## Open problems and missing evidence

- **F1: Sail-arm compilation is unproven.** Two isolated Jib runs passed
  type checking and rewrites and reached the displayed
  `Compiling ... (initialize_registers)` stage. Neither produced `.ir`
  before cancellation. An independent C++ generation run also produced
  no `.cpp` or header. No memory-cap kill was observed. Compiler
  containers had 12 GiB limits inside the required 24 GiB scope; the
  limits were not increased. Local evidence remains under
  `target/tcg/arm-compat/compile.log` and
  `target/tcg/aarch64-oracle/aarch64.cpp.log`. The second Jib run used
  an ephemeral cache and its log disappeared when its container stopped.
- **F2: No generated CPU exists.** There is no AArch64 generated crate,
  crate-size measurement, full-model parse/lower test, or AArch64
  reproducibility result. Real-number support for the Arm FP semantics
  and the model's actual intrinsic requirements remain unresolved.
- **F3: No instruction lockstep ran.** There is no AArch64 oracle
  library, corpus, coverage count, or divergence result. Integer,
  load/store, branch, system-register, exception, SIMD, and FP execution
  remain unverified. Zero executed cases is not evidence of agreement.
- **F4: No AArch64 MIPS results exist.** There is no interpreter timing,
  recorded QEMU baseline, or benchmark-runner integration. The images
  write `tohost` before a semihosting exit; a future same-binary timing
  method must account for that fixed exit tail and startup subtraction.
- **F5: The gates are incomplete.** `just ci::check` was interrupted
  during VM acceptance tests and cannot be reported as passing.
  `just bench-check` failed on existing x86_64 QEMU Ubuntu 26.04
  `idle_cpu_pct` history: 1.13 to 1.78 percent, beyond the recorded
  0.56 allowance. No history entries or thresholds were changed.

## Resume through issues

1. **A1:** Diagnose the full-model generation cost under the unchanged
   cap. Produce and compare two Jib outputs, then establish that the
   actual Arm model parses and lowers while RISC-V output stays identical.
2. **A2:** Generate the licensed AArch64 crate, validate the unintegrated
   adapter against its real types and entry points, and implement the
   missing runtime operations with regression tests.
3. **A3:** Build the independent in-process oracle and run the required
   instruction corpus, recording coverage and every divergence.
4. **A4:** Integrate and measure both flat workloads against QEMU using
   matching images, instruction counts, CPU affinity, and repeated samples.
   Complete CI and resolve the pre-existing benchmark-gate finding without
   rewriting historical measurements or weakening its threshold.

The AArch64 `virt` machine remains outside this work: GICv2 with GICv2m,
PL011, architectural timer, PSCI, AAVMF, PCIe/device integration, ACPI,
control-socket acceptance, and snapshots are still required later.

All jobs, compiler containers, and test VMs owned by this session were
stopped when the owner paused the work. Other sessions' processes were
left alone. No merge into main was performed.

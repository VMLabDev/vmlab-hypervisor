# RISC-V virt reference

`qemu.dtb` is the existing black-box QEMU 11.1.2 capture cited by phase-3
notes/02. It describes two harts and 2 GiB of RAM. `qemu-argv.txt` preserves
the capture command. No QEMU implementation source was read.

The original artifact is
`/home/wil/dev/vmlab-hypervisor/target/p3-research/g2/riscv64/virt-full.dtb`.
Its SHA-256 is recorded by `sha256sum qemu.dtb`; the captured DTB includes a
per-boot entropy seed, so another capture need not have the same digest.

`qemu-linux.dtb` is the same capture's `guest-fdt.dtb`, the tree Linux
consumed after OpenSBI, edk2, and the EFI loader modified it. Keep it separate
from the initial tree: it adds UEFI memory-map addresses, boot arguments,
OpenSBI reserved-memory nodes, and M-mode PLIC context masking, disables
the flash node, and removes the PMU event map.

## Normalization

`scripts/hwref/fdt.py` parses DTB v17. It normalizes node and property order,
resolves phandles to node paths, and omits `rng-seed` and `kaslr-seed` from
`/chosen`. Register ranges, interrupt routing, memory reservations, boot CPU,
and ISA properties remain compared. `scripts/hwref/diff.py` uses this parser
for `.dtb` artifacts.

The VMM's device-tree unit test can write initial trees for 1, 2, and 4 harts
when `VMLAB_HV_RISCV_FDT_ARTIFACTS` names an existing empty directory. These
fixtures have 2 GiB of RAM and no command line or initrd. They test the tree
given to firmware, not the tree firmware later passes to Linux.

## ISA policy

Phase-3 D7 permits an implemented ISA subset instead of QEMU's `-cpu max`.
The current advertisement is `rv64imafdc_zicsr_zifencei`, the subset covered
by the pinned architectural corpus. The larger model-configuration inventory
in `crates/vmlab-hv-tcg-riscv64/MODEL_ISA.md` includes unvalidated features
and is not a capability advertisement.

## Gate status

Generate the initial tree and compare it without booting a VM:

```sh
p3b_fdt_dir=$(mktemp -d)
VMLAB_HV_RISCV_FDT_ARTIFACTS="$p3b_fdt_dir" cargo test --release -p vmlab-hv-vmm dtc_accepts_tree
python3 scripts/hwref/fdt.py docs/hwref/riscv64-virt/qemu.dtb "$p3b_fdt_dir/virt-2.dtb" --riscv-subset
```

The comparison reports ten D7 differences: `riscv,isa`,
`riscv,isa-extensions`, and the omitted `riscv,cbom-block-size`,
`riscv,cbop-block-size`, and `riscv,cboz-block-size` on each of two harts.
The block-size properties belong to Zicbo extensions that this subset does
not advertise. All other normalized properties, nodes, reservations, and
the boot hart match. The command fails on any unexplained difference.

The Linux S1 test collects `linux.dtb`, `pci-config.txt`, `interrupts.txt`,
`iomem.txt`, and `cpuinfo.txt` through the guest agent before its snapshot
round trip. Set `VMLAB_HV_ACCEPTANCE_KEEP=1` to retain those artifacts in
the test's reported temporary directory.

The S1 path has reached the OpenSBI banner. A subsequent instruction-access
fault on the jump to pflash exposed the standalone Sail platform's PMA map;
the VMM now configures virt's RAM, executable pflash, and IO ranges. A
regression test reproduces the fault with the defaults and executes the
pflash instruction with the virt PMAs, including after CPU-state restore.
The paused S1 snapshot save/load/delete round trip passes.

Runtime revisions `b7747be8` and `a916903` each pass the complete Sail-oracle
corpus: 133 programs, including the two FP regressions, with no divergences.
`just tcg-generate-check` reproduces both
committed generated artifacts byte-for-byte. The three-repeat warm run for
`b7747be8` passes `just bench-check`; its CoreMark median is 46.861 MIPS
(6.493 seconds for 304,258,499 instructions). Earlier failed comparisons
remain in benchmark history. A matched, alternating three-repeat comparison
with pre-projection revision `2083e3b0` measures the corrected runtime 3.71%
slower on scalar code and 3.19% slower on CoreMark. Both remain within the
suite's five-percent regression threshold; this does not establish equal
speed.

Live firmware execution exposed excessive block-cache invalidation in the
VMM memory adapter. Page-wide guards treated stores to data beside code as
instruction changes. RAM guards now track exact instruction windows; CPU
page epochs avoid rereading unchanged windows until DMA becomes possible.
The regression executes stores beside a warm instruction loop, both before
and after enabling DMA: neither case retrains the unchanged instructions.
Separate tests check instruction replacement, overlapping windows, stores
between fetch and recording, and bounded guard storage. These changes are
in the VMM adapter, not the interpreter's execution engine.

A second slowdown appeared after edk2 enabled paging. S1 CPU-state snapshots
identified the software TLB as changing trace context beyond registers and
counters. The
recorder treated the entire TLB as a constant guard, so replacing an unused
translation retrained an otherwise unchanged code loop. TLB contents are now
runtime inputs. Vector inputs project only the entries a trace selects;
dynamic indexes have explicit guards, and vector updates preserve unread
entries. This also removes an unsupported `vector_access` stop from paged
loads. The regressions compare every Sail global with the reference
interpreter, execute without retraining when unused TLB entries change
between empty and filled in Sv39, Sv48, and Sv57, and remap the executing
page to prove that cached code observes a changed translation. Continuation
tests cover unused entry shapes, changed indexes, and bounds failures.

Paged data-load testing then exposed repeated Sail fallback on a warmed
second-page access. Index guards derived entirely from trace-entry inputs
now reject an incompatible trace before execution, allowing a separate
cached trace for the new index. Guards that depend on memory reads remain
ordered continuations. The eight-page regression checks every Sail global
against the reference interpreter and requires zero source operations on
each warmed path. Counted-memory tests verify that rejecting a trace does
not duplicate a store and that a memory-derived index reads MMIO once.
Repeatedly revisiting eight pages exposed a separate four-variant-per-PC
eviction limit, even when the working set fit the metadata budget. CPU
traces now use the configured metadata budget to bound variants. After
warming the complete page set, the revisit regression requires no Sail
operations or new recordings and checks that usage stays within that budget.

Entry-computable vector reads now project the runtime-selected element,
instead of specializing its index. A single trace reads all 64 TLB-style
indices without retraining. Projected-input dependencies survive scalar
pruning and renumbering. Memory-derived indices keep ordered guards, and
vector updates retain their existing index guards. The paged-load revisit
regression also passes with the interpreter's default metadata budget.

The next firmware replay reached an edk2 load-access exception at
`0x10008008`, the device-ID register of an advertised empty virtio-MMIO
transport. The PMA covered only the UART page, and the bus lacked these
eight transports. `empty-virtio-mmio.json` records a QEMU 11.1.2 black-box
capture of their registers, writes, and narrow and unaligned accesses.
The reduced oracle machine uses 128 MiB and no attached PCI devices; its
empty transport configuration is the same as the full capture. The VMM
now exposes the captured empty registers and covers their full IO aperture.
The regression reproduces cause 5 before the PMA fix and reads each
transport after CPU-state restoration. Direct-kernel S1 tests also probe
every transport before printing their success marker.

After timer initialization, a later cold boot dropped to hundreds of
instructions per second and retrained almost every instruction. A regression
reproduced this by changing only a future timer deadline: the deadline was
a constant context guard even though no interrupt should fire. Externally
clocked platform CPUs now capture timer comparisons, pending IRQs, and
trap-data CSRs as runtime values. Standalone interpreter policy is unchanged.
Machine and supervisor timer tests check warm-cache retention, pending-bit
transitions, and every Sail global against the reference interpreter. Trap
CSR tests cover EPC, cause (including interrupt causes), trap value, and
scratch registers. A separate test takes and returns from machine and
supervisor timer interrupts repeatedly, comparing every Sail global after
each step. All four platform tests pass. The initial interrupt-test assertion
incorrectly expected an instruction to retire before trap entry; the corrected
test expects Sail to take the interrupt first.

Runtime revision `a916903` records three-repeat medians of 0.019441 seconds
for the scalar loop and 6.117216 seconds for CoreMark (49.738 MIPS).
`just bench-check` passes with the earlier failed comparisons retained.
The subsequent cold Linux run executed about 425 million instructions in
45 minutes, reaching neither edk2's banner nor the agent. At the owner's
request it was stopped as throughput-defect evidence, not accepted as a
boot result. The CI driver was also terminated to prevent another VM launch
during the shared benchmark window. Its failed log is retained; CI must run
again after the runtime changes.

The saved profile showed allocation and aggregate cloning during trace
recording. The named executor used by the recorder cloned a whole struct
before selecting a field, and both executors cloned a whole vector before
selecting an element. A no-VM allocation regression reproduced this:
ten named field selections grew from 16,110 to 1,654,110 allocated bytes
when an unread vector grew from one to 4,096 entries. Vector selection grew
from 10,410 to 1,648,410 bytes.

Revision `5dfc0ed` borrows field and union projections and vector-selection
inputs, cloning only the selected result. The same ten-call regressions now
allocate 8,410 bytes in the named executor and 1,200 bytes in the indexed
executor, independently of unread aggregate size. Bounds, type errors,
bit selection, nested projections, and ownership tests pass. Repeated
code/data TLB conflicts stay native, and machine/supervisor timer entry and
return still match every Sail global. Formatting and TCG clippy pass.
After merging main `6cceef4`, revision `0d6d578` passes all 133 Sail-oracle
programs without divergence. Generated Rust and compressed IR remain
byte-identical. Three-repeat warm medians are 0.019528 seconds for the
scalar loop and 6.167672 seconds for CoreMark, or 49.331 MIPS.
`just bench-check` passes. These measurements exclude cold trace creation
and do not establish firmware boot throughput.

The fresh initial-tree comparison reports exactly the ten approved D7
differences. The hwref suite runs 62 tests with two optional recaptures
skipped; all other tests pass. All 87 benchmark unit tests pass after the
merge. Full CI is still running. Its three direct-kernel S1 tests pass,
including all empty MMIO transports, shutdown, exit-on-reset, and preserved
RAM across warm reset in the same process. Cold edk2 and Linux acceptance
remain pending.

The post-merge firmware diagnostics exposed a separate 4,096-PC cache cap.
It evicted entries while using only 18.8 million of 64 million configured
metadata units. A unit-level IR CPU reproduces the premature eviction with
4,097 PCs. Revision `5d40efe` removes that count threshold and retains the
metadata budget. All 4,097 cached paths then execute natively without Sail
operations or new recordings, using 45,067 of 8,000,000 units. A separate
test verifies eviction and bounded usage when the working set exceeds its
budget.

The full TCG/IR test run and release rebuild pass. This runtime also passes
all 133 Sail-oracle programs in four disjoint parts. The validation checks
the exact manifest coverage, including both FP cases, and every part exits
zero. Its three-repeat warm medians are 0.019256 seconds for scalar code
and 6.345797 seconds for CoreMark, or 47.946 MIPS. `just bench-check` passes;
the CoreMark median is slower than the preceding run but remains within the
suite's measured-noise threshold.

The CI run compiled before this cache change was interrupted and is not a
current-source acceptance result. A new cold Linux S1 run uses the rebuilt
release binary. Fresh full CI, live Linux acceptance, hardware capture,
booted-guest snapshot restoration, and D2 boot timings remain pending.

The benchmark runner's TCG path referenced an undefined `BOOT_DEADLINE`,
removed by an earlier x86 readiness change. The original 1,800-second
deadline is restored, retaining `--timeout-scale`. A no-VM regression first
reproduces the `NameError`, then exercises CPU release, agent waiting,
boot-only samples, result persistence, and cleanup. All 63 benchmark unit
tests pass; existing socket and file cleanup warnings remain.

edk2 boot, Linux hardware capture, booted-guest snapshot restoration, and
boot-to-agent-ready timing are pending. This reference does not establish
a phase-3b parity pass.

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

Runtime revision `b7747be8` passes the complete Sail-oracle corpus: 133
programs, including the two FP regressions, with no divergences.
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

edk2 boot, Linux hardware capture, booted-guest snapshot restoration, and
boot-to-agent-ready timing are pending. This reference does not establish
a phase-3b parity pass.

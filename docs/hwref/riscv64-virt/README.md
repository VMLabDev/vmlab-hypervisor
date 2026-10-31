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

edk2 boot, Linux hardware capture, booted-guest snapshot restoration, and
boot-to-agent-ready timing are pending. This reference does not establish
a phase-3b parity pass.

# A20 aliases and loop measurement

The pc machine uses memfd-backed RAM with separate CPU and DMA views. When
A20 is disabled, each odd 1-MiB CPU bank aliases the preceding even bank.
The DMA view retains the original physical addresses and contents.
Enabling A20 restores the original CPU mappings without copying RAM.

This avoids one KVM memslot per MiB and instruction-by-instruction emulation.
A transition remaps each odd RAM bank with `MAP_FIXED`; transition work grows
with installed RAM. Steady-state RAM accesses use KVM mappings directly.
All vCPUs leave KVM before the mappings change.

The legacy aperture needs separate handling. PAM shadow banks gain matching
aliases above 1 MiB. The high BIOS has a precomputed masked view, including
open-bus bytes where the masked source lies outside the ROM. Direct VGA
framebuffer mappings are removed while A20 is disabled; accesses then use
the masked MMIO path.

The i8042 output port and port 0x92 control the same effective gate. A repeated
i8042 output write reasserts its value even if port 0x92 changed the effective
gate meanwhile. Snapshots save both controller registers and the effective
gate. Reset restores the enabled gate in place.

## CPU-visible verification

`a20_acceptance::pc_a20_cpu_wraparound_reset_and_snapshot` passed through the
executable's control socket on 2026-10-31. Its custom ROM checks actual CPU
reads and writes through both controllers, including real-mode FFFF:0510,
the 2/3-MiB pair, and the PAM ROM alias. It restores enabled and disabled
snapshots, resets through both controllers, observes a fresh boot, and keeps
the same COM1 socket connection.

The probe exposed two implementation defects before passing: repeated i8042
writes did not reassert the gate, and snapshot restore tried to remove an
already-absent memslot. Both have regression coverage. A separate mapping
test verifies that CPU aliases do not change the DMA view or discard the
original odd-bank contents. A ROM test covers a partially populated MiB bank.

## Real-mode loop

The parent session ran `a20_acceptance::pc_a20_real_mode_loop_cost` under a
24-GiB systemd scope with swap disabled. The guest executes 100 million
segmented byte loads through ES:0510, with ES=FFFF, for each sample.
The host measures command-to-completion elapsed time.

| A20 | Samples, ms | Mean, ms |
|-----|-------------|----------|
| Enabled | 20.726186, 20.451369, 20.409992 | 20.529182 |
| Disabled | 20.525449, 20.372201, 20.339539 | 20.412396 |

The disabled mean is 0.6% lower, within the observed sample variation.
This run shows no steady-state slowdown. These figures measure the loop,
not FreeDOS or Windows 98 boot time, and exclude the gate-transition cost.
Template boot measurements remain unqualified.

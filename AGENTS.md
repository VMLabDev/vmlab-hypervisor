# AGENTS.md

`vmlab-hv` is a clean-room, MIT-licensed Rust hypervisor that vmlab
(`/home/wil/dev/vmlab`) will run one process per machine, in place of QEMU.

## Before you change anything

Read the specification at
`/home/wil/dev/vmlab-hypervisor/.scratch/plans/vmlab-hypervisor/SPEC.md`. It is
the contract. A `Dn` reference points at a decision row in `MAP.md`, in the same
directory. Its reasoning is in `notes/`, which you open only when the outcome
alone is not enough. When the spec and your task brief disagree, follow the spec
and say so in your report.

## Crates

- `vmlab-hv`: the binary. Parses `--machine`, `--control`, `--paused` and wires the VMM.
- `vmlab-hv-proto`: the machine description and the control protocol. vmlab depends on it.
- `vmlab-disk`: the `DiskImage` seam, the raw and qcow2 implementations, and the image operations in `ops`. vmlab depends on it.
- `vmlab-hv-devices`: device models written against the traits in its `lib.rs` (`BusDevice`, `PciDevice`, `VirtioDevice`, `IrqLine`, `MsiSink`, `Snapshot`). They know nothing about KVM.
- `vmlab-hv-vmm`: KVM, guest memory, vCPUs, the buses, the machine model, the control server, and snapshot orchestration.

Shared traits and types are the interface between parallel work. If one needs
to change, make the smallest change that works, and name it in your report.

## Clean room

Implement from specifications: the VIRTIO 1.2 spec, Intel and AMD manuals, the
ACPI spec, the qcow2 spec (`docs/interop/qcow2.txt` in QEMU's tree is a spec
document and may be read), the Linux boot protocol, and the rust-vmm crate
docs. Use QEMU only as a black box: run it, or run `qemu-img`, to observe
behaviour.

Guardrail: never read QEMU's C source, and never copy or translate it. The
project's licence depends on it.

Code lifted from crosvm keeps its BSD-3 header and gets a line in
`THIRD_PARTY_NOTICES`. Apache-2.0 crates are used as dependencies only.

## Finishing a task

A task is done when all three hold:

1. `just ci::check` passes: fmt, clippy with `-D warnings`, and the tests.
2. Every `unimplemented!()` or `todo!()` inside your task's scope has been replaced by working code with tests.
3. The behaviour your task owns is proven by a test at the seam the spec names for it: S1 (the `vmlab-hv` control socket) or S2 (`vmlab-disk` against `qemu-img`). Device unit tests sit below those seams and do not replace them.

Commit on your own branch with conventional-commit messages (`feat(disk): ...`).
Add no co-author trailers. End with a report that lists what works, what you
verified and how, any shared interface you changed, and anything left
unfinished, with the reason.

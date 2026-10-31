# vmlab-hypervisor

`vmlab-hv` is a clean-room, MIT-licensed hypervisor written in Rust. vmlab
(https://github.com/VMLabDev/vmlab) runs one `vmlab-hv` process per machine in
place of QEMU. A machine selects it with `hypervisor = "vmlab"`; QEMU stays the
default. Each process takes a typed machine description and serves a JSON-lines
control protocol on a unix socket:

```
vmlab-hv --machine <machine.json> --control <control.sock> [--paused]
```

The machine description and control protocol live in `vmlab-hv-proto`, and the
disk image code in `vmlab-disk`; vmlab consumes both as git dependencies.
Snapshots are the hypervisor's own versioned device-state format stored in qcow2
internal snapshots. Offline (disk-only) snapshots stay plain qcow2 and remain
portable to QEMU. Guest-visible hardware on the q35 machine matches QEMU's, so
existing templates and OVMF variable stores keep working; the captures under
`docs/hwref/` are the parity evidence.

## Status

The work is organised in four phases. Where they stand:

- Phase 1, container micro-VMs: complete. Direct kernel boot, virtio-blk,
  virtio-net, virtio-console, vhost-user-fs through an external virtiofsd,
  online and offline snapshots, and the S1 acceptance suite.
- Phase 2, modern x86_64 VMs on q35: built out through its final sub-phase.
  SeaBIOS and OVMF boot, with SMM and secure boot; a TPM 2.0 device over
  swtpm's socket; Hyper-V enlightenments through KVM; virtio-gpu 3D (virgl and
  Venus) through rutabaga with dma-buf scanout; in-process virtio-fs whose
  state rides snapshots; Intel HDA audio on the host's PipeWire or PulseAudio;
  xHCI with host USB passthrough and hot-plug; vfio-pci passthrough of
  pre-bound functions; and guest reset in place, preserving host connections.
  Windows and UEFI templates boot.
- Phase 3, riscv64 and aarch64 under the project's own TCG: in progress. The
  pinned Sail toolchain emits Jib IR from the sail-riscv model; the parser,
  typed IR, and generator produce the committed riscv64 interpreter crate,
  which runs the pinned instruction corpora in lockstep against the Sail
  oracle. aarch64 and the Cranelift JIT are not started.
- Phase 4, legacy PC (i440fx, IDE, floppy): not started. macOS guests are a
  separate phase after that, also not started.

## Crates

| Crate | What it is |
|---|---|
| `vmlab-hv` | The binary. Parses `--machine`, `--control`, `--paused` and wires the VMM. |
| `vmlab-hv-proto` | The machine description and the control protocol. vmlab depends on it. |
| `vmlab-disk` | The `DiskImage` seam, the raw and qcow2 implementations, and the image operations in `ops`. vmlab depends on it. |
| `vmlab-hv-vmm` | KVM, guest memory, vCPUs, the buses, the `microvm` and `q35` machine models, the control server, and snapshot orchestration. |
| `vmlab-hv-devices` | Device models written against the traits in its `lib.rs`. They know nothing about KVM. |
| `vmlab-hv-firmware` | q35 firmware inputs: fw_cfg, ACPI tables, SMBIOS, and the loaders. |
| `vmlab-hv-display` | The shared-memory frame protocol, the display server core, a client, and the RFB server. |
| `vmlab-hv-tests` | The S1 integration harness: it spawns the binary and drives it over the control and guest protocols. |
| `vmlab-hv-sail-ir` | A parser and printer for Sail's textual Jib IR. |
| `vmlab-hv-tcg-ir` | The typed IR the interpreter executes and the JIT will consume. |
| `vmlab-hv-tcg` | The reference riscv64 interpreter over that IR, with SoftFloat. |
| `vmlab-hv-tcg-emit` | The generator that emits the committed riscv64 crate from the Jib IR. |
| `vmlab-hv-tcg-riscv64` | The generated riscv64 model, derived from sail-riscv (BSD-2-Clause). |
| `vmlab-hv-lockstep` | Instruction-by-instruction lockstep against the Sail oracle. |

Device models in `vmlab-hv-devices` cover the virtio family, the ICH9 chipset,
VGA and virtio-gpu, HDA, xHCI, and the TPM TIS. The `pipewire` and `pulse`
cargo features enable the corresponding host audio backends; without them the
null backend runs.

## Host requirements

- x86_64 Linux with KVM. The VMM requires `/dev/kvm`; there is no TCG fallback
  for x86_64 guests yet.
- A stable Rust toolchain (the workspace sets `rust-version = "1.99"`,
  edition 2024) and `just`.
- pkg-config plus virglrenderer >= 1.3.0, EGL, and GBM development libraries.
  `vmlab-hv-devices` probes all three at build time.
- For the audio features, the libpipewire and libpulse development files
  (PipeWire also needs libclang for bindgen).
- Firmware arrives by machine description. SeaBIOS comes from your distro;
  vmlab pins and bundles the OVMF builds it passes, so the host needs no OVMF
  package. The description names the CODE and VARS paths.
- Optional at runtime: `swtpm` for TPM machines, `virtiofsd` for external
  vhost-user-fs shares, `qemu-img` for the S2 disk tests, and `sqfstar` to
  build the S1 rootfs.

## Build and test

```sh
just workspace-build   # cargo build --workspace
just ci::check         # cargo fmt --check, clippy -D warnings, cargo test
```

The S2 disk tests and the unit tests run anywhere. The S1 integration tests
need `/dev/kvm`, vmlab's pinned container kernel from a vmlab checkout, and a
squashfs rootfs; `just s1-tests-run` builds the rootfs from the pinned Alpine
minirootfs and runs the suite. Every S1 test skips with a printed reason when
a prerequisite is missing.

The convention for a full suite run on the development host is a systemd-run
scope with a memory cap and one test thread, for example
`systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 -- env
RUST_TEST_THREADS=1 just ci::check`. The verification records in
`docs/memory-leak.md` and `crates/vmlab-hv-tcg-riscv64/VERIFICATION.md` use
this shape.

## Clean room

Everything here is implemented from specifications: the VIRTIO 1.2 spec, the
Intel and AMD manuals, the ACPI spec, the qcow2 spec, the Linux boot protocol,
and the rust-vmm crate docs. QEMU is used only as a black box: run it, diff its
guest-visible hardware (`docs/hwref/`), check images with `qemu-img`, and race
it in `bench/`. Never read QEMU's C source, and never copy or translate it.
The project's licence depends on this.

Code lifted from crosvm keeps its BSD-3 header, and Sail-derived and SoftFloat
code keeps its upstream notices. Every such file is listed in
`THIRD_PARTY_NOTICES`. Apache-2.0 crates are dependencies only.

## Further reading

- vmlab, the lab runner this hypervisor serves: https://github.com/VMLabDev/vmlab
- `docs/hwref/` — black-box QEMU hardware captures and the P2 differential.
- `docs/research/` — the grounding notes and the QEMU-viability study behind
  the project.
- `bench/` — the benchmark suite that holds phase 3's speed bar against QEMU.
- Topic notes: `docs/gpu.md`, `docs/vfio.md`, `docs/reset.md`,
  `docs/display-testing.md`, `docs/memory-leak.md`.

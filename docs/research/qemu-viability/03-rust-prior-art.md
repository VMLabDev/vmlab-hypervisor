# Slice C — Rust prior art vs the vmlab QEMU inventory

Verdict up front: the modern virtio/KVM core of vmlab's usage exists in Rust several
times over (rust-vmm crates, cloud-hypervisor, crosvm, firecracker, libkrun), and
crosvm already covers the hard GPU cases (virgl, venus, headless rendering) plus USB
passthrough. What no Rust project covers: PC chipsets and every pre-virtio device
(q35/pc, IDE/AHCI, floppy, e1000/pcnet, VGA/qxl/cirrus), TCG-class foreign-arch CPU
emulation, qcow2 internal snapshots with in-image vmstate, anything QMP-shaped, a VNC
server with QEMU's pseudo-encodings, and a SeaBIOS-compatible legacy BIOS. Those six
areas are where "delete QEMU features and port the rest" has no Rust shortcut.

## 1. Projects surveyed (measured from shallow clones in qv/, 2026-10-29/30)

| project | HEAD (date) | Rust LOC (files) | license | arches / accel |
|---|---|---|---|---|
| cloud-hypervisor | aefcd6d (10-29) | 205,525 (328) | Apache-2.0 | x86-64, aarch64, riscv64 (experimental, README.md:62-68) / KVM + mshv |
| crosvm | ad804a6 (10-29) | 382,959 (993) | BSD-3-Clause | x86_64, aarch64, riscv64 crate (completeness not checked) / KVM |
| firecracker | 76cb694 (10-29) | 131,034 (366) | Apache-2.0 | x86_64, aarch64 (rust-toolchain.toml:15) / KVM |
| libkrun | d166df9 (10-29) | 109,866 (358) | Apache-2.0 | x86_64, aarch64 / KVM + Apple HVF + Windows WHP (src/hvf, src/whp) |
| QEMU rust/ (11.1.50) | d8a6048 (10-18) | 12,056 (65); device code 3,198 (2 devices) | GPL-2.0-or-later | bindings only |

LOC counts are `find -name '*.rs' | cat | wc -l` on each clone, including tests,
generated code and vendored crates; treat as an upper bound on hand-written code
(estimate basis: whole-tree count).

Component crates (cloned to qv/rust-vmm/, counted same way; versions from crates.io
2026-10-30): kvm-ioctls 0.25.1 (23,909 ln), vm-memory 0.18.0 (12,327), virtio-queue
0.18.0 (in rust-vmm/vm-virtio, repo total 62,827 incl. virtio-blk/console/device/vsock
crates), vhost 0.18.0 (16,322), vm-superio 0.8.2 (2,781: 16550-style serial, pl011,
i8042, rtc_pl031), linux-loader 0.14.0 (5,518: bzImage/PE/FDT/direct boot),
vmm-sys-util 0.15.0 (7,625), acpi_tables 0.2.1 (10,317). All Apache-2.0 OR
BSD-3-Clause except linux-loader (AND). virtiofsd (gitlab.com/virtio-fs/virtiofsd,
18,848 ln, Apache-2.0 AND BSD-3-Clause) — the daemon vmlab already spawns is Rust.
Firecracker depends on vm-superio 0.8.2 (src/vmm/Cargo.toml:56); cloud-hypervisor and
crosvm build on the rust-vmm stack directly.

## 2. Support matrix

Legend: Y supported, P partial (caveat in note), N no. CH = cloud-hypervisor,
CS = crosvm, FC = firecracker, Q = QEMU upstream rust/. "Other" names the nearest
Rust thing outside the four.

### Machines and accelerators

| inventory item | CH | CS | FC | Q | other |
|---|---|---|---|---|---|
| x86 machine q35 | N | N | N | N | none |
| x86 machine pc (i440fx/PIIX) | N | N | N | N | none |
| aarch64 virt-class machine | Y¹ | Y¹ | Y¹ | N | libkrun Y |
| riscv64 virt, DT boot | P² | P³ | N | N | none production |
| KVM | Y | Y | Y | n/a⁴ | libkrun Y (+HVF/WHP) |
| TCG (foreign-arch guest) | N | N | N | N⁵ | Unicorn only (see §3) |
| Hyper-V hv-* enlightenments | N | N | N | N | none |

¹ Own minimal platform (GICv3 + ACPI/DT), not binary- or DT-compatible with QEMU's `virt`; guests boot but machine identity differs. CH README.md:62-68. ² CH docs/riscv.md, "experimental and offers limited functionality". ³ riscv64 crate exists in-tree (Cargo.toml:193); completeness not checked. ⁴ QEMU's KVM stays C; rust/ only swaps two device models. ⁵ No TCG port plan exists (see §5).

### Devices

| inventory item | CH | CS | FC | Q | other |
|---|---|---|---|---|---|
| virtio-blk-pci | Y | Y | Y | N | rust-vmm virtio-blk; libkrun Y |
| ide-hd / ide-cd (PIIX IDE, AHCI) | N | N | N | N | none |
| floppy | N | N | N | N | none |
| virtio-net-pci | Y | Y | Y | N | libkrun Y |
| e1000 | N | N | N | N | none |
| pcnet | N | N | N | N | none |
| VGA (std) | N | N | N | N | none |
| virtio-vga / virtio-gpu 2D | N⁶ | Y | N | N | libkrun Y |
| qxl | N | N | N | N | none |
| cirrus-vga | N | N | N | N | none |
| virgl 3D | N⁶ | Y⁷ | N | N | libkrun Y (rutabaga virgl_renderer) |
| venus Vulkan | N | Y⁸ | N | N | none |
| GPU vfio-pci passthrough | Y | Y | N | N | — |
| egl-headless (headless GL) | N | Y⁹ | N | N | — |
| VNC server | N | N | N | N | P: generic RFB servers only¹⁰ |
| virtio-serial + named port | P¹¹ | N¹² | N | N | rust-vmm virtio-console crate |
| isa-serial (16550/pl011) | N | Y¹³ | Y | N | vm-superio (serial, pl011) |
| qemu-xhci | N | Y | N | N | — |
| usb-host (hostbus/addr/port) | N | Y | N | N | — |
| usb-tablet / pointer input | N | P¹⁴ | N | N | libkrun virtio-input |
| tpm-tis + swtpm | P¹⁵ | N | N | N | swtpm itself is C |
| vhost-user-fs-pci | Y¹⁶ | Y | N | N | virtiofsd (Rust) |

⁶ No GPU device anywhere in CH tree (`find -name '*gpu*'` empty; virtio-devices/src has no gpu.rs). ⁷ Cargo feature `virgl_renderer` → rutabaga_gfx (crosvm/Cargo.toml:333). ⁸ Venus context-init in devices/src/virtio/gpu/protocol.rs + rutabaga_gfx (pinned =0.1.80); vulkan also via `vulkan_display`/`vulkano` features. ⁹ gpu_display backends wl/x/win/android/stub — the stub backend renders headless; crosvm also has a screenshot command. ¹⁰ rfb2 (https://crates.io/crates/rfb2, 2025) and rustvncserver (https://crates.io/crates/rustvncserver, 2026, Apache-2.0) implement RFB servers; neither documents QEMU's pseudo-encodings (-261 LED state, -239/-314 cursor, -224/-258/-259) — those would be hand-written. ¹¹ virtio-devices/src/console.rs, single console, not a multiport virtserialport. ¹² crosvm has no virtio-console device (devices/src/virtio/ listing); console is the in-tree UART serial. ¹³ devices/src/serial.rs, 901 lines, own implementation. ¹⁴ virtio-input (devices/src/virtio/input), no USB tablet and no PS/2 send-key equivalent. ¹⁵ tpm crate implements a TPM CRB device (not TIS) talking to an external swtpm over the PTM unix-socket protocol (tpm/src/emulator.rs); swtpm stays a C dependency. ¹⁶ virtio-devices/src/vhost_user/{fs,blk,net}.rs.

### Block storage and snapshots

| inventory item | CH | CS | FC | Q | other |
|---|---|---|---|---|---|
| qcow2 read/write | Y¹⁷ | Y¹⁸ | N¹⁹ | N | imago²⁰, qcow2-rs²¹ |
| qcow2 backing files / rebase | Y | Y | — | N | imago Y |
| qcow2 compressed clusters | Y (zlib) | N²² | — | N | imago read |
| qcow2 internal snapshots (qemu-img snapshot -c/-a/-d) | N²³ | N²⁴ | N | N | none — all reject |
| online snapshot w/ vmstate (snapshot-save/-load jobs) | P²⁵ | P²⁶ | P²⁷ | N | none QMP-shaped |
| qemu-img tooling (info/resize/convert/commit) | N | N | N | N | none²⁸ |

¹⁷ Own implementation, 10,590 lines (block/src/formats/qcow/, parser 2,390 + sync engine 2,275 + uring engine 896). ¹⁸ Own implementation, 3,773 lines (disk/src/qcow/). ¹⁹ Zero `qcow` matches in firecracker/src — raw only. ²⁰ imago 0.2.6 (https://crates.io/crates/imago, MIT, active 2026-10-16, by a QEMU block maintainer): read-write qcow2, backing chains, compression, optional vm-memory interop; no internal snapshots (src/qcow2/metadata.rs round-trips nb_snapshots with a TODO). libkrun's HEAD commit bumps imago. ²¹ qcow2-rs 0.1.6 (https://crates.io/crates/qcow2-rs, 2024, ublk ecosystem): r/w incl. compressed+backing, no snapshot-table code. Also read-only: `qcow2` (2016, dormant), `qcow2-core`/`qcow2-forensic` (forensics). ²² disk/src/qcow/mod.rs: `CompressedBlocksNotSupported`. ²³ block/src/formats/qcow/parser.rs:127,385 `UnsupportedSnapshots` for writable images with nb_snapshots != 0. ²⁴ disk/src/qcow/mod.rs:670 `SnapshotsNotSupported` when nb_snapshots != 0; crosvm cannot even open such images. ²⁵ CH snapshot()/restore snapshots the whole VM (CPU, RAM, devices: vmm/src/vm.rs:2956, device_manager.rs:6588) to CH's own format; live migration to a second CH exists (docs/live_migration.md). Not qcow2-embedded, not QMP. ²⁶ crosvm whole-VM snapshot crate (snapshot/src), documented "highly experimental". ²⁷ FC paused-VM snapshot/restore (src/vmm/src/snapshot, persist.rs) with a versioned format independent of FC releases (docs/snapshotting/versioning.md); snapshot-editor/rebase-snap tools ship in-tree. ²⁸ qcow2-rs/imago ship meta-dump/convert utilities, not a qemu-img equivalent.

### Display, firmware, boot

| inventory item | CH | CS | FC | Q | other |
|---|---|---|---|---|---|
| SeaBIOS / legacy BIOS boot | N | N²⁹ | N | N | none |
| UEFI pflash (OVMF/AAVMF + VARS) | P³⁰ | P³¹ | N | N | — |
| secure boot (cfi.pflash01 secure=on) | not checked³² | N³³ | N | N | — |
| direct -kernel boot (containers) | Y | Y | Y | N | linux-loader crate |
| SMBIOS type=1 override | P³⁴ | P³⁵ | not checked | N | — |

²⁹ crosvm's pflash exists solely for OVMF variable storage (devices/src/pflash.rs:5-7); its `--bios` path loads firmware images, not a SeaBIOS-compatible BIOS. ³⁰ UEFI via CH's edk2 fork (CLOUDHV_EFI.fd) loaded as firmware, no pflash device, no persistent VARS (docs/uefi.md). ³¹ devices/src/pflash.rs is a purpose-built clone of QEMU's pflash for OVMF varstore — no CFI tables, and crosvm has no SMM. ³² CH's edk2 fork does build secboot variants but whether vmlab's `.ms`/secboot flows map was not checked. ³³ pflash.rs:17-20 states no SMM, so secure boot's SMM dependency is absent. ³⁴ `--platform oem_strings` etc. (vmm/src/config.rs:1058); arbitrary type=1 fields not verified. ³⁵ crosvm generates SMBIOS (x86_64/src/smbios.rs) but an arbitrary type=1 override was not verified.

### Control plane and plumbing

| inventory item | CH | CS | FC | Q | other |
|---|---|---|---|---|---|
| QMP (all commands/events vmlab uses) | N³⁶ | N | N | P³⁷ | client crates only |
| send-key / input-send-event / screendump | N | P³⁸ | N | N | — |
| netdev stream (unix-socket L2 to vmlab switch) | N | N | N | N | none — all use tap³⁹ |
| netdev tap with pre-opened fd | Y | Y | Y | N | libkrun Y |
| memory-backend-memfd,share (virtiofs) | Y | Y | — | N | — |

³⁶ CH/CS/FC each have their own control plane (HTTP API, control sockets); none speak QMP. ³⁷ Initial QAPI/QObject bindings merged in QEMU 11.0; QMP commands themselves are future work (Jan 2026 status post, https://lists.nongnu.org/archive/html/qemu-rust/2026-01/msg00103.html). ³⁸ crosvm has a screenshot command and virtio-input injection; no send-key QMP equivalent. ³⁹ CH net_util = tap/open_tap only; FC virtio/net/tap.rs; crosvm net_util tap. Replacing the stream netdev means writing an L2 socket backend or moving vmlab's switch to tap fds.

## 3. TCG replacement: what exists

Unicorn (https://github.com/unicorn-engine/unicorn) is the closest thing and it is
QEMU TCG itself: a C fork of QEMU 5.0.1's JIT (FAQ: "Prior to 2.0.0, qemu 2.2.1.
After that, qemu 5.0.1"), GPLv2, CPU-core only ("a pure CPU emulator" — no devices,
no interrupt controller, no system-mode machine; OS emulation is out of scope and
rebased rarely). Covers x86/x64, aarch64, riscv32/64 — but from a QEMU 5.0.1 ISA
base, so e.g. no ratified RISC-V vector. Official Rust bindings: `unicorn-engine`
2.1.5 (2025-09, GPL-2.0; https://crates.io/crates/unicorn-engine); the old `unicorn`
0.9.1 crate is deprecated.

Pure Rust: codegen substrates exist (dynasmrt, MPL-2.0, x64/aarch64/riscv host
backends; Cranelift, Apache-2.0) but nobody has built a multi-arch system-capable
emulator on them. What exists is user-mode x86 (mwemu, Apache-2.0, active), a 32-bit
Win32 interpreter (retrowin32, abandoned for a static-translation experiment), a
weekend x86-64 core (remu64), an ARMv4T GBA core, and two tiny full-system RV64
interpreters that run Linux (riscv-rust, stale since 2020; RV64emu-rs, 23 stars, no
FP). MartyPC is the one serious pure-Rust full-system x86 machine (MIT, cycle-accurate
8088) — 1980s guests only. Conclusion: for foreign-arch labs (a core vmlab use —
TCG is chosen whenever guest arch != host arch), there is no Rust substitute; the
realistic options are keep QEMU-as-TCG-process or link Unicorn knowing it is
GPLv2 and device-less.

## 4. QEMU's upstream rust/ effort

State at 11.1.50 (this tree) and master: experimental, `--enable-rust` opt-in
(meson default `disabled`), two devices in-tree — pl011 UART (landed 9.2, Dec 2024)
and x86 HPET (landed 10.0, Apr 2025), 3,198 lines of device code total, with the C
implementations still present; Kconfig swaps C→Rust under HAVE_RUST. A third device
(pcf8574 GPIO expander over new I2C bindings) is on the list. Scope today is
SysBusDevice models in safe Rust; "later, it may become possible to write ... PCI
devices that can do DMA, complete boards, or backends" (devel/rust.html). Stated
positions: no TCG port, no virtio port (they intend to reuse rust-vmm's vm-memory /
vm-virtio instead), block layer exploratory (Kevin Wolf's async series dormant),
QAPI bindings merged in 11.0 with QMP commands as future work, migration bindings
exist (vmstate "stable", migratable "proof of concept"). Sources:
https://www.qemu.org/docs/master/devel/rust.html ,
https://wiki.qemu.org/RustInQemu , quarterly updates
https://lists.gnu.org/archive/html/qemu-devel/2025-05/msg00455.html ,
https://lists.nongnu.org/archive/html/qemu-rust/2025-10/msg00076.html ,
https://lists.nongnu.org/archive/html/qemu-rust/2026-01/msg00103.html .
Practical read: QEMU's own Rust effort will not remove C features on any horizon
relevant to a strip-down fork; it is additive device authoring, and slow.

## 5. Gaps no Rust project covers

1. PC platform: q35 and i440fx/PIIX chipsets, IDE/AHCI, floppy, e1000, pcnet,
   std VGA, qxl, cirrus, SMM. vmlab's legacy profiles depend on these
   (e1000/pcnet/cirrus/ide/floppy in shipped.wcl). Nothing in Rust; upstream QEMU
   rust/ swaps exactly one UART and one timer.
2. TCG-class software emulation of x86_64/aarch64/riscv64 guests (§3). Without it,
   every foreign-arch lab needs a QEMU-shaped TCG process retained.
3. qcow2 internal snapshots and QEMU's snapshot-save/-load with vmstate inside the
   image. All five Rust qcow2 implementations reject or lack the snapshot table
   (§2, block table); CH/CS/FC whole-VM snapshots exist but in three mutually
   incompatible, non-qcow2 formats with no QMP job semantics.
4. The QMP surface: 15+ commands and 6 events vmlab uses (send-key,
   input-send-event, screendump, eject, device_add/del, snapshot jobs, query-jobs,
   job-dismiss, SHUTDOWN/SUSPEND/WAKEUP/RESET/RESUME/DEVICE_DELETED). No Rust VMM
   speaks QMP; a Rust replacement means rewriting vmlab's control plane against a
   new API and re-implementing the operations.
5. A VNC server with QEMU's pseudo-encodings (LED-state -261, RichCursor/AlphaCursor,
   DesktopSize) over a unix socket, which vmlab's client and recorder consume.
   Generic Rust RFB servers exist; the QEMU extensions would be from-scratch.
6. SeaBIOS-compatible legacy BIOS boot; the whole DOS/Win9x/XP-era boot path.
7. (Partial gap) TPM: only CH has a device, it is CRB not TIS, and it still shells
   out to swtpm (C). No Rust TPM 2.0 emulator found.

Covered well by prior art (for contrast): KVM driving, virtio block/net/console/fs/
balloon/rng, vhost-user, tap, direct kernel boot, ACPI/SMBIOS generation, UEFI via
edk2 (with caveats), virgl + venus + headless GPU (crosvm, via rutabaga_gfx — the
same virglrenderer stack QEMU's egl-headless uses), USB xhci + host passthrough
(crosvm), vfio-pci (CH, crosvm), and virtiofsd is already Rust today.

## 6. Licenses

QEMU is GPLv2; any port of QEMU C code into Rust stays GPLv2 (QEMU's own rust crates
are GPL-2.0-or-later, https://gitlab.com/qemu-project/qemu/-/raw/master/Cargo.toml).
cloud-hypervisor, firecracker, libkrun: Apache-2.0. crosvm: BSD-3-Clause. rust-vmm
crates: Apache-2.0 OR BSD-3-Clause (linux-loader: AND). virtiofsd: Apache-2.0 AND
BSD-3-Clause. Unicorn: GPL-2.0. imago: MIT. qcow2-rs: MIT OR Apache-2.0.

Consequences: BSD/MIT/BSD-3 code (crosvm, vmm-sys-util, imago, qcow2-rs) can be
merged into a GPLv2 QEMU fork. Apache-2.0 code (rust-vmm kvm-ioctls/vm-memory/
virtio-queue/vhost, cloud-hypervisor, firecracker) is GPLv3-compatible only — it
cannot be folded into a GPLv2-only derivative, so a "QEMU fork ported to Rust" and
"reuse rust-vmm components" are mutually exclusive for those crates. Conversely an
Apache-2.0 rewrite of vmlab can use everything except QEMU-derived and Unicorn code.

## Method notes / not checked

- Everything in §1 measured from the clones listed; matrix cells verified against
  source files cited in the footnotes (paths relative to qv/<project>).
- Web facts fetched 2026-10-30 (scout transcripts: qcow2 crates, Unicorn, QEMU rust
  roadmap — URLs inline above).
- Not checked: crosvm riscv64 maturity beyond crate presence; CH edk2 secboot; FC
  SMBIOS customization; actual encoding coverage of rfb2/rustvncserver beyond their
  READMEs; hvf/whpx (out of inventory); whether CH/crosvm snapshots survive version
  skew (FC's versioning policy is documented, CH's is not).

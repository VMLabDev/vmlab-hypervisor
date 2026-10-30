# Slice B — subsystem coupling and Rust porting difficulty

Tree: `qv/qemu` = upstream 11.1.50 (`v11.1.0-2519-gd8a6048`) plus the 6 vmlab `ui/egl-*` external-dma-buf patches (HEAD stack `674931d..d8a6048`). All counts are `wc -l` on that tree unless marked "estimate". Object and device counts come from a fresh build I made (`qv/build-b`, x86_64+aarch64+riscv64, `--disable-docs`): 3,260 objects; `qemu-system-x86_64` = 91 MB unstripped, 15.9 MB text, 31,588 text symbols, **513 device types**, 45 machines, 49 `-object` types. Whole C tree (no rust/, no tests/): 3,170,099 lines. Note: my first build in `qemu/build` was wiped by a sibling slice; `build-b` is out-of-tree. Upstream velocity for context: 2,519 commits between `v11.1.0` and this HEAD.

## 1. Core object model, device model, memory, main loop, coroutines

Size (vmlab subset ≈ 25k, partly estimate):
- `qom/*.c` 4,293 (object.c 3,309; API in `include/qom/object.h`, 2,465 lines).
- `hw/core/*.c` 15,815 total, needed ≈ 13k (estimate): qdev.c 904, qdev-properties.c 1,225 + -system 1,371, machine.c 1,803, loader.c 1,942 (firmware/pflash/ELF), numa.c 898, bus/clock/reset/cpu-common; eif.c 671 not needed. qdev API header `include/hw/core/qdev.h` 1,154.
- Memory API: `include/system/memory.h` 2,866 + `system/memory.c` 3,676 + `physmem.c` 4,579 + ioport 371 + memory_mapping 376.
- Main loop/BQL/iothreads: `util/main-loop.c` 691, `aio-posix.c` 858, `async.c` 760, fdmon (epoll + io_uring 522), `iothread.c` 429 → ≈ 3.5k.
- Coroutines: `util/qemu-coroutine*.c` 1,064 + `util/coroutine-ucontext.c` 359 (manual `makecontext`/`swapcontext` stack switching).

Main internal APIs: `TypeInfo`/`type_init`, `ObjectProperty` graph; `DeviceClass.realize/unrealize`, Resettable, `BusState`, qdev property parsing; `MemoryRegion`/`MemoryRegionOps`, `MemoryListener`, `AddressSpace` + RCU-protected FlatView, `address_space_{rw,ld,st}`, `RAMBlock`, dirty bitmap; `bql_lock`, `AioContext`/`aio_poll`/BH; `qemu_coroutine_create/switch`.

Hard parts: a design problem, not a translation problem — every other subsystem calls into it. Object lifetime/refcounts across threads, BQL vs AioContext lock discipline, RCU flatview rebuilds, dirty-memory tracking, and the coroutine model the block layer, VNC jobs and QMP are all written against (a Rust rewrite maps that to async/tasks: re-architecture). qdev property syntax is user-visible ABI (vmlab emits `-device` strings verbatim). Highest concurrency in tree.

Rust upstream: the seams exist — `rust/qom` (990), `rust/hw/core` "hwcore" (758, qdev.rs 459), `rust/system` (337, sysbus + memory), `rust/bql` (900), `rust/qemu-macros` (1,491), `rust/bindings` (653 + bindgen); devices ported: pl011 (1,159), hpet (1,197). No Rust replacement for the C core.

## 2. KVM accel and vCPU thread model

Size ≈ 15k: `accel/kvm/kvm-all.c` 4,809 + kvm-accel-ops 128; `target/i386/kvm/kvm.c` 7,142 (includes the Hyper-V enlightenments behind vmlab's 12 `hv-*` flags) + vmsr_energy 331; drop xen-emu 1,979, tdx 1,636; `target/arm/kvm.c` 2,664; vCPU threads in `system/cpus.c` 683 + `runstate.c` 1,183.

Main internal APIs: `kvm_vm_ioctl`/`kvm_vcpu_ioctl` (`kvm-all.c:3682`), the per-vCPU `KVM_RUN` loop (`kvm-all.c:3489`), `KVMMemoryListener` → `KVM_SET_USER_MEMORY_REGION` (`kvm-all.c:2105`, 2891–3099), `kvm_arch_put/get_registers` state sync, irqfd/ioeventfd, split irqchip routing.

Hard parts: vCPU threads run `KVM_RUN` outside the BQL, kicked by signal; register/MSR sync points; guest-visible Hyper-V enlightenment semantics; high concurrency (per-vCPU threads + BQL + memory listener). The KVM UAPI itself is a stable external ABI, which helps.

Rust upstream: none — but best-trodden path in the Rust VMM world (rust-vmm `kvm-ioctls`/`kvm-bindings`; cloud-hypervisor/crosvm run this exact loop).

## 3. TCG

Size ≈ 200k (estimate for the 3-arch subset; >half is target/arm):
- Core: `tcg/*.c` 22,132 (tcg.c 7,066, tcg-op-gvec 4,361, optimize 3,357, tcg-op 2,732, tcg-op-ldst 1,348, region 929) + host backend `tcg/x86_64/tcg-target.c.inc` 4,657 (aarch64 backend 3,760 if ARM hosts) + `accel/tcg/*.c` 12,229 (cputlb.c 2,898, tcg-runtime-gvec 1,558, user-exec 1,271, tb-maint 1,225, cpu-exec 1,098, translator 584).
- Frontends: `target/i386` 82,685 incl. headers (tcg/ 11,554: translate 3,709, fpu_helper 3,393, seg_helper 2,535); `target/arm` 143,260 (tcg/*.c 82,660: translate-a64 11,310, translate-sve 8,976, sve_helper 8,754, mve_helper 3,472, translate 6,995, vfp 3,461, vec_helper 3,412; root .c 28,506); `target/riscv` 54,020 (.c 31,320 + headers).
- decodetree: `scripts/decodetree.py` 1,658 Python; 39 `.decode` files / 18,327 lines tree-wide (arm 15/9,130; riscv 7/1,635; i386 none — hand-switched decoder).

What a Rust TCG means: a runtime JIT — IR (~200 ops), liveness + constant folding, per-host register allocation and emission, TB cache with chaining and self-modifying-code invalidation, the softmmu TLB with its atomic-probe fast path — then three complete ISA emulators, x86 and ARM/SVE among the hardest ISAs that exist. vmlab needs TCG for every foreign-arch guest and any non-KVM host, including correct Windows 9x/XP. No Rust prior art at this scale; upstream `rust/` has nothing. Corner-case fixes still land weekly (tcg/riscv64 and tcg/aarch64 series in this HEAD). A Rust TCG is a standalone multi-year project; the realistic fork keeps TCG in C behind an FFI or drops TCG-arch support.

Rust upstream: none.

## 4. PC platform for legacy guests

Size ≈ 40–43k (ACPI slice is the soft edge, estimate):
- Boards: `hw/i386/pc.c` 1,832, pc_piix 672, pc_q35 609, pc_sysfw 279; chipset: q35 733, i440fx 421, `hw/isa/piix.c` 522, lpc_ich9 945, smbus_ich9 154.
- ACPI: `hw/acpi/` 14,596 (43 files), vmlab slice ≈ 7–8k (estimate; aml-build 2,888, cpu 727, ich9 522, pci 353, core + TPM2 tables) + `hw/i386/acpi-build.c` 2,385.
- fw_cfg 1,322 (the SeaBIOS/OVMF interface; `bootindex` wiring in pc.c); IDE/AHCI 7,234 of `hw/ide` 10,296; fdc 2,461 (needs i8257 DMA); VGA 2,380 + vga-pci 426 + vga-isa 127 + cirrus_vga 3,027; e1000 1,767; pcnet 1,744+294; isa-serial 994+201; i8259 458, i8254 385, i8257 673, hpet 791, ioapic 518, apic 1,193, rtc 1,046.

Main internal APIs: qdev machine initfn, PCI BAR/ISA registration, fw_cfg entries, AML builders, `qemu_irq` routing, per-device VMStateDescription.

Hard parts: almost no concurrency (BQL-serialized) and no unsafe memory — the difficulty is guest-visible ABI density: register layouts SeaBIOS/OVMF and Windows 9x/XP drivers poke, ACPI table shapes picky old firmware parses, cirrus BLT and e1000 shadow-register bug-compatibility. Volume plus correctness archaeology, not architecture. `intel_iommu.c` 5,751 not needed (VT-d unused).

Rust upstream: none; `rust/hw/timer/hpet` (1,130) is the template for simple devices.

## 5. Block layer

Size ≈ 50k (core measured + support estimate): `block.c` 8,537, `block/block-backend.c` 2,876, `blockdev.c` 3,771; qcow2 17,700 (7 files incl. qcow2.h); `block/file-posix.c` 4,773, raw-format 682, `block/snapshot.c` 785; jobs: `job.c` 1,277, job-qmp 188, blockjob 628; `qemu-img.c` 6,049; io.c/qiov/accounting ≈ 3–5k (estimate). (`block/` is 82,342 over 77 files — the rest is drivers vmlab never uses.)

Main internal APIs: `BlockDriver.bdrv_co_*`, BlockBackend `blk_co_pread/pwrite`, the BDS node graph with its lock discipline and drain, AioContext attachment, qcow2 L1/L2/refcount metadata, `qcow2_snapshot_*`, the block-job machine behind QMP `snapshot-save/load/delete` + `query-jobs`/`job-dismiss`.

Hard parts: every I/O path is a coroutine (couples to §1); drain and graph-modify semantics are historically bug-rich; qcow2 must stay byte-compatible with existing images (vmlab templates, rebase/freeze chains); `qemu-img` flags and JSON output are ABI vmlab parses (`src/template/qimg.rs`), so the CLI must be reimplemented or vmlab changed. Internal snapshots embed device state, coupling to §6. Not checked: which AIO engine vmlab images use (assumed `aio=threads`).

Rust upstream: none. Read-only qcow2 exists in Rust (crosvm's qcow crate); the writable path with internal snapshots does not.

## 6. Migration / vmstate

Size ≈ 9k in `migration/` (estimate): savevm.c 3,814, vmstate.c 970, vmstate-types.c 955, qemu-file.c 911, global_state.c 154, options/params and snapshot-job plumbing — of 35,583 total (44 files; network migration, multifd, RDMA, TLS, compression, cpr droppable). Plus `VMStateDescription` tables scattered through every device (not counted — not checked) and `include/migration/vmstate.h` 1,357.

Main internal APIs: `vmstate_register` (`savevm.c:946`) and the SaveStateEntry list; the `VMStateDescription` DSL with `version_id`/`minimum_version_id` and subsections; the `QEMUFile` byte stream (`save_section_header`, `savevm.c:1058`); driven by the snapshot-save/load/delete QMP jobs.

Hard parts: the densest compatibility contract per line of code in the port. qcow2 internal snapshots already on disk (every vmlab template and freeze) embed this exact byte stream — field order, subsection inclusion, version negotiation, section-id allocation order (device creation order). Reproduce it exactly or existing images stop loading. vhost-user-fs state rides the same stream (the QEMU ≥ 8.2 / virtiofsd ≥ 1.11 requirement).

Rust upstream: `rust/migration` (1,175) — upstream is actively building this bridge, emitting the same C format.

## 7. UI: VNC, console, egl-headless, virtio-gpu/virgl/venus

Size ≈ 20k: VNC core ≈ 8,849 (`ui/vnc*.c` minus sasl/vencrypt/ws/stubs; vnc.c 4,361 incl. the LED-state pseudo-encoding vmlab consumes, vnc-jobs 397 worker thread, encoders tight 1,745 / zrle 548 that vmlab's Raw-only client never requests — trimmable to ~5k, estimate); console layer ≈ 3.3k (console.c 1,519, console-gl 212, shader 226, dmabuf 212, input 722, cursor/kbd-state); `ui/egl-headless.c` 303 + egl-helpers 826; virtio-gpu family 8,179 (virtio-gpu.c 2,007, base 357, gl 248, virgl 1,619, rutabaga 1,179, vga 288+51, udmabuf 217, PCI wrappers).

Main internal APIs: `QemuConsole`/graphic_hw_ops, pixman surfaces and GL scanout via dma-buf (`include/ui/dmabuf.h`), `qemu_input_*` and LED events, VNC 3.8 server, `virgl_renderer_*` C API (e.g. `virtio-gpu-virgl.c:188`).

Hard parts: the protocol sides are easy — vmlab's own RFB client is already Rust, auth-less, Raw encoding. The fragility is host-GPU interop: virgl/venus mean calling virglrenderer (C, no official Rust bindings) and EGL/GBM dma-buf import per vendor — the vmlab fork exists precisely because QEMU 11.1 egl-headless mishandles NVIDIA external-only dma-bufs (the 6 HEAD patches). A rewrite inherits that vendor treadmill, plus QMP `screendump` fallback for GL displays. Moderate concurrency (VNC worker thread + BQL).

Rust upstream: none for `ui/`, but upstream merged `virtio-gpu-rutabaga.c` calling `rutabaga_gfx_ffi` (`meson.build:1430`) — crosvm's Rust GPU stack behind a C shim. That is the interop pattern a Rust port would reuse (bind virglrenderer directly, or route through rutabaga). Community RFB crates exist (quality not checked).

## 8. QMP/QAPI

Size: generator `scripts/qapi/*.py` 5,979 (16 modules) + `qapi/*.json` schemas; generated C is 190 files / 99,804 lines in `build-b/qapi/`; `monitor/` 5,364 (8 files — vmlab needs qmp.c 809, monitor.c 775, qmp-cmds 212, qmp-cmds-control, fds; HMP 1,807 + hmp-cmds droppable); `qobject/` 3,698.

Hard parts: none structural. The contract is the QMP JSON protocol vmlab codes against — 19 commands, 6 events (inventory §7), capability negotiation, error shapes, event ordering. In Rust the generator and its 100k generated lines collapse into serde types for the used slice plus a dispatcher: ~8–12k handwritten lines (estimate). No unsafe, no concurrency beyond BQL.

Rust upstream: none; least needed.

## 9. vhost-user, vfio-pci, USB passthrough

Size ≈ 28k: virtio core (used by every virtio device anyway) virtio.c 4,746 + virtio-pci 2,678 + virtio-bus 375; vhost.c 2,659 + vhost-user.c 3,498 + vhost-user-fs 458 + vhost-user-base 434 + fs-pci; vfio subset ≈ 9–10k (estimate) of 17,741 — pci.c 4,169, listener 1,312, one container backend (legacy 1,269 or iommufd 1,071), quirks 1,599 (GPU passthrough); dropping ccw/spapr/ap/igd 1,003; USB: core 832 + bus 747 + desc 813 + `host-libusb.c` 1,965 + hcd-xhci 3,688 + nec 77 + dev-hid (usb-tablet) 876.

Main internal APIs: `vhost_dev_init` and the vhost-user message protocol (virtiofsd is already an external Rust process); MemoryListener-backed DMA mapping for vfio; VFIO/IOMMUFD ioctls, eventfd INTx/MSI-X; libusb async transfers on the aio loop; xhci rings + streams; qdev hot-plug behind QMP `device_add` (vmlab hot-plugs `usb-host`).

Hard parts: vhost-user device-state transfer in the snapshot stream (the ≥ 8.2/≥ 1.11 pairing vmlab enforces); usb-host state must also survive snapshot-save (PRD:302); vfio kernel-ABI mapping of QEMU's RAM topology and GPU reset/recovery. Concurrency: libusb event thread + vhost queues outside BQL. Best Rust prior art of any subsystem: rust-vmm vhost-user crates, vfio-bindings + pci crate, crosvm's xhci and host USB.

Rust upstream: none in-tree.

## Ranking, easiest to hardest

1. **QMP/QAPI** — codegen collapses into serde; small used slice; only contract is JSON vmlab already speaks.
2. **VNC + console + input** — simple auth-less protocol, Raw-only client, one worker thread; trimmable to ~5k.
3. **KVM + vCPU threads** — proven rust-vmm path; guest-visible parts (Hyper-V flags) are enumerable.
4. **vhost-user / vfio-pci / USB** — protocol/passthrough with strong Rust precedent; xhci is the only big emulated piece.
5. **PC platform** — no concurrency, but ~40k lines of guest-visible ABI archaeology (win9x/XP pickiness, ACPI).
6. **virtio-gpu GL stack (virgl/venus, egl-headless, dmabuf)** — small, but forever bound to C virglrenderer and vendor GPU quirks; bind-don't-rewrite, and it is where vmlab already carries out-of-tree patches.
7. **Block layer** — 50k coroutine-drenched lines, qcow2 byte-compat, plus the qemu-img CLI surface vmlab parses.
8. **Migration/vmstate** — smallest line count, hardest constraint: the snapshot byte stream woven through every device's state description.
9. **Core object model / memory / BQL / main loop / coroutines** — not a port but a redesign every other subsystem sits on.
10. **TCG** — a multi-year JIT project covering x86, ARM/SVE and RISC-V with no Rust prior art; >half the total code.

## Total Rust lines

Basis: measured C subsets sum to ≈ 394k (25+15+200+42+50+9+20+5+28). Applying per-subsystem expansion (≈1:1 to 1.2:1 for emulated device and target code; redesign-not-translation for §1; §8's 106k generated lines collapse to ~10k; bind-don't-rewrite for virglrenderer/libusb/vfio keeps their FFI thin):

- Without TCG (drop foreign-arch/non-KVM support): **180–260k Rust lines**.
- Including TCG: **370–490k**, central estimate ~430k.

Calibration against the Rust VMMs cloned beside this one: crosvm 376,982, cloud-hypervisor 205,525, firecracker 131,034, libkrun 109,866 Rust lines — full-featured VMMs without TCG, without a writable qcow2 path, without QMP and without Windows-legacy devices land squarely in the no-TCG band.

## Not checked

- Per-device VMStateDescription line totals outside `migration/`.
- Which AIO engine vmlab images use (assumed `aio=threads`).
- TPM stack sizing (tpm_tis_common 894 + tis_isa 202 + tis_sysbus 181 + tpm_emulator, unmeasured — small; swtpm is external).
- netdev `stream`/`tap` backends and chardev socket backend sizing (small).
- gdbstub, semihosting, tracing backend (`-trace monitor_qmp_cmd_in_band` is used by an e2e lab), qemu-io (test-only per inventory).
- Whether snapshot-save is refused with a vfio GPU attached.
- Community Rust crate quality for RFB/virglrenderer.

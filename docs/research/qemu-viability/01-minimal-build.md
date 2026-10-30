# Slice A: minimal build measurement

QEMU 11.1.50 (commit d8a6048aae, upstream + vmlab egl-headless patches) built from the
clean clone at `qv/qemu`. Host: gcc 16.2.1, 32 threads (Ryzen 9 9950X), Arch Linux,
NVIDIA GB202 GPU. Build dir: `qv/qemu/build-min`. No features failed to build; one
Kconfig fix was needed after the first build (A3).

## 1. Configure line

```
../configure --target-list=x86_64-softmmu,aarch64-softmmu,riscv64-softmmu \
  --without-default-features --without-default-devices \
  --with-devices-x86_64=vmlab --with-devices-aarch64=vmlab --with-devices-riscv64=vmlab \
  --enable-tcg --enable-kvm --enable-vnc --enable-pixman --enable-opengl \
  --enable-virglrenderer --enable-vhost-user --enable-tpm --enable-libusb \
  --enable-tools --enable-linux-aio --enable-fdt=system --enable-spice
```

`--without-default-features` disables everything else (audio, slirp, gtk/sdl/curses,
docs, guest-agent, xen, plugins, seccomp, capstone, crypto backends, numa, bpf, zstd,
lzo/snappy, curl/nfs/iscsi/rbd/ssh, usb-redir, smartcard, hmp, fuse, io_uring, ...).
Detected and enabled: spice-server 0.16.0, virglrenderer 1.3.0, epoxy 1.5.10,
gbm 26.2.4, libusb 1.0.30, libfdt 1.8.1, linux-aio, pixman, vhost-user, kvm, tcg, tpm.
iasl is not installed; it only affects `bios-tables-test` diffs, not the build.

Build wall times: configure 7 s; full `ninja -j 32` 39 s (1801 edges); rebuild after
the device-config fix 17 s.

### Adjustments recorded (in build order)

- A1 **spice left enabled** (brief said disable): `qxl-vga` is in the inventory and
  `hw/display/Kconfig` gates `QXL` on `SPICE && PCI && PIXMAN`, so QXL cannot be built
  without the spice feature. Cost measured: 9 files, 7,012 lines (qxl* 3,357;
  ui/spice-* 2,940; audio/spiceaudio.c 315; chardev/spice.c 400).
- A2 **libusb, pixman, tools re-enabled**: usb-host is `hw/usb/host-libusb.c` (libusb);
  the VNC server `require(pixman)` (meson.build:1958); `--enable-tools` is the only
  way to get qemu-img — tools is all-or-nothing and also builds qemu-nbd,
  qemu-pr-helper, qemu-bridge-helper, qemu-edid. qemu-storage-daemon and the qemu-vnc
  D-Bus tool were still not built.
- A3 **TPM emulator backend missing after first build**: `-tpmdev help` printed "No TPM
  backend types are available". Cause: `TPM_EMULATOR` is `default y` in
  `backends/tpm/Kconfig:11`, and `--without-default-devices` suppresses default-y
  symbols, so `tpm_emulator.c` was silently skipped. Fix: added `CONFIG_TPM_EMULATOR=y`
  to all three device configs, rebuilt; `-tpmdev help` then lists `emulator`.

## 2. Device config used

`--with-devices-<arch>=vmlab` resolves to `configs/devices/<arch>-softmmu/vmlab.mak`
(three files, 41/31/31 lines, written into the clone working tree). Symbols set:

| Kconfig symbol | x86_64 | aarch64 | riscv64 |
|---|---|---|---|
| Q35, I440FX (machines q35+pc) | y,y | - | - |
| ARM_VIRT / RISCV_VIRT (machine virt) | - | y | y |
| FDC_ISA (floppy) | y | - | - |
| PFLASH_CFI01 (cfi.pflash01) | y | y | y |
| E1000_PCI, PCNET_PCI | y,y | - | - |
| VGA_PCI (VGA) | y | y | y |
| VGA_CIRRUS (cirrus-vga) | y | - | - |
| QXL (qxl-vga; needs spice) | y | - | - |
| VIRTIO_VGA (virtio-vga/-gl) | y | - | - |
| VIRTIO_PCI, VIRTIO_BLK, VIRTIO_NET | y | y | y |
| VIRTIO_SERIAL, VIRTIO_GPU (-gl via virgl) | y | y | y |
| VHOST_USER_FS (vhost-user-fs-pci) | y | y | y |
| USB_XHCI_PCI (qemu-xhci), USB_HID (usb-tablet) | y | y | y |
| TPM_TIS_ISA (tpm-tis) / TPM_TIS_SYSBUS (tpm-tis-device) | y | y | y |
| TPM_EMULATOR (-tpmdev emulator; see A3) | y | y | y |
| SERIAL_ISA (isa-serial) | y | - | - |
| VFIO_PCI (vfio-pci) | y | y | y |

Implied deps come from Kconfig selects (Q35 → AHCI_ICH9/ICH9 USB; I440FX → PIIX →
IDE_PIIX, ISA_BUS, USB_UHCI). e1000/pcnet/cirrus/qxl/isa-serial/floppy/IDE were scoped
to x86_64 only: the inventory emits them from windows/templeos profiles, which are x86
guests. Out-of-scope scoping, not a build constraint.

## 3. Compiled line counts

Measured from `build-min/compile_commands.json` (2,360 entries) cross-checked against
the objects ninja actually produced (1,388 `.o` files on disk; the other entries are
user-mode, multiprocess, other-arch and test targets with no build edge).

- 1,388 objects → 1,382 distinct C sources = **1,038 repo `.c` files (697,750 lines)**
  + 344 meson-generated `.c` (qapi/trace/hwdata, 174,441 lines, in build-min).
- Headers actually included (from `ninja -t deps`): 1,091 qemu repo headers
  (186,435 lines) + 356 build-generated headers (142,118 lines) + 561 system headers.
- Repo totals (`git ls-files`): `*.c` 4,197 files / 2,098,730 lines; `*.h` 2,863 /
  398,096; `*.inc` 201 / 460,516; combined 7,261 files / 2,957,342 lines.
- So the minimal build compiles **24.7% of repo `.c` files, 33.2% of repo `.c` lines,
  23.6% of all repo C+H+INC lines**. The full compiled set excluding system headers
  (repo + generated .c and .h) is 1,200,744 lines.

### By directory (repo `.c` files actually compiled)

| dir | files | lines | dir | files | lines |
|---|---|---|---|---|---|
| target/ (3 arches) | 129 | 179,646 | hw/riscv | 12 | 8,347 |
| block/ | 49 | 51,814 | hw/ide | 9 | 7,890 |
| util/ | 87 | 30,555 | hw/block | 8 | 6,471 |
| (top-level .c) | 22 | 28,069 | hw/char | 7 | 3,755 |
| migration/ | 35 | 27,872 | hw/input | 3 | 2,947 |
| hw/isa+smbios+pci-host+i2c | 20 | 7,220 | hw/rtc+nvram+tpm+mem+timer | 18 | 6,511 |
| hw/intc | 28 | 22,619 | other hw/ (12 subdirs) | 20 | 3,154 |
| system/ | 28 | 21,542 | ui/ | 39 | 24,359 |
| tcg/ | 9 | 21,004 | crypto/ | 32 | 10,778 |
| hw/virtio | 22 | 17,987 | net/ | 21 | 8,975 |
| accel/ | 33 | 16,314 | chardev/ | 20 | 8,019 |
| hw/display | 23 | 15,200 | io/ | 13 | 6,765 |
| hw/acpi | 41 | 14,467 | nbd/ | 4 | 5,615 |
| hw/vfio | 17 | 13,745 | fpu/ + backends/ | 18 | 10,321 |
| hw/core | 31 | 13,338 | disas/ + gdbstub/ | 12 | 7,934 |
| hw/arm | 8 | 11,655 | qapi/ + qom/ + qobject/ | 34 | 12,039 |
| hw/usb | 13 | 11,383 | monitor/ + replay/ + semihosting | 26 | 7,375 |
| hw/i386 | 25 | 10,952 | contrib/ + subprojects/ | 13 | 9,209 |
| hw/pci | 15 | 10,265 | audio/ (core only, no backends) | 7 | 3,807 |
| hw/net | 8 | 9,482 | stubs/ + dump/ + scsi/ + misc | 79 | 8,350 |

Notable: `target/` + `tcg/` + `accel/` = 216,964 lines = 31% of compiled repo C —
the three CPU frontends and TCG dominate what remains. `hw/` total is 197,388 lines
(28%), of which the boards themselves (hw/i386, hw/arm, hw/riscv) are only 30,954.
Core infra (block, migration, ui, system, util, io, net, chardev, crypto, qom/qapi,
monitor, etc.) is ~283k lines and is the part a port-to-Rust cannot avoid.

## 4. Binary sizes

| binary | on disk (debug info) | stripped (`strip --strip-all`) |
|---|---|---|
| qemu-system-x86_64 | 51,145,896 | 11,854,072 |
| qemu-system-aarch64 | 73,327,568 | 13,227,000 |
| qemu-system-riscv64 | 50,634,768 | 10,457,880 |
| qemu-img | 9,935,080 | 1,914,136 |

Also built: qemu-nbd, qemu-pr-helper, qemu-bridge-helper, qemu-edid.

## 5. Verification — binary starts, inventory devices present

`-device help` per machine (distinct user-creatable devices): x86_64 q35 253, x86_64
pc 253, aarch64 virt 35, riscv64 virt 33. Every inventory device was checked by exact
name in the list:

- x86_64 (q35 and pc): virtio-blk-pci, ide-hd, ide-cd, isa-fdc/floppy, virtio-net-pci,
  e1000, pcnet, VGA, virtio-vga, virtio-vga-gl, virtio-gpu-pci, virtio-gpu-gl-pci,
  qxl-vga, cirrus-vga, virtio-serial-pci, virtserialport, isa-serial, qemu-xhci,
  usb-tablet, usb-host, tpm-tis, vfio-pci, vhost-user-fs-pci — all present.
- aarch64 virt and riscv64 virt: virtio-blk-pci, virtio-net-pci, virtio-gpu-pci,
  virtio-gpu-gl-pci, VGA, virtio-serial-pci, virtserialport, qemu-xhci, usb-tablet,
  usb-host, tpm-tis-device, vfio-pci, vhost-user-fs-pci — all present.
- `virtio-gpu-gl` is not a listed name; QEMU registers it as virtio-gpu-gl-pci /
  virtio-gpu-gl-device and the bare alias resolves (`-device virtio-gpu-gl,help`
  prints options). Same alias pattern as virtio-blk.
- Machines: q35, pc (pc-i440fx-11.2), virt (arm, virt-11.2), virt (riscv) all listed.
- Display backends: none + egl-headless (VNC is via `-vnc`, always compiled).

**Missing-device list: empty.**

Functional smokes (all with `-accel tcg`, killed by timeout = expected exit 124):

- Full vmlab-style q35 line: two pflash drives with
  `-global driver=cfi.pflash01,property=secure,value=on`, qcow2 blockdev, virtio-blk,
  `-netdev stream,server=off,addr.type=unix,...`, virtio-serial + virtserialport
  `vmlab.agent.0`, qemu-xhci + usb-tablet, VGA, `-vnc unix:`, `-qmp unix:,server`,
  `-trace monitor_qmp_cmd_in_band` — starts clean, VNC and QMP unix sockets created.
- TPM: swtpm (`--tpm2 --ctrl type=unixio`) + `-tpmdev emulator` + `tpm-tis` (x86) and
  `tpm-tis-device` (aarch64): both connect and stay up.
- QMP snapshot round-trip (x86): `qmp_capabilities`, `snapshot-save`, `snapshot-load`,
  `query-jobs`, `job-dismiss` all succeed; qcow2 holds internal snapshot `s1` with
  vm-state 695,731 bytes.
- egl-headless + `virtio-vga-gl` on the GB202 render node starts without error.
- aarch64 virt + virtio-gpu-pci and riscv64 `virt,acpi=off` + virtio-gpu-pci stay up.
- qemu-img `create -f qcow2` and `info --output=json` work.

Caveat (not a vmlab gap): a bare machine without `-netdev/-nic` prints
`warning: Default NIC 'e1000e' is not available in this binary`. e1000e is not in the
inventory and vmlab always names its NICs or passes `-nic none`.

## 6. Not checked

- Stock default build for comparison (optional per brief; repo totals above used instead).
- `-netdev tap,fd=N` (needs a pre-opened tap fd; stream netdev tested instead).
- vhost-user-fs-pci against a live virtiofsd (device present; virtiofsd is a separate
  Rust binary, not built here).
- usb-host hot-plug via QMP `device_add` with real host hardware (device and libusb
  linked; not exercised).
- VNC wire protocol/encodings from a client (sockets open only; vmlab's RFB client
  not run against this binary).
- qemu-img subcommands beyond create/info (rebase/convert/snapshot/commit/resize).
- s390x/ppc64/loongarch64 targets (accepted by vmlab config, no firmware/container
  support in vmlab; outside this slice).

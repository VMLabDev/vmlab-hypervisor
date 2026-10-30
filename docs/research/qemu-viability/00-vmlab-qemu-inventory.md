The inventory is below, built from reading the code. Unless a line says "PRD" or "example", it is something vmlab actually emits or calls. Lab authors can add any QEMU argument through the `qemu_args` escape hatch, which vmlab appends unchanged as the last arguments (`src/qemu/cmdline.rs:722`). A stripped fork would therefore break any user lab that leans on a removed feature. The repo's own e2e labs use it for `-smbios type=1,...` (`e2e/labs/core-lab/vmlab.wcl:22`) and `-trace monitor_qmp_cmd_in_band` (`e2e/labs/scripting-lab/vmlab.wcl:29`).

## 1. Architectures and machine types
- **Binary name**: always `qemu-system-{arch}` (`cmdline.rs:280`). `x86` is only a label and runs on `qemu-system-x86_64` (`cmdline.rs:102`).
- **Arches the config accepts**: x86_64, x86, aarch64, riscv64, loongarch64, s390x, ppc64 (`src/config/model.rs:1156-1164`).
  - Only x86_64, aarch64 and riscv64 have firmware tables (`firmware.rs:83-91`) and container support (`resolve.rs:633-637`).
  - loongarch64, s390x and ppc64 just pass validation and would get `-machine virt` (`resolve.rs:494-496`). No test covers them.
- **Machine types**:
  - x86: `q35` or `pc` (i440fx), taken from the profile, default q35 (`profiles/mod.rs:25-30`, `resolve.rs:482-489`).
  - riscv64: `virt,acpi=off` (`resolve.rs:490-493`).
  - Every other arch: `virt` (`resolve.rs:496`).
  - Containers: `q35` on x86_64, `virt` on aarch64, `virt,acpi=off` on riscv64 (`resolve.rs:633-637`).
  - When virtiofs is used, the machine gets `memory-backend=mem0` added (`cmdline.rs:335-347`).
  - microvm and isapc: not found.

## 2. Accelerators
- Only `kvm` and `tcg` exist (`model.rs:577`). A profile test rejects `accel = "hvf"` (`profiles/mod.rs:387`). hvf and whpx: not found.
- **When TCG is used** (`cmdline.rs:110-170`):
  - KVM is chosen only when `/dev/kvm` opens read/write and the guest arch matches the host arch (`cmdline.rs:120-180`).
  - Otherwise TCG is used, with a warning (`labd/vm.rs:994`). Foreign-arch guests therefore always run on TCG.
  - A VM can force `accel = "tcg"`, or force `"kvm"` and get a refusal instead of a fallback.
  - Containers use `pick_accel` with no override (`container.rs:68`).
- **CPU model**:
  - KVM: `-cpu host`, or `host,vmx=off,svm=off` on x86 unless the VM sets `nested` (`cmdline.rs:266-278`).
  - With `hyperv = true` on x86 under KVM, the `hv-*` flags are added: relaxed, vapic, spinlocks, vpindex, runtime, time, synic, stimer, frequencies, reset, tlbflush, ipi (`cmdline.rs:219`).
  - TCG: `-cpu max`.
  - A user-set `cpu_model` (e.g. `pentium3`) replaces either default (`cmdline.rs:357`).
- **Other flags**: `-smp`, `-m`, `-rtc base=<instant|localtime>` (`cmdline.rs:363`), `-S` (`cmdline.rs:719`), `-name` (`cmdline.rs:324`).

## 3. Devices
**Storage**
- `virtio-blk-pci`: disks; CD-ROMs on `virt` (`cmdline.rs:506`, `:559`).
- `ide-hd` and `ide-cd` on `bus=ide.N`: q35 AHCI or pc PIIX IDE (`cmdline.rs:515`, `:563`).
- Floppy via `-drive if=floppy,format=raw` (`cmdline.rs:568-574`).
- Containers: `virtio-blk-pci` for rootfs and scratch (`container.rs:210`, `:217`).

**NICs**
- The model string comes from the profile (`cmdline.rs:599-603`):
  - `virtio-net-pci`, with `host_mtu=` when the MTU is not 1500.
  - `e1000` (windows-legacy, windows-xp, templeos).
  - `pcnet` (windows-9x) (`profiles/shipped.wcl:22,70,85,103,123`).
- `rtl8139` appears only in a schema comment (`profile_schema.wcl:20`). ne2k: not found.
- No NIC gives `-nic none` (`cmdline.rs:607`).

**Display**
- The profile `display` value maps as follows (`resolve.rs:441-459`):
  - `std` → `VGA`
  - `virtio-vga` → `virtio-vga` on x86, `virtio-gpu-pci` elsewhere
  - `virtio-gpu` → `virtio-gpu-pci`
  - `qxl` → `qxl-vga`
  - anything else is passed through as written, e.g. `cirrus-vga` (windows-9x, `shipped.wcl:104`).
- GPU modes (`cmdline.rs:629-678`):
  - virgl: `virtio-vga-gl` on x86, `virtio-gpu-gl` plus `-vga none` on virt.
  - vulkan: the same devices with `venus=true,blob=true,hostmem=4G`.
  - passthrough: `vfio-pci,host=<addr>`.
- Containers: `-vga none` (`container.rs:130`).

**Serial / console**
- `virtio-serial-pci` with `virtserialport,name=vmlab.agent.0` (`cmdline.rs:459-464`). Containers also get `vmlab.ctl.0` (`container.rs:187-200`).
- `isa-serial` index 0 (agent) and index 1 (serial log) on legacy profiles (`cmdline.rs:401-408`, `:466-471`).
- Otherwise `-serial file:` (`cmdline.rs:409`, `container.rs:153`).

**USB**
- `-usb` on x86, `qemu-xhci` (optionally `id=usbx`), and `usb-tablet` (`cmdline.rs:690-700`).
- Hot-plugged `usb-host` with hostbus, hostaddr and hostport via QMP `device_add` (`src/usb/mod.rs:523-532`).

**TPM**
- `tpm-tis` on x86, `tpm-tis-device` elsewhere, both on `-tpmdev emulator` (`cmdline.rs:612-625`).

**Filesystem**
- `vhost-user-fs-pci` (`cmdline.rs:704-715`, `container.rs:249-259`).

**Not found**: sound devices (no `-audiodev`, hda, ac97 or sb16), pvpanic, vsock, virtio-rng, balloon, virtio-input, virtio-scsi, nvme, ramfb.

## 4. Netdev backends
- `-netdev stream,server=off,addr.type=unix,addr.path=...`, connecting to vmlab's own userspace switch (`cmdline.rs:580-587`, `container.rs:224-229`).
- `-netdev tap,fd=N` with a pre-opened fd, the afxdp/ebpf fast path (`cmdline.rs:588-590`, `process.rs:28,86-102`).
- Not found: user/slirp, socket, dgram, vhost-user-net, `vhost=on`, bridge.
- SMB shares are served by a host `smbd` over the lab network, not via QEMU (`process.rs:308-320`).

## 5. Block
- **Blockdev drivers**: `qcow2` (disks, container scratch) and `raw` with `read-only=on` (ISOs, squashfs rootfs). Both sit on `file.driver=file` (`cmdline.rs:494-498`, `:541-544`, `container.rs:204-216`).
- **Legacy `-drive`**: `if=pflash,format=raw` for firmware and `if=floppy,format=raw`.
- **qemu-img subcommands** (`src/template/qimg.rs`):
  - `create -f qcow2`, plain and with `-b`/`-F qcow2` (`:91`, `:105`)
  - `rebase -u` and safe `rebase` (`:118`, `:128`, `store.rs:728`)
  - `info -U --output=json` (`:140`)
  - `resize` (`:152`)
  - `convert -O qcow2` (`:160`) and `convert -l snapshot.name=... -B ... -F qcow2` (`:195`)
  - `snapshot -c/-a/-d` (`:168-182`)
  - `commit -q -d` (`:205`)
  - `qemu-io` appears only in tests (`src/freeze/qemu_tests.rs`, `oci/client.rs:2476`).
- **Snapshots**: always qcow2-internal.
  - Online: QMP `snapshot-save`, `snapshot-load` and `snapshot-delete` as jobs, with vmstate on `disk0` and devices `disk0..N` (`qmp/client.rs:367-423`, `labd/vm.rs:1364,1409,1442`). Containers use vmstate `scratch` (`labd/container.rs:1058,1083,1136`).
  - Offline: `qemu-img snapshot`.
  - Job polling uses `query-jobs` and `job-dismiss` (`client.rs:429-459`).
  - The legacy `savevm`/`loadvm`: not found. External snapshots and block jobs (mirror, commit, stream, backup): not found. The PRD allows external snapshots but none are implemented (`docs/vmlab-prd.md:477`).

## 6. Display / UI
- Always `-display none` plus `-vnc unix:<sock>,share=ignore` (`cmdline.rs:372-386`). TCP VNC: not found in the code; the PRD calls it optional (PRD:836).
- With a virgl/vulkan GPU, `-display egl-headless[,rendernode=/dev/dri/renderDN]` (`cmdline.rs:666-670`).
- vmlab's VNC client is RFB 3.8 with no auth (`src/vnc.rs`). It uses:
  - encodings Raw, DesktopSize, AlphaCursor (-314) and RichCursor (-239)
  - QEMU's LED-state pseudo-encoding (-261)
  - tolerated without use: -224, -258, -259 (`vnc.rs:45-58`, `:207`).
- The recorder streams over the same VNC socket (`src/record/executor.rs:69`). Screenshots use QMP `screendump`, falling back to VNC on GL displays (`labd/display/mod.rs:105-128`).
- Containers have no VNC (`container.rs:129`).
- SPICE is deferred (PRD:836). The only `"spice"` string is a test fixture (`template/meta.rs:1073`). gtk, sdl, dbus and an explicit dmabuf option: not found. The PRD mentions egl-headless dma-buf behaviour (PRD:250).

## 7. Chardevs, monitor, QMP
- **Chardevs**: `socket` (server for agent/ctl; client for swtpm and virtiofsd) and `file` (serial log) (`cmdline.rs:404,449-455,614,707`).
- **Monitor**: `-qmp unix:...,server=on,wait=off` and `-monitor none` (`cmdline.rs:388-394`).
- **QMP commands sent** (`qmp/client.rs`):
  - `qmp_capabilities`
  - power: `system_powerdown`, `system_wakeup`, `quit`, `stop`, `cont`
  - `query-status` (only the QMP tests poll it)
  - media: `eject`, `blockdev-close-tray`
  - display and input: `screendump`, `send-key`, `input-send-event` (abs, btn)
  - hot-plug: `device_add`, `device_del`
  - snapshots: `snapshot-save`, `snapshot-load`, `snapshot-delete`, plus `query-jobs`, `job-dismiss`
  - `query-name` appears only in tests.
- **QMP events consumed**: SHUTDOWN (`hypervisor/qemu.rs:161`), SUSPEND, WAKEUP, RESET, RESUME (`hypervisor/mod.rs:222-227`, so ACPI S3 matters), DEVICE_DELETED (`client.rs:344`).
- **QGA**: not used. The "guest-ping" comment refers to vmlab's own agent over its own virtio-serial port (`labd/vm.rs:274`).

## 8. Firmware, TPM, secure boot
- **SeaBIOS**: the x86 default; nothing is added and boot order comes from `bootindex` (`cmdline.rs:439`, `:483`).
- **UEFI**: OVMF on x86_64 (4M plain, secboot, `.ms` VARS), AAVMF on aarch64, `RISCV_VIRT_CODE/VARS` on riscv64. vmlab bundles Debian edk2 and falls back to distro paths (`firmware.rs:81-91`, `:131-259`).
- **Secure boot**: `-global driver=cfi.pflash01,property=secure,value=on` on x86_64 (`cmdline.rs:433-437`). No explicit `smm=on` is set.
- **TPM**: `swtpm socket --tpm2 --tpmstate dir= --ctrl type=unixio,path= --terminate` (`process.rs:284-292`).
- **Containers**: direct `-kernel`/`-initrd`/`-append` boot; console is `ttyAMA0` on aarch64, `ttyS0` elsewhere (`container.rs:136-149`).

## 9. Everything else
- **`-object` types**: only `memory-backend-memfd,share=on`, used when virtiofs is present (`cmdline.rs:340-347`).
- **virtiofsd** is spawned as `--socket-path --shared-dir --cache auto --log-level warn`. When the binary supports it, `--migration-mode find-paths --migration-verify-handles --migration-on-error guest-error` is added (`virtiofsd.rs:343-366`).
  - Requires QEMU ≥ 8.2 and virtiofsd ≥ 1.11 so vhost-user-fs state travels in the migration stream (PRD:514).
  - Without that support, online snapshots of the VM are refused (`virtiofsd.rs:93`, `labd/vm.rs:446-456`).
- **Migration**: not used directly. The `-incoming` flag and `migrate` commands: not found. Online snapshots still rely on the vmstate/migration code inside QEMU: `usb-host` state (PRD:302), and keeping `+invtsc` out of the Hyper-V flags (`cmdline.rs:217`).
- **Audio**: not found; the PRD lists it as out of scope (PRD:860).
- **vhost-user**: only `vhost-user-fs`.
- **Host-GPU overrides**:
  - A GPU can name its own QEMU binary, which is run with `-L <stock share/qemu>` (`cmdline.rs:325`, `hostgpu.rs:398-447`).
  - The PRD says this exists because QEMU 11.1's egl-headless needs a patch for NVIDIA Venus (PRD:250).
  - Vulkan loader environment variables are set in QEMU's environment (`hostgpu.rs:251-289`, `vk_layer.rs:31-37`).
- **Version notes in code and docs**:
  - Several behaviours were checked against QEMU 11.1 (`cmdline.rs:688`, PRD:250, :302).
  - The riscv64 example's README asks for QEMU ≥ 8.1 (`examples/riscv64-ubuntu/README.md:9`).

All paths are relative to `/home/wil/dev/vmlab`.
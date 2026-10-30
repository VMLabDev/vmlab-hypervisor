# q35-seabios-linux-modern

q35 + SeaBIOS disk boot, linux-modern template configuration.

Base profile: `linux-modern with firmware = seabios (Ubuntu 26.04 / AlmaLinux 10)`. The exact QEMU argv is in `qemu-argv.txt`; host facts (QEMU version, CPU, KVM parameters, firmware hashes) in `environment.txt`.

## Contents

`guest/` — collected inside the guest:
- `lspci-nnnvvvxxxx.txt` — every PCI function, full config space
- `lspci-nn.txt`, `lspci-tv.txt`, `proc-pci-devices.txt` — PCI IDs and topology
- `acpi/` — every ACPI table from /sys/firmware/acpi/tables (raw,
  `dynamic-`-prefixed ones from `tables/dynamic/`), plus iasl output;
  `acpi-dsl/iasl.log` records disassembly diagnostics
- `dmidecode.txt`, `smbios.bin`, `dmi-tables/`, `dmi-sysfs.txt` — SMBIOS
- `cpuid.txt` — raw CPUID leaves (hwref-cpuid; Alpine packages no `cpuid`)
- `dmesg.txt`, `e820-dmesg.txt`, `e820-memmap.txt` — boot log and e820 map
- `interrupts.txt`, `iomem.txt`, `ioports.txt` — IRQ/MSI and address maps
- `fwcfg/` — fw_cfg revision and by_name file list (+ small files)
- `pnp.txt` — PNP/ISA devices from /sys/bus/pnp
- `usb.txt`, `serial16550.txt`, `cpuinfo.txt`, `uname.txt`, `modprobe.log`

Boot/console artefacts: `serial.log` (COM1 console: kernel under SeaBIOS,
OVMF's own serial console under UEFI), `agent-channel.log` (the stream on
the `vmlab.agent.0` chardev — the tarball rode it where marked),
`qmp.log`, `qemu-argv.txt`, `qemu-version.txt`, `environment.txt`,
`qemu-stderr.log`.

## Probe deviations

- Boots the same probe kernel and initramfs from a Syslinux FAT32 disk
  through SeaBIOS, with no direct-kernel fw_cfg boot blobs.
- Uses linux-modern's production argv with the templates' SeaBIOS
  override: 2 vCPUs, 4 GiB, virtio-vga, default PS/2 input and
  ICH9 USB tablet. No ROM, BIOS, CPU or USB argument overrides.
  Socket paths, lab/VM name, MAC and probe disk path are run inputs.
- The probe's linux-virt kernel has no virtio_gpu module; PCI config
  still records the virtio-vga device and firmware-selected BARs.


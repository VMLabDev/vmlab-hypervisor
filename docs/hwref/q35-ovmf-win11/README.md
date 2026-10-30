# q35-ovmf-win11

q35 + OVMF, Windows 11 profile devices, TPM (no secure boot).

Base profile: `windows-server (the windows-11 device set, secure boot off)`. The exact QEMU argv is in `qemu-argv.txt`; host facts (QEMU version, CPU, KVM parameters, firmware hashes) in `environment.txt`.

## Refresh provenance

Refreshed on 2026-10-31 for the current probe and NIC ROM-header collector.
The QEMU argv matches the capture at commit `710125e`, with only temporary
socket, disk, VARS and output paths replaced. QEMU 11.1.2, the kernel and the
CODE/VARS inputs are unchanged. The raw DSDT is byte-identical.

The previous initramfs SHA-256 was
`54a1eaaefa7158197cc8d07e8a59e09e3db675ed0021271a8d948bbc39ebd790`;
the new hash and all current asset hashes are in `environment.txt`.
The previous environment-file SHA-256 was
`26f8bedb6e7dde5f73c5ede3e9a33305f12ff7235b540581e017c067635c4aa9`;
the previous argv-file SHA-256 was
`8ac9d7c4373b1b92930901cf02eea6ddcfc7414d744b67b79d412c2d8c2e78e8`.
Git history retains the previous capture. The updated probe inventories NIC
ROM headers and permits bounded, length-checked fw_cfg PIO transfers.

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
- `efivars/` — UEFI variable listing and small variables

Boot/console artefacts: `serial.log` (COM1 console: kernel under SeaBIOS,
OVMF's own serial console under UEFI), `agent-channel.log` (the stream on
the `vmlab.agent.0` chardev — the tarball rode it where marked),
`qmp.log`, `qemu-argv.txt`, `qemu-version.txt`, `environment.txt`,
`qemu-stderr.log`, `swtpm-argv.txt`/`swtpm.log`.

## Probe deviations

- Boots as a real UEFI disk boot: disk0.qcow2 carries an MBR + FAT32
  partition with an unsigned UKI (systemd stub + the same kernel and
  initramfs) at `\EFI\BOOT\BOOTX64.EFI`. No `-kernel`, so the fw_cfg
  file list has no direct-kernel probe additions; the guest is Linux,
  not Windows.


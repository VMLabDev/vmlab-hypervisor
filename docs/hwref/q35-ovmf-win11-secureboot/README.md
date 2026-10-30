# q35-ovmf-win11-secureboot

q35 + OVMF secure boot, Windows 11 profile devices, TPM.

Base profile: `windows-11`. The exact QEMU argv is in `qemu-argv.txt`; host facts (QEMU version, CPU, KVM parameters, firmware hashes) in `environment.txt`.

## Refresh provenance

Refreshed on 2026-10-31 for the current probe and NIC ROM-header collector.
The QEMU argv matches the capture at commit `710125e`, with only temporary
socket, disk, VARS and output paths replaced. QEMU 11.1.2, the kernel and the
CODE/VARS inputs are unchanged. The raw DSDT is byte-identical.

The previous initramfs SHA-256 was
`54a1eaaefa7158197cc8d07e8a59e09e3db675ed0021271a8d948bbc39ebd790`;
the new hash and all current asset hashes are in `environment.txt`.
The previous environment-file SHA-256 was
`3ce493ded8a5214e94db4a416a7517c9e9c2e34ee286b9af95ef2635ce2b35f7`;
the previous argv-file SHA-256 was
`12e57b1511b15d5b9b2b0d53941d23bb785e43174a5219157bde538d414ad42b`.
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
- **Secure boot is captured in setup mode, not enforcing.** The booted
  VARS is the blank `OVMF_VARS_4M.fd` (vmlab's Pairing::Setup); the
  enforcing `OVMF_VARS_4M.ms.fd` (Pairing::Enrolled — what
  `secure_boot = true` actually boots) refuses every unsigned image,
  including this probe. An enforcing capture needs a signed probe. The
  secboot CODE pflash, the `cfi.pflash01,secure=on` global, SMM and the
  firmware configuration are recorded here; enforcement parity is not
  proven (`SecureBoot` is 0 here, 1 under the ms VARS).
  The ms VARS sha256 is in `environment.txt`.


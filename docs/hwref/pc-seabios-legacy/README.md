# pc-seabios-legacy

pc (i440fx) + SeaBIOS, legacy profile device superset.

Base profile: `windows-9x + windows-xp/windows-legacy superset`. The exact QEMU argv is in `qemu-argv.txt`; host facts (QEMU version, CPU, KVM parameters, firmware hashes) in `environment.txt`.

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

- Boots vmlab's own container kernel (`vmlinuz`, Alpine linux-virt
  6.12.114) with an initramfs built from the Alpine minirootfs plus
  pciutils/dmidecode/iasl, via `-kernel`. vmlab boots a real guest disk;
  the device set is identical, but fw_cfg gains the `etc/boot-kernel`,
  `etc/boot-initrd`, `etc/boot-cmdline-size` and `etc/boot-cmdline` files
  (see `guest/fwcfg/sizes.txt`). The blank guest disk and the filler CD
  preserve disk bootindex 50. The legacy CD uses index 1 instead of
  vmlab's 0 because QEMU direct-kernel boot reserves index 0.
- The device set is a superset across the legacy profiles (windows-9x
  cirrus/pcnet/IDE/isa-serial/RTC-localtime plus windows-xp/legacy's
  e1000 and the answer-file floppy). Extra devices change PCI slots;
  this is a superset reference, not each profile's exact topology.
- linux-virt has no pcnet32, floppy or TPM drivers, so those devices
  appear without a bound driver (no /dev/fd0, no pcnet in
  /proc/interrupts). Their guest-visible shape — PCI config space,
  DSDT nodes and the PNP0700/0501 PNP devices — is fully captured.


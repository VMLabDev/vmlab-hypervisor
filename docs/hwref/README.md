# QEMU hardware references

Black-box guest observations for D30 hardware parity and the D26 differential suite.
QEMU source code is not used. The argv follows vmlab's `src/qemu/cmdline.rs`
and `src/profiles/shipped.wcl` at capture time.

## Recapture

Run `uv run scripts/hwref/capture.py` from the repository root.
Pass a configuration name to recapture one; `--list` lists configurations.
Run `uv run scripts/hwref/capture.py --summarize` to regenerate these tables.
Requirements and asset preparation are in `scripts/hwref/README.md`.
Existing results for the selected configuration are replaced.

## Run the P2 differential

Capture into fresh directories to preserve the reference data:

```sh
python3 scripts/hwref/capture.py --output .scratch/hwref-a q35-seabios-linux-generic
python3 scripts/hwref/capture.py --output .scratch/hwref-b q35-seabios-linux-generic
python3 scripts/hwref/capture.py diff \
  .scratch/hwref-a/q35-seabios-linux-generic \
  .scratch/hwref-b/q35-seabios-linux-generic --no-allow-list
```

An empty report and exit 0 mean the normalized observations match. Exit 1
means unapproved differences. Exit 2 means invalid input or a missing required
artefact. `--report PATH` writes the report to a file.
`python3 scripts/hwref/diff.py` accepts the same diff arguments.

To capture the same probe under `vmlab-hv`, supply a resolved phase-2 machine
description and the binary to test:

```sh
python3 scripts/hwref/capture.py --backend vmlab-hv \
  --binary target/debug/vmlab-hv --machine /path/to/machine.json \
  --output .scratch/hwref-hv
python3 scripts/hwref/capture.py diff \
  .scratch/hwref-a/q35-seabios-linux-generic .scratch/hwref-hv
```

The description must select `machine = "q35"` and `boot.kind = "firmware"`.
Use the proto crate's firmware fields for SeaBIOS or OVMF, with its CODE path,
VARS path, secure-boot setting, and boot order. Give the probe disk the ID
`hwref-probe` and include exactly one port named `vmlab.agent.0`.
Set vCPUs, memory, display, NICs, CPU policy, and explicit PCI addresses to
match the reference configuration. The capture retains those device fields.
It replaces the probe disk with the generated firmware-bootable media,
redirects the serial log and agent socket, and connects NICs to local sinks.
Other writable disks get disposable qcow2 overlays. OVMF VARS is copied,
so the original disks and variable store remain unchanged.

The harness launches `vmlab-hv --machine PATH --control SOCKET --paused`,
connects the guest stream, handshakes with protocol version 1, and sends
`resume`. The guest archive has a length and SHA-256 check before extraction.
Missing KVM, the binary, probe assets, or firmware produces a named `SKIP`
and exit 0. A known rejection of q35 or `Boot::Firmware` also skips, with
`SKIPPED.txt` in the output. Other launch errors, timeouts, control errors,
and incomplete archives fail. A skipped output has no `guest/` and cannot
pass the diff command.

New SeaBIOS captures boot the probe from a Syslinux FAT32 disk. They use
SeaBIOS and SeaVGABIOS, disable the vapic ROM and NIC ROMs, and do not load
QEMU's linuxboot ROMs. The checked-in Linux reference used direct-kernel boot.
Its extra fw_cfg boot blobs remain visible differences against disk boots.
OVMF captures use the same kernel and initramfs in a UKI. The unsigned probe
requires setup-mode VARS, as in the checked-in OVMF references.

## Normalization and approved differences

The comparison operates on every file under `guest/`, including unknown
files. Unknown binaries get a byte-for-byte hex diff. Missing files are
reported separately. Host argv, versions, environment, transport archives,
and console logs outside `guest/` remain provenance, not hardware parity
inputs. The decoded archive supplies the guest observations.

Normalization removes log timestamps, audit times, IRQ and serial counters,
MAC addresses, system UUIDs, kernel relocation addresses, and the clocksource
watchdog's scheduling-delay notice. Boot-log lines are sorted to remove
asynchronous log ordering. Native host CPUID leaves and host CPU descriptions
are masked. Hypervisor CPUID leaves and guest CPU counts remain comparable.
PCI BAR allocation bases and MSI destinations are masked in text and raw
config dumps. BAR type bits, sizes, IDs, capabilities, and routing remain.
ACPI FADT table pointers, FACS wake vectors, root-table pointers, BGRT image
addresses, RSDP pointers, and affected checksums are masked in binaries.
SMBIOS UUIDs, native processor IDs, and entry-point addresses are masked in
every binary copy.
Movable ACPI and Linux setup-data boundaries in e820 and iomem are named by
role and size, including adjacent RAM boundaries. Other e820 endpoints,
including the guest RAM ceiling and architectural holes, remain exact.
iasl timestamps, relocated table pointers, checksums, and redundant raw-table
text are removed from disassemblies. Other fw_cfg blobs remain byte-exact.

`scripts/hwref/approved-differences.json` records D6. It permits reference-only
ICH9 USB functions at `00:1d.0`, `.1`, `.2`, and `.7` only with their captured
Intel IDs. It permits a candidate-only `1b36:000d` xHCI function. The filter
applies to PCI inventories, config blocks, topology, resource ranges, and USB
host-controller listings. Changes to a function present on both sides are
never approved automatically. For identity-checked omissions, the AML parser
removes only the corresponding exact `Name (_ADR)` device nodes from a
device-only `_SB.PCI0` scope. Additional methods prevent that normalization.
The ASL nodes, derived DSDT header/log lengths, and table-loader offsets are
adjusted for those removed bytes. Other AML and loader fields remain compared.
IRQ routing and other boot-log differences remain visible for review. The
allow-list does not suppress arbitrary USB-related lines.

The 2a gate also records category and reason metadata for two temporary groups:
`etc/smi/*` and `etc/tpm/*` are **deferred to 2c** and must be removed from the
allow-list when 2c lands. `genroms/kvmvapic.bin` and
`genroms/linuxboot_dma.bin` are **pending owner decision** because these
QEMU-built ROMs are not shipped. These rules permit only reference-only files;
changed files, candidate-only files and failed blob collection still fail.
Their corresponding fw_cfg inventory entries and now-empty top-level
directories are removed. Directory link counts and selector-link numbers are
canonicalized only when such an omission applies. Other files remain compared.

To check that the tool retains real differences:

```sh
python3 scripts/hwref/capture.py diff \
  docs/hwref/q35-seabios-linux-generic docs/hwref/q35-ovmf-win11
```

This exits 1 and reports different GPU IDs, vCPU counts, Hyper-V leaves,
ACPI tables including TPM2, firmware identity, and fw_cfg contents.
Run the Python regression suite with:

```sh
python3 -m unittest discover -s scripts/hwref -p 'test_*.py'
```

Set `HWREF_RECAPTURE=1` on a provisioned KVM host to include the P2 black-box
test. It boots QEMU twice, requires an empty diff with no allow-list, then
changes a captured PCI ID and requires exit 1. Without that opt-in, the test
skips with a prerequisite message.

## Capture contents and limits

Each configuration directory records exact argv, version, host CPU and firmware
hashes, console logs, and a `guest/` directory. The guest records full PCI config
space and topology, raw ACPI tables and iasl output, SMBIOS text and binaries,
CPUID leaves, e820, IRQs, fw_cfg inventory, and PNP resources.
Per-configuration READMEs describe probe deviations and capture failures.

The Linux capture uses `linux-generic` with `usb = true` to include xHCI.
The legacy capture combines devices from several profiles; its slots do not
represent every individual profile. Secure-boot OVMF uses blank VARS in setup
mode because the probe is unsigned. Enforcing secure boot remains uncaptured.
CPUID depends on the host, and IRQ counts, timestamps and dynamic tables vary.
These are Linux-initialized observations, not reset-state device registers.
This package provides QEMU reference data; it does not change S1 or S2 behaviour.

## QEMU version

QEMU emulator version 11.1.2

<!-- Generated by scripts/hwref/capture.py --summarize; do not edit by hand. -->

## q35-seabios-linux-generic

q35 + SeaBIOS, linux-generic profile devices

### PCI functions

| BDF | vendor:device | class | device | subsystem | BARs |
|-----|--------------|-------|--------|-----------|------|
| 00:00.0 | `8086:29c0` | 0600 | Class: Device | 1af4:1100 Device | none |
| 00:01.0 | `1af4:1003` | 0780 | Class: Device | 1af4:0003 Device | I/O ports at c080 [size=64]; Memory at febd4000 (32-bit, non-prefetchable) [size=4K]; Memory at fe000000 (64-bit, prefetchable) [size=16K] |
| 00:02.0 | `1af4:1001` | 0100 | Class: Device | 1af4:0002 Device | I/O ports at c000 [size=128]; Memory at febd5000 (32-bit, non-prefetchable) [size=4K]; Memory at fe004000 (64-bit, prefetchable) [size=16K] |
| 00:03.0 | `1af4:1000` | 0200 | Class: Device | 1af4:0001 Device | I/O ports at c100 [size=32]; Memory at febd6000 (32-bit, non-prefetchable) [size=4K]; Memory at fe008000 (64-bit, prefetchable) [size=16K]; Expansion ROM at feb80000 [disabled] [size=256K] |
| 00:04.0 | `1234:1111` | 0300 | Class: Device | 1af4:1100 Device | Memory at fd000000 (32-bit, prefetchable) [size=16M]; Memory at febd7000 (32-bit, non-prefetchable) [size=4K]; Expansion ROM at 000c0000 [disabled] [size=128K] |
| 00:05.0 | `1b36:000d` | 0c03 | Class: Device | 1af4:1100 Device | Memory at febd0000 (64-bit, non-prefetchable) [size=16K] |
| 00:1d.0 | `8086:2934` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at c120 [size=32] |
| 00:1d.1 | `8086:2935` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at c140 [size=32] |
| 00:1d.2 | `8086:2936` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at c160 [size=32] |
| 00:1d.7 | `8086:293a` | 0c03 | Class: Device | 1af4:1100 Device | Memory at febd8000 (32-bit, non-prefetchable) [size=4K] |
| 00:1f.0 | `8086:2918` | 0601 | Class: Device | 1af4:1100 Device | none |
| 00:1f.2 | `8086:2922` | 0106 | Class: Device | 1af4:1100 Device | I/O ports at c180 [size=32]; Memory at febd9000 (32-bit, non-prefetchable) [size=4K] |
| 00:1f.3 | `8086:2930` | 0c05 | Class: Device | 1af4:1100 Device | I/O ports at 0700 [size=64] |

### ACPI tables

| table | OEM table ID | bytes | rev | OEM ID | OEM rev | creator | creator rev |
|-------|--------------|-------|-----|--------|---------|---------|-------------|
| APIC | BXPC | 128 | 3 | BOCHS  | 1 | BXPC | 1 |
| DSDT | BXPC | 8647 | 1 | BOCHS  | 1 | BXPC | 1 |
| FACP | BXPC | 244 | 3 | BOCHS  | 1 | BXPC | 1 |
| FACS | — | 64 | 0 | — | — | — | — |
| HPET | BXPC | 56 | 1 | BOCHS  | 1 | BXPC | 1 |
| MCFG | BXPC | 60 | 1 | BOCHS  | 1 | BXPC | 1 |
| WAET | BXPC | 40 | 1 | BOCHS  | 1 | BXPC | 1 |

### SMBIOS type 0/1

- BIOS: vendor `SeaBIOS`, version `Arch Linux 1.17.0-2-2`, date `04/01/2014`
- System: manufacturer `QEMU`, product `Standard PC (Q35 + ICH9, 2009)`, version `pc-q35-11.1`, serial ``, SKU ``, family ``
- UUID: `00000000-0000-0000-0000-000000000000`

## q35-ovmf-win11

q35 + OVMF, Windows 11 profile devices, TPM (no secure boot)

### PCI functions

| BDF | vendor:device | class | device | subsystem | BARs |
|-----|--------------|-------|--------|-----------|------|
| 00:00.0 | `8086:29c0` | 0600 | Class: Device | 1af4:1100 Device | none |
| 00:01.0 | `1af4:1003` | 0780 | Class: Device | 1af4:0003 Device | I/O ports at 60c0 [size=64]; Memory at 80845000 (32-bit, non-prefetchable) [size=4K]; Memory at 380000000000 (64-bit, prefetchable) [size=16K] |
| 00:02.0 | `1af4:1001` | 0100 | Class: Device | 1af4:0002 Device | I/O ports at 6000 [size=128]; Memory at 80844000 (32-bit, non-prefetchable) [size=4K]; Memory at 380000004000 (64-bit, prefetchable) [size=16K] |
| 00:03.0 | `1af4:1000` | 0200 | Class: Device | 1af4:0001 Device | I/O ports at 6180 [size=32]; Memory at 80843000 (32-bit, non-prefetchable) [size=4K]; Memory at 380000008000 (64-bit, prefetchable) [size=16K]; Expansion ROM at 80800000 [disabled] [size=256K] |
| 00:04.0 | `1af4:1050` | 0300 | Class: Device | 1af4:1100 Device | Memory at 80000000 (32-bit, prefetchable) [size=8M]; Memory at 38000000c000 (64-bit, prefetchable) [size=16K]; Memory at 80842000 (32-bit, non-prefetchable) [size=4K]; Expansion ROM at 000c0000 [disabled] [size=128K] |
| 00:1d.0 | `8086:2934` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at 6160 [size=32] |
| 00:1d.1 | `8086:2935` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at 6140 [size=32] |
| 00:1d.2 | `8086:2936` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at 6120 [size=32] |
| 00:1d.7 | `8086:293a` | 0c03 | Class: Device | 1af4:1100 Device | Memory at 80841000 (32-bit, non-prefetchable) [size=4K] |
| 00:1f.0 | `8086:2918` | 0601 | Class: Device | 1af4:1100 Device | none |
| 00:1f.2 | `8086:2922` | 0106 | Class: Device | 1af4:1100 Device | I/O ports at 6100 [size=32]; Memory at 80840000 (32-bit, non-prefetchable) [size=4K] |
| 00:1f.3 | `8086:2930` | 0c05 | Class: Device | 1af4:1100 Device | I/O ports at 6080 [size=64] |

### ACPI tables

| table | OEM table ID | bytes | rev | OEM ID | OEM rev | creator | creator rev |
|-------|--------------|-------|-----|--------|---------|---------|-------------|
| APIC | BXPC | 144 | 3 | BOCHS  | 1 | BXPC | 1 |
| BGRT | EDK2 | 56 | 1 | INTEL  | 2 |      | 16777235 |
| DSDT | BXPC | 9407 | 1 | BOCHS  | 1 | BXPC | 1 |
| FACP | BXPC | 244 | 3 | BOCHS  | 1 | BXPC | 1 |
| FACS | — | 64 | 0 | — | — | — | — |
| HPET | BXPC | 56 | 1 | BOCHS  | 1 | BXPC | 1 |
| MCFG | BXPC | 60 | 1 | BOCHS  | 1 | BXPC | 1 |
| TPM2 | BXPC | 76 | 4 | BOCHS  | 1 | BXPC | 1 |
| WAET | BXPC | 40 | 1 | BOCHS  | 1 | BXPC | 1 |

### SMBIOS type 0/1

- BIOS: vendor `Debian distribution of EDK II`, version `2026.08+ds-2`, date `09/26/2026`
- System: manufacturer `QEMU`, product `Standard PC (Q35 + ICH9, 2009)`, version `pc-q35-11.1`, serial ``, SKU ``, family ``
- UUID: `00000000-0000-0000-0000-000000000000`

## q35-ovmf-win11-secureboot

q35 + OVMF secure boot, Windows 11 profile devices, TPM

### PCI functions

| BDF | vendor:device | class | device | subsystem | BARs |
|-----|--------------|-------|--------|-----------|------|
| 00:00.0 | `8086:29c0` | 0600 | Class: Device | 1af4:1100 Device | none |
| 00:01.0 | `1af4:1003` | 0780 | Class: Device | 1af4:0003 Device | I/O ports at 60c0 [size=64]; Memory at 80845000 (32-bit, non-prefetchable) [size=4K]; Memory at 380000000000 (64-bit, prefetchable) [size=16K] |
| 00:02.0 | `1af4:1001` | 0100 | Class: Device | 1af4:0002 Device | I/O ports at 6000 [size=128]; Memory at 80844000 (32-bit, non-prefetchable) [size=4K]; Memory at 380000004000 (64-bit, prefetchable) [size=16K] |
| 00:03.0 | `1af4:1000` | 0200 | Class: Device | 1af4:0001 Device | I/O ports at 6180 [size=32]; Memory at 80843000 (32-bit, non-prefetchable) [size=4K]; Memory at 380000008000 (64-bit, prefetchable) [size=16K]; Expansion ROM at 80800000 [disabled] [size=256K] |
| 00:04.0 | `1af4:1050` | 0300 | Class: Device | 1af4:1100 Device | Memory at 80000000 (32-bit, prefetchable) [size=8M]; Memory at 38000000c000 (64-bit, prefetchable) [size=16K]; Memory at 80842000 (32-bit, non-prefetchable) [size=4K]; Expansion ROM at 000c0000 [disabled] [size=128K] |
| 00:1d.0 | `8086:2934` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at 6160 [size=32] |
| 00:1d.1 | `8086:2935` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at 6140 [size=32] |
| 00:1d.2 | `8086:2936` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at 6120 [size=32] |
| 00:1d.7 | `8086:293a` | 0c03 | Class: Device | 1af4:1100 Device | Memory at 80841000 (32-bit, non-prefetchable) [size=4K] |
| 00:1f.0 | `8086:2918` | 0601 | Class: Device | 1af4:1100 Device | none |
| 00:1f.2 | `8086:2922` | 0106 | Class: Device | 1af4:1100 Device | I/O ports at 6100 [size=32]; Memory at 80840000 (32-bit, non-prefetchable) [size=4K] |
| 00:1f.3 | `8086:2930` | 0c05 | Class: Device | 1af4:1100 Device | I/O ports at 6080 [size=64] |

### ACPI tables

| table | OEM table ID | bytes | rev | OEM ID | OEM rev | creator | creator rev |
|-------|--------------|-------|-----|--------|---------|---------|-------------|
| APIC | BXPC | 144 | 3 | BOCHS  | 1 | BXPC | 1 |
| BGRT | EDK2 | 56 | 1 | INTEL  | 2 |      | 16777235 |
| DSDT | BXPC | 9544 | 1 | BOCHS  | 1 | BXPC | 1 |
| FACP | BXPC | 244 | 3 | BOCHS  | 1 | BXPC | 1 |
| FACS | — | 64 | 0 | — | — | — | — |
| HPET | BXPC | 56 | 1 | BOCHS  | 1 | BXPC | 1 |
| MCFG | BXPC | 60 | 1 | BOCHS  | 1 | BXPC | 1 |
| TPM2 | BXPC | 76 | 4 | BOCHS  | 1 | BXPC | 1 |
| WAET | BXPC | 40 | 1 | BOCHS  | 1 | BXPC | 1 |
| WSMT | EDK2 | 40 | 1 | INTEL  | 2 |      | 16777235 |

### SMBIOS type 0/1

- BIOS: vendor `Debian distribution of EDK II`, version `2026.08+ds-2`, date `09/26/2026`
- System: manufacturer `QEMU`, product `Standard PC (Q35 + ICH9, 2009)`, version `pc-q35-11.1`, serial ``, SKU ``, family ``
- UUID: `00000000-0000-0000-0000-000000000000`

## pc-seabios-legacy

pc (i440fx) + SeaBIOS, legacy profile device superset

### PCI functions

| BDF | vendor:device | class | device | subsystem | BARs |
|-----|--------------|-------|--------|-----------|------|
| 00:00.0 | `8086:1237` | 0600 | Class: Device | 1af4:1100 Device | none |
| 00:01.0 | `8086:7000` | 0601 | Class: Device | 1af4:1100 Device | none |
| 00:01.1 | `8086:7010` | 0101 | Class: Device | 1af4:1100 Device | I/O ports at 01f0 [size=8]; I/O ports at 03f4; I/O ports at 0170 [size=8]; I/O ports at 0374; I/O ports at c080 [size=16] |
| 00:01.2 | `8086:7020` | 0c03 | Class: Device | 1af4:1100 Device | I/O ports at c040 [size=32] |
| 00:01.3 | `8086:7113` | 0680 | Class: Device | 1af4:1100 Device | none |
| 00:02.0 | `8086:100e` | 0200 | Class: Device | 1af4:1100 Device | Memory at febc0000 (32-bit, non-prefetchable) [size=128K]; I/O ports at c000 [size=64]; Expansion ROM at feb40000 [disabled] [size=256K] |
| 00:03.0 | `1022:2000` | 0200 | Class: Device | — | I/O ports at c060 [size=32]; Memory at febf0000 (32-bit, non-prefetchable) [size=32]; Expansion ROM at feb80000 [disabled] [size=256K] |
| 00:04.0 | `1013:00b8` | 0300 | Class: Device | 1af4:1100 Device | Memory at fc000000 (32-bit, prefetchable) [size=32M]; Memory at febf1000 (32-bit, non-prefetchable) [size=4K]; Expansion ROM at 000c0000 [disabled] [size=128K] |

### ACPI tables

| table | OEM table ID | bytes | rev | OEM ID | OEM rev | creator | creator rev |
|-------|--------------|-------|-----|--------|---------|---------|-------------|
| APIC | BXPC | 120 | 3 | BOCHS  | 1 | BXPC | 1 |
| DSDT | BXPC | 8748 | 1 | BOCHS  | 1 | BXPC | 1 |
| FACP | BXPC | 116 | 1 | BOCHS  | 1 | BXPC | 1 |
| FACS | — | 64 | 0 | — | — | — | — |
| HPET | BXPC | 56 | 1 | BOCHS  | 1 | BXPC | 1 |
| WAET | BXPC | 40 | 1 | BOCHS  | 1 | BXPC | 1 |

### SMBIOS type 0/1

- BIOS: vendor `SeaBIOS`, version `Arch Linux 1.17.0-2-2`, date `04/01/2014`
- System: manufacturer `QEMU`, product `Standard PC (i440FX + PIIX, 1996)`, version `pc-i440fx-11.1`, serial ``, SKU ``, family ``
- UUID: `00000000-0000-0000-0000-000000000000`

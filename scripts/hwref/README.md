# hwref — QEMU hardware reference capture

Black-box captures of the guest-visible hardware that QEMU 11.1 presents for
vmlab's shipped device profiles. These dumps are the oracle side of the
differential suite (D26) and the reference for D30's "QEMU-identical
guest-visible hardware".

The capture boots a probe guest — vmlab's own container kernel with an
initramfs built from the Alpine 3.22 minirootfs plus `pciutils`, `dmidecode`
and `iasl`, inside an argv derived from vmlab's `src/qemu/cmdline.rs`.
Per-configuration READMEs record differences from shipped profiles. The
probe collects its dumps and streams them out over the `vmlab.agent.0`
channel (virtio-serial, or the 16550 on COM1 for the legacy config); the
COM2 serial log carries it where the agent channel is the console.

## Run

```
python3 scripts/hwref/capture.py            # all four configs
python3 scripts/hwref/capture.py --list
python3 scripts/hwref/capture.py pc-seabios-legacy
python3 scripts/hwref/capture.py --summarize   # regenerate docs/hwref/tables.md
python3 scripts/hwref/capture.py --output .scratch/hwref-run q35-seabios-linux-generic
python3 scripts/hwref/capture.py diff REFERENCE_DIR CANDIDATE_DIR
uv run --no-project python -m unittest discover -s scripts/hwref -p 'test_*.py'
```

First run downloads ~90 MB from dl-cdn.alpinelinux.org into `.cache/`
(gitignored) and builds the initramfs, the UKI for the OVMF boots, and the
blank guest media. Results land in `docs/hwref/<config>/`; each capture
records its exact QEMU argv (`qemu-argv.txt`), the swtpm argv, and the host
facts (QEMU version, CPU, KVM parameters, firmware hashes).

New SeaBIOS captures boot a Syslinux FAT32 probe disk. They use the same
kernel and initramfs as the OVMF UKI, without QEMU linuxboot or kvmvapic ROMs.
The Syslinux package is cached with the Alpine probe packages.
`--output` requires a fresh directory and leaves the checked-in captures alone.
The full P2 workflow, `vmlab-hv` machine-description requirements, normalization
rules, and D6 allow-list are documented in `docs/hwref/README.md`.
Set `HWREF_RECAPTURE=1` when running the Python suite to exercise two real
QEMU boots and the diff command.

Requirements: `/usr/bin/qemu-system-x86_64`, `qemu-img`, `/dev/kvm`,
`gcc`, `cpio`, `xorriso`, mtools, `sfdisk`, `objcopy`,
`/usr/lib/systemd/boot/efi/linuxx64.efi.stub` (for the UKI), `swtpm`,
network on first run.

The CPU configuration assumes AMD KVM with AVIC enabled. Asset and vmlab
source hashes are recorded in `environment.txt`. Selected captures replace
previous results. The script exits nonzero on a failed guest capture.

The fw_cfg collector inventories every named blob and copies blobs up to 64 KiB
with a 120-second timeout. Incomplete transfers fail capture validation.
Raw ACPI tables are captured independently.
`iasl` writes disassemblies beside raw tables in `guest/acpi/`; diagnostics are
in `guest/acpi-dsl/iasl.log`. The generated `docs/hwref/README.md` contains the
PCI, ACPI, and SMBIOS summaries.

Secure-boot firmware boots in setup mode because the probe is unsigned.
Enforcing secure boot is not captured. The legacy configuration combines
devices across profiles and therefore has different slots from each individual
profile. IRQ counts, timestamps, and host-derived CPUID data can vary.

## Clean room

QEMU is only ever run as a black box — never read. vmlab's sources are read
to mirror its argv; that is our own code.

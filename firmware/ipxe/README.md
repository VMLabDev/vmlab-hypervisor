# Build the virtio-net option ROM

Run `just firmware-ipxe-build` from the repository root. The build requires
Git, make, GCC with 32-bit freestanding support, binutils, Perl, Python, and uv.
The first build needs network access to GitHub. Later builds use
`target/ipxe/upstream` and reject a different commit or tracked source edits.

`upstream.json` pins upstream iPXE v2.0.0 to commit
`12798ec29aa8a64d8675c4378b99f5fe28447afb`. The local console configuration
enables COM1 output alongside the firmware console. No QEMU-built ROM is an
input.

The local `general.h` enables upstream `EFI_DOWNGRADE_UX`. OVMF registers
its native IPv4 and IPv6 boot options through SNP, without an additional
iPXE LoadFile boot option. A black-box QEMU comparison with the same OVMF
configuration verifies identical `Boot####` and `BootOrder` variables:

```sh
HWREF_RECAPTURE=1 python -m unittest discover -s scripts/hwref -p test_p2.py -k own_ipxe
```

## Outputs

All outputs are under `target/ipxe/`:

| File | Contents |
| --- | --- |
| `1af41000.rom` | Legacy BIOS PCI ROM for `1af4:1000` |
| `1af41000.efirom` | x86_64 EFI PCI ROM for `1af4:1000` |
| `virtio-net.rom` | Combined legacy and EFI ROM |
| `ipxe-source.tar` | Upstream source and vmlab build inputs |
| `SHA256SUMS` | SHA-256 hashes of the ROMs and source archive |

The combined image uses upstream `util/catrom.pl`, which updates the PCI
last-image flag and checksum. Its size fits the captured 256 KiB NIC ROM BAR.
The images are unsigned; secure-boot authentication is a separate requirement.

The build fixes `SOURCE_DATE_EPOCH` and `BUILD_SERIAL`. A clean rebuild with
GCC 16.2.1 and binutils 2.47 produced identical ROM hashes. Different toolchain
versions can produce different bytes. `NO_WERROR=1` permits upstream warnings
with newer compilers; compiler errors still fail the build.

## Use and verify

Set a q35 NIC's optional `rom` field to the absolute path of
`1af41000.rom` for SeaBIOS, or `virtio-net.rom` for a combined image. Omitting
`rom` disables the expansion ROM. The factory reads the file and calls
`VirtioPciDevice::new_transitional(...).with_option_rom(&image)` before PCI
registration. No shared device trait changes are required.

Run `cargo test -p vmlab-hv-devices transitional_tests -- --nocapture` to
check the transport and built ROMs. Run
`cargo test -p vmlab-hv --test ipxe -- --nocapture` for the P1 control-socket
probe. The latter requires KVM, SeaBIOS, SeaVGABIOS, and a built ROM; missing
assets produce a named skip. The probe resumes a paused q35 machine and checks
for iPXE on serial or in VGA text memory, read by the VMM's diagnostic output
when the probe quits through the control socket. The legacy ROM prefix uses
BIOS text services before initializing iPXE's serial console.

The hwref collector records NIC ROMs as `guest/pci-roms/<BDF>.rom`. The diff
compares PCI ROM vendor/device identity and image types, while lspci retains
ROM presence and BAR size. Executable bytes are not compared. Recapture both
sides to add ROM headers to older captures, which contain only lspci's BAR
observations. The D16 entry is in `compared`, not `platform_omissions`.

## Licence and corresponding source

iPXE is separate firmware, not MIT-licensed hypervisor code. Its source carries
GPLv2-or-later and other licence notices; the source archive preserves them.
When distributing a built ROM, provide its complete corresponding source and
build configuration under the applicable GPLv2 terms. Distribute
`ipxe-source.tar`, this directory, and `SHA256SUMS` alongside the ROMs rather
than relying on an upstream download link as the source offer.

Upstream build documentation: https://ipxe.org/download

Upstream source: https://github.com/ipxe/ipxe

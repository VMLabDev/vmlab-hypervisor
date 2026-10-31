# Test PCI passthrough

Use a spare PCI function that the workstation owner explicitly designates.
No device has been selected by this work package. Do not use a display GPU,
a storage controller carrying host filesystems, or the owner's keyboard or mouse
controller. The test never changes driver bindings.

## Prepare the designated function

Set `VMLAB_HV_TEST_VFIO_DEVICE` to its canonical, lowercase PCI address,
including the domain, for example `0000:65:00.0`. The example is a format
illustration, not a device recommendation.

Inspect the device and its IOMMU group before changing any binding:

```sh
BDF="$VMLAB_HV_TEST_VFIO_DEVICE"
[[ "$BDF" =~ ^[0-9a-f]{4}:[0-9a-f]{2}:[01][0-9a-f]\.[0-7]$ ]]
lspci -nnk -s "$BDF"
GROUP=$(readlink -f "/sys/bus/pci/devices/$BDF/iommu_group")
test -n "$GROUP"
ls -l "$GROUP/devices"
```

The host needs enabled IOMMU and interrupt remapping. Stop if the group contains
an essential host device. VFIO requires every group member to be detached from
its host driver or bound to VFIO. Obtain separate owner authorization before
changing any additional function, including a GPU's audio function. Do not use
ACS overrides or no-IOMMU mode.

After confirming that the selected function is idle and safe to detach, run
these commands on the host. Record the original driver first:

```sh
ORIGINAL_DRIVER=$(basename "$(readlink -f "/sys/bus/pci/devices/$BDF/driver")")
sudo modprobe vfio-pci
printf '%s' vfio-pci | sudo tee "/sys/bus/pci/devices/$BDF/driver_override"
printf '%s' "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind"
printf '%s' "$BDF" | sudo tee /sys/bus/pci/drivers_probe
readlink "/sys/bus/pci/devices/$BDF/driver"
ls -l "/dev/vfio/$(basename "$GROUP")"
```

The driver link must end in `vfio-pci`. Grant the test account access to the
group's VFIO node through the host's device-permission policy. The VMM refuses
other bindings and never unbinds, rebinds, or changes IOMMU groups.
Set the account's locked-memory limit to at least the guest RAM size. This
test needs 4 GiB. Check the limit in KiB with `ulimit -l`.

## Run the S1 acceptance test

Install SeaBIOS at `/usr/share/qemu/bios-256k.bin`. Provide the Ubuntu 26.04
template under `~/.local/share/vmlab/templates/x86_64/ubuntu-26.04/`, or set
`VMLAB_HV_TEST_VM_DISK` to its `disk.qcow2`. The test creates an overlay and
does not write the template.

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env RUST_TEST_THREADS=1 cargo test -p vmlab-hv-tests --test vfio -- --nocapture
```

The test boots the designated function at guest `00:06.0`, checks `lspci -n`
against its host vendor and device IDs, refuses snapshot save and load through
the control socket, and quits. Without the device variable, it prints a skip
reason before accessing hardware. Missing KVM, firmware, or template assets
also produce a named skip.

For hardware acceptance, also exercise the function with its normal guest
driver and repeat the test after guest shutdown and reset. Enumeration alone
does not prove GPU rendering, DMA throughput, or every interrupt mode.

After every VM has exited, restore the original host driver if appropriate:

```sh
printf '%s' "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind"
printf '%s' "$ORIGINAL_DRIVER" | sudo tee "/sys/bus/pci/devices/$BDF/driver_override"
printf '%s' "$BDF" | sudo tee /sys/bus/pci/drivers_probe
printf '\n' | sudo tee "/sys/bus/pci/devices/$BDF/driver_override"
lspci -nnk -s "$BDF"
```

## Machine description and limits

```json
"vfio": [
  {"host": "0000:65:00.0", "pci": {"bus": 0, "device": 6, "function": 0}}
]
```

Assignment requires q35 and an explicit, unoccupied bus-zero guest address.
The VMM resets the function at start and VM exit, maps guest RAM through a
VFIO type1 container, and directly maps mmap-capable BAR pages into both KVM
address spaces. Sparse mmap areas are respected. MSI-X table and PBA pages
remain trapped; other registers on those pages use VFIO region access.
Guest BAR and ROM addresses never reach host config space.

MSI advertises one maskable vector when hardware supports masking. MSI-X
preserves the hardware vector count. Both use eventfds and KVM MSI routing;
masked vectors retain pending events. INTx uses KVM irqfd resampling at the
q35 IOAPIC PIRQ GSI. Legacy PIC-only INTx delivery is not implemented.
Other capability accesses pass through the kernel VFIO config-space filters.
No vendor-specific GPU reset or NVIDIA workarounds are included.

Online snapshot save and load are refused while any VFIO function is assigned.
Disk-only image operations remain independent of assignment.

The implementation follows the Linux VFIO and KVM interfaces:
https://docs.kernel.org/driver-api/vfio.html
https://docs.kernel.org/virt/kvm/api.html

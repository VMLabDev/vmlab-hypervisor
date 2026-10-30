# vmlab minimal device set: covers the inventory in ../inventory.md
# Machines: q35 (default) and pc (i440fx). AHCI comes with Q35
# (select AHCI_ICH9), PIIX IDE and ISA bus come with I440FX (select PIIX).
CONFIG_Q35=y
CONFIG_I440FX=y

# Storage: floppy (-drive if=floppy), cfi.pflash01 firmware (-drive if=pflash,
# -global driver=cfi.pflash01,property=secure,value=on for OVMF secboot)
CONFIG_FDC_ISA=y
CONFIG_PFLASH_CFI01=y

# NICs: virtio-net-pci plus profile models e1000 (windows-legacy/xp/templeos)
# and pcnet (windows-9x)
CONFIG_E1000_PCI=y
CONFIG_PCNET_PCI=y

# Display: std VGA, cirrus-vga (windows-9x), qxl-vga (display=qxl; QXL
# requires SPICE), virtio-vga / virtio-vga-gl (virgl)
CONFIG_VGA_PCI=y
CONFIG_VGA_CIRRUS=y
CONFIG_QXL=y
CONFIG_VIRTIO_VGA=y

# USB: qemu-xhci, usb-tablet, usb-host (host-libusb module via libusb)
CONFIG_USB_XHCI_PCI=y
CONFIG_USB_HID=y

# TPM: tpm-tis on -tpmdev emulator (swtpm). TPM_EMULATOR is default-y in
# backends/tpm/Kconfig, so --without-default-devices drops it unless set.
CONFIG_TPM_TIS_ISA=y
CONFIG_TPM_EMULATOR=y

# Serial: isa-serial index 0/1 on legacy profiles (serial log + agent)
CONFIG_SERIAL_ISA=y

# GPU passthrough
CONFIG_VFIO_PCI=y

# Virtio: blk (disks/CD), net, serial (vmlab.agent.0), gpu (-gl for virgl)
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_SERIAL=y
CONFIG_VIRTIO_GPU=y
CONFIG_VHOST_USER_FS=y

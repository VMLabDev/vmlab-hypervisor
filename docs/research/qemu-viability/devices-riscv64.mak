# vmlab minimal device set: covers the inventory in ../inventory.md
# Machine: virt (VMs, containers, RISCV_VIRT_CODE/VARS)
CONFIG_RISCV_VIRT=y

# Virtio: blk (rootfs/scratch/CD), net, serial (vmlab.agent.0 + ctl.0),
# gpu-pci / gpu-gl (virgl), vhost-user-fs (virtiofs)
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_SERIAL=y
CONFIG_VIRTIO_GPU=y
CONFIG_VHOST_USER_FS=y

# Display: std maps to VGA on every arch
CONFIG_VGA_PCI=y

# USB: qemu-xhci, usb-tablet, usb-host
CONFIG_USB_XHCI_PCI=y
CONFIG_USB_HID=y

# TPM: tpm-tis-device on -tpmdev emulator (swtpm). TPM_EMULATOR is default-y
# in backends/tpm/Kconfig, so --without-default-devices drops it unless set.
CONFIG_TPM_TIS_SYSBUS=y
CONFIG_TPM_EMULATOR=y

# GPU passthrough
CONFIG_VFIO_PCI=y

# Firmware: pflash for RISCV_VIRT_CODE/VARS
CONFIG_PFLASH_CFI01=y

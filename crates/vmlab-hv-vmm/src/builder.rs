//! Building a [`Vm`] from a [`MachineDescription`] and a [`DeviceFactory`].

use parking_lot::Mutex;
use std::sync::Arc;

use vmlab_hv_devices::{IrqLine, MsiSink};
use vmlab_hv_proto::machine::{Accel, Arch, Boot, MachineDescription};

use crate::acpi::{self, AcpiConfig};
use crate::bus::{IoBus, MmioBus};
use crate::e820;
use crate::error::VmmError;
use crate::factory::DeviceFactory;
use crate::kvm::{KvmIrqLine, KvmMsiSink, KvmVm};
use crate::layout::{MMCONFIG_BASE, MMCONFIG_SIZE, MemLayout};
use crate::loader;
use crate::memory::{GuestRam, register_memslots};
use crate::pci::{PciConfigIo, PciEcam, PciRoot};
use crate::pm::{PM_BLOCK_LEN, PM1A_EVT_BASE, PmDevice, SCI_GSI, SleepSignal};
use crate::snapshot::{DeviceNode, DiskNode, SnapshotRegistry};
use crate::vm::{Vm, VmShared};

/// Static checks that do not need KVM; each error names what is wrong.
pub fn validate(desc: &MachineDescription) -> Result<(), VmmError> {
    if desc.arch != Arch::X86_64 {
        return Err(VmmError::InvalidMachine(format!(
            "arch {:?} is not supported: phase 1 builds x86_64 machines only",
            desc.arch
        )));
    }
    if desc.accel != Accel::Kvm {
        return Err(VmmError::InvalidMachine(format!(
            "accel {:?} is not supported: phase 1 runs KVM only",
            desc.accel
        )));
    }
    if desc.vcpus == 0 {
        return Err(VmmError::InvalidMachine("vcpus must be at least 1".into()));
    }
    MemLayout::new(desc.memory_mib).map_err(VmmError::InvalidMachine)?;
    let Boot::Kernel { kernel, .. } = &desc.boot;
    if !kernel.exists() {
        return Err(VmmError::InvalidMachine(format!(
            "kernel image {} does not exist",
            kernel.display()
        )));
    }
    Ok(())
}

/// Build the whole machine. With `paused`, the vCPUs stay parked until the
/// `resume` command (`--paused`, the `-S` equivalent).
pub fn build(
    desc: &MachineDescription,
    factory: &dyn DeviceFactory,
    paused: bool,
) -> Result<Vm, VmmError> {
    validate(desc)?;
    let kvm = KvmVm::new()?;
    if desc.vcpus as usize > kvm.max_vcpus() {
        return Err(VmmError::InvalidMachine(format!(
            "vcpus {} exceeds the host KVM limit {}",
            desc.vcpus,
            kvm.max_vcpus()
        )));
    }

    let layout = MemLayout::new(desc.memory_mib).map_err(VmmError::InvalidMachine)?;
    // A machine with shares hands its RAM to virtiofsd: back it with a
    // shared memfd. Everything else maps anonymous private memory.
    let ram = if desc.shares.is_empty() {
        GuestRam::anonymous(layout.clone())?
    } else {
        GuestRam::shared_memfd(layout.clone())?
    };
    register_memslots(&kvm.vm, &ram)?;

    // Platform: e820, ACPI, kernel.
    let e820_map = e820::build_e820(&layout);
    let acpi = acpi::install(
        &ram.mem,
        &AcpiConfig {
            vcpus: desc.vcpus,
            layout: &layout,
        },
    )?;
    let Boot::Kernel {
        kernel,
        initrd,
        cmdline,
    } = &desc.boot;
    let boot = loader::load(
        &ram.mem,
        kernel,
        initrd.as_deref(),
        cmdline,
        &e820_map,
        Some(acpi.rsdp),
        layout.low_ram_top(),
    )?;

    // Buses and the PCI root.
    let mut io_bus = IoBus::new();
    let mut mmio_bus = MmioBus::new();
    let pci_root = Arc::new(Mutex::new(PciRoot::new(layout.mmio64_base)));
    io_bus
        .register(
            0xCF8,
            12,
            Arc::new(Mutex::new(PciConfigIo::new(pci_root.clone()))),
        )
        .map_err(VmmError::Other)?;
    mmio_bus
        .register(
            MMCONFIG_BASE,
            MMCONFIG_SIZE,
            Arc::new(Mutex::new(PciEcam::new(pci_root.clone()))),
        )
        .map_err(VmmError::Other)?;

    // Devices, in the machine description's order. Each built device also
    // hands back its snapshot handle; the registry keeps them in machine
    // order for snapshot orchestration.
    let vm_fd = kvm.vm.clone();
    let msi: Arc<dyn MsiSink> = Arc::new(KvmMsiSink::new(vm_fd.clone()));
    let mut snap_disks = Vec::new();
    let mut snap_nodes = Vec::new();

    // COM1: 16550A on IRQ 4, output to the serial log.
    let irq4: Arc<dyn IrqLine> = Arc::new(KvmIrqLine::new(vm_fd.clone(), 4));
    let serial =
        factory
            .serial(&desc.serial_log, irq4)
            .map_err(|e| VmmError::DeviceUnavailable {
                device: e.device,
                reason: format!("serial (COM1): {}", e.reason),
            })?;
    io_bus
        .register(0x3F8, 8, serial.device)
        .map_err(VmmError::Other)?;
    if let Some(node) = serial.snapshot {
        snap_nodes.push(DeviceNode::Plain(node));
    }

    let add_pci = |root: &Arc<Mutex<PciRoot>>,
                   built: Result<crate::factory::BuiltPci, crate::error::DeviceUnavailable>,
                   what: String|
     -> Result<crate::factory::BuiltPci, VmmError> {
        let built = built.map_err(|e| VmmError::DeviceUnavailable {
            device: e.device,
            reason: format!("{what}: {}", e.reason),
        })?;
        root.lock()
            .add_device(built.function.clone())
            .map_err(VmmError::Other)?;
        Ok(built)
    };

    // Rootfs and scratch disks in order: /dev/vda, /dev/vdb, ...
    for disk in &desc.disks {
        let blk = factory
            .virtio_blk(disk, &ram.mem, msi.clone())
            .map_err(|e| VmmError::DeviceUnavailable {
                device: e.device,
                reason: format!(
                    "disk `{}` (/dev/vd{}): {}",
                    disk.id,
                    disk_index_letter(disk, &desc.disks),
                    e.reason
                ),
            })?;
        pci_root
            .lock()
            .add_device(blk.pci.function.clone())
            .map_err(VmmError::Other)?;
        if let Some(node) = blk.pci.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
        snap_disks.push(DiskNode {
            id: disk.id.clone(),
            writable: !disk.read_only,
            vmstate: disk.vmstate,
            image: blk.disk,
        });
    }
    // One multiport console with the named ports.
    if !desc.ports.is_empty() {
        let built = add_pci(
            &pci_root,
            factory.virtio_console(&desc.ports, &ram.mem, msi.clone()),
            "virtio-console ports".to_string(),
        )?;
        if let Some(node) = built.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
    }
    // One NIC per interface.
    for nic in &desc.nics {
        let built = add_pci(
            &pci_root,
            factory.virtio_net(nic, &ram.mem, msi.clone()),
            format!("nic {}", nic.mac),
        )?;
        if let Some(node) = built.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
    }
    // One vhost-user-fs function per share.
    for share in &desc.shares {
        let built = add_pci(
            &pci_root,
            factory.vhost_user_fs(share, &ram.mem, msi.clone()),
            format!("share `{}`", share.tag),
        )?;
        if let Some(node) = built.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
    }

    // ACPI PM1a block on IRQ 9.
    let sleep_signal = Arc::new(SleepSignal::default());
    let sci: Arc<dyn IrqLine> = Arc::new(KvmIrqLine::new(vm_fd, SCI_GSI));
    let pm = Arc::new(Mutex::new(PmDevice::new(sci, sleep_signal.sink())));
    io_bus
        .register(PM1A_EVT_BASE, PM_BLOCK_LEN, pm.clone())
        .map_err(VmmError::Other)?;

    let shared = VmShared::new(
        kvm,
        io_bus,
        mmio_bus,
        pci_root,
        pm,
        sleep_signal,
        layout.mmio64_base,
    );
    shared.set_snapshot_registry(Arc::new(SnapshotRegistry {
        digest: machine_digest(desc),
        disks: snap_disks,
        nodes: snap_nodes,
    }));

    // vCPUs: the BSP enters the kernel's 64-bit entry; the APs stay in
    // reset state until the kernel's INIT-SIPI-SIPI wakes them.
    let mut vcpu_fds = Vec::with_capacity(desc.vcpus as usize);
    for id in 0..desc.vcpus {
        let mut vcpu = shared.kvm.create_vcpu(id as u8)?;
        if id == 0 {
            loader::init_vcpu(&mut vcpu, &boot)?;
        }
        vcpu_fds.push((id as u8, vcpu));
    }

    let mut vm = Vm::from_parts(shared, vcpu_fds, ram)?;
    if !paused {
        vm.start();
    }
    Ok(vm)
}

/// `a`, `b`, ... for a disk's guest device name, for error messages only.
fn disk_index_letter(
    disk: &vmlab_hv_proto::machine::Disk,
    disks: &[vmlab_hv_proto::machine::Disk],
) -> char {
    let idx = disks.iter().position(|d| d.id == disk.id).unwrap_or(0);
    (b'a' + idx.min(25) as u8) as char
}

/// SHA-256 of the serialised machine description: the snapshot identity a
/// restore refuses to cross (D32). Field order is fixed by the struct, so
/// serialisation is deterministic for a given build.
fn machine_digest(desc: &MachineDescription) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(desc).unwrap_or_default();
    Sha256::digest(&bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmlab_hv_proto::machine::{Boot, Disk, DiskFormat, MachineDescription};

    fn desc() -> MachineDescription {
        MachineDescription {
            name: "test".into(),
            arch: Arch::X86_64,
            accel: Accel::Kvm,
            vcpus: 1,
            memory_mib: 256,
            boot: Boot::Kernel {
                kernel: "/kernel".into(),
                initrd: None,
                cmdline: "console=ttyS0".into(),
            },
            disks: vec![],
            nics: vec![],
            serial_log: "/tmp/serial.log".into(),
            ports: vec![],
            shares: vec![],
            rtc_base: None,
        }
    }

    #[test]
    fn rejects_unsupported_arch_and_accel() {
        let mut d = desc();
        d.arch = Arch::Aarch64;
        let err = validate(&d).unwrap_err().to_string();
        assert!(err.contains("x86_64"), "{err}");

        let mut d = desc();
        d.accel = Accel::Tcg;
        let err = validate(&d).unwrap_err().to_string();
        assert!(err.contains("KVM"), "{err}");
    }

    #[test]
    fn rejects_zero_vcpus_and_bad_memory() {
        let mut d = desc();
        d.vcpus = 0;
        assert!(validate(&d).unwrap_err().to_string().contains("vcpus"));

        let mut d = desc();
        d.memory_mib = 1;
        assert!(validate(&d).unwrap_err().to_string().contains("memory"));
    }

    #[test]
    fn rejects_missing_kernel_naming_the_path() {
        let err = validate(&desc()).unwrap_err().to_string();
        assert!(err.contains("/kernel"), "{err}");
    }

    /// The smallest bzImage `linux-loader` accepts.
    fn tiny_bzimage(dir: &std::path::Path) -> std::path::PathBuf {
        let mut image = vec![0u8; 0x4000];
        image[0x1FE] = 0x55;
        image[0x1FF] = 0xAA;
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x0208u16.to_le_bytes());
        image[0x211] = 0x01; // LOADED_HIGH
        image[0x1F1] = 4; // setup_sects
        image[0x214..0x218].copy_from_slice(&0x20_0000u32.to_le_bytes());
        let path = dir.join("vmlinuz");
        std::fs::write(&path, image).unwrap();
        path
    }

    #[test]
    fn build_fails_clearly_when_devices_are_missing() {
        // /dev/kvm is needed; without it the device error cannot be reached.
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm is not available");
            return;
        }
        let dir = std::env::temp_dir().join(format!("vmlab-hv-builder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = desc();
        d.boot = Boot::Kernel {
            kernel: dir.join("broken-vmlinuz"),
            initrd: None,
            cmdline: String::new(),
        };
        std::fs::write(dir.join("broken-vmlinuz"), b"not a bzImage").unwrap();
        d.disks = vec![Disk {
            id: "rootfs".into(),
            path: "/disk.raw".into(),
            format: DiskFormat::Raw,
            read_only: true,
            vmstate: false,
        }];
        // The kernel is not a bzImage, so the failure names the kernel
        // before the missing device models are reached.
        let Err(err) = build(&d, &crate::factory::NoDevices, true) else {
            panic!("a non-bzImage kernel must fail the build");
        };
        let err = err.to_string();
        assert!(err.contains("bzImage"), "{err}");

        // With a loadable kernel and no disks the serial device is the first
        // thing asked of the factory: the failure names COM1, not a panic.
        d.disks.clear();
        d.boot = Boot::Kernel {
            kernel: tiny_bzimage(&dir),
            initrd: None,
            cmdline: String::new(),
        };
        let Err(err) = build(&d, &crate::factory::NoDevices, true) else {
            panic!("building without device models must fail");
        };
        let err = err.to_string();
        assert!(err.contains("serial") && err.contains("COM1"), "{err}");
    }
}

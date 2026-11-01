//! Building a [`Vm`] from a [`MachineDescription`] and a [`DeviceFactory`].

use parking_lot::Mutex;
use std::sync::Arc;

use vmlab_hv_devices::{IrqLine, MsiSink};
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, DiskBus, FirmwareKind, Machine, MachineDescription, PciAddress,
};

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
    let mut hosts = std::collections::BTreeSet::new();
    for device in &desc.vfio {
        crate::vfio::validate_address(&device.host).map_err(VmmError::InvalidMachine)?;
        if !hosts.insert(&device.host) {
            return Err(VmmError::InvalidMachine(
                "duplicate VFIO host address".into(),
            ));
        }
    }
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
    if desc.vcpus == 0 || desc.vcpus > 255 {
        return Err(VmmError::InvalidMachine(
            "vcpus must be in 1..=255 (xAPIC topology)".into(),
        ));
    }
    if let Some(display) = &desc.display {
        if display.device != vmlab_hv_proto::machine::DisplayDevice::VirtioVga
            && (display.gpu.is_some() || display.render_node.is_some())
        {
            return Err(VmmError::InvalidMachine(
                "GPU acceleration requires virtio-vga".into(),
            ));
        }
        if display.gpu.is_some() != display.render_node.is_some() {
            return Err(VmmError::InvalidMachine(
                "gpu and render_node must be specified together".into(),
            ));
        }
    }
    MemLayout::new(desc.memory_mib).map_err(VmmError::InvalidMachine)?;
    crate::clock::rtc_base(desc.rtc_base.as_deref())?;
    validate_legacy_features(desc)?;
    for disk in &desc.disks {
        if (disk.path.is_none()
            && !(matches!(disk.bus, DiskBus::Ahci | DiskBus::Ide) && disk.cdrom))
            || disk
                .path
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            || (disk.cdrom && disk.vmstate)
        {
            return Err(VmmError::InvalidMachine(format!(
                "disk `{}` requires an image path; only a non-vmstate CD-ROM may be empty",
                disk.id
            )));
        }
    }
    if desc.machine == Machine::Microvm
        && (!desc.vfio.is_empty()
            || desc.display.is_some()
            || desc.tablet.is_some()
            || desc.tpm.is_some()
            || desc.ahci_pci.is_some()
            || desc.console_pci.is_some()
            || desc.disks.iter().any(|disk| {
                disk.bus != DiskBus::Virtio
                    || disk.cdrom
                    || disk.port.is_some()
                    || disk.pci.is_some()
            })
            || desc.nics.iter().any(|nic| nic.pci.is_some())
            || desc.shares.iter().any(|share| share.pci.is_some())
            || desc.audio.is_some())
    {
        return Err(VmmError::InvalidMachine(
            "q35 devices and explicit PCI addresses require machine=q35".into(),
        ));
    }
    let image = match (&desc.machine, &desc.boot) {
        (Machine::Microvm, Boot::Kernel { kernel, .. }) => kernel,
        (
            Machine::Q35 | Machine::Pc,
            Boot::Firmware {
                firmware,
                code,
                vars,
                secure_boot,
                ..
            },
        ) => {
            if *firmware == FirmwareKind::Seabios && (vars.is_some() || *secure_boot) {
                return Err(VmmError::InvalidMachine(
                    "SeaBIOS does not support vars or secure_boot".into(),
                ));
            }
            if *firmware == FirmwareKind::Ovmf && vars.is_none() {
                return Err(VmmError::InvalidMachine(
                    "OVMF requires a writable vars path".into(),
                ));
            }
            if let Some(vars) = vars
                && !vars.is_file()
            {
                return Err(VmmError::InvalidMachine(format!(
                    "firmware vars {} does not exist",
                    vars.display()
                )));
            }
            validate_q35_devices(desc)?;
            code
        }
        _ => {
            return Err(VmmError::InvalidMachine(
                "microvm requires kernel boot; q35 and pc require firmware boot".into(),
            ));
        }
    };
    if !image.is_file() {
        return Err(VmmError::InvalidMachine(format!(
            "boot image {} does not exist",
            image.display()
        )));
    }
    Ok(())
}

fn validate_q35_devices(desc: &MachineDescription) -> Result<(), VmmError> {
    let mut used = std::collections::BTreeSet::from([(0, 0)]);
    if desc.machine == Machine::Pc {
        used.extend([(1, 0), (1, 1), (1, 2), (1, 3)]);
    }
    let mut add = |address: Option<PciAddress>, name: &str| -> Result<(), VmmError> {
        let a = address.ok_or_else(|| {
            VmmError::InvalidMachine(format!("q35 {name} requires an explicit PCI address"))
        })?;
        if a.bus != 0 || a.device >= 32 || a.function >= 8 || !used.insert((a.device, a.function)) {
            return Err(VmmError::InvalidMachine(format!(
                "invalid or duplicate PCI address for {name}: {a:?}"
            )));
        }
        Ok(())
    };
    let mut ports = std::collections::BTreeSet::new();
    for device in &desc.vfio {
        add(Some(device.pci), "VFIO")?;
    }
    for disk in &desc.disks {
        match disk.bus {
            DiskBus::Virtio => {
                if disk.cdrom || disk.port.is_some() {
                    return Err(VmmError::InvalidMachine(format!(
                        "virtio disk {} cannot have cdrom or an AHCI port",
                        disk.id
                    )));
                }
                add(disk.pci, &disk.id)?;
            }
            DiskBus::Ahci => {
                if disk.pci.is_some() || disk.port.is_none_or(|p| p >= 6 || !ports.insert(p)) {
                    return Err(VmmError::InvalidMachine(format!(
                        "AHCI disk {} requires a unique port in 0..6 and uses ahci_pci",
                        disk.id
                    )));
                }
                if disk.cdrom && !disk.read_only {
                    return Err(VmmError::InvalidMachine("CD-ROM must be read_only".into()));
                }
            }
            DiskBus::Ide => {}
        }
    }
    if !ports.is_empty() && desc.ahci_pci.is_none() {
        return Err(VmmError::InvalidMachine(
            "AHCI disks require ahci_pci".into(),
        ));
    }
    if let Some(a) = desc.ahci_pci {
        if (a.bus, a.device, a.function) != (0, 31, 2) {
            return Err(VmmError::InvalidMachine(
                "q35 AHCI must be at 00:1f.2".into(),
            ));
        }
        add(Some(a), "AHCI")?;
    }
    if desc.ports.iter().any(|port| port.transport.is_virtio()) {
        add(desc.console_pci, "virtio-serial")?;
    }
    for nic in &desc.nics {
        add(nic.pci, "NIC")?;
    }
    for share in &desc.shares {
        if share.backing().is_none() {
            return Err(VmmError::InvalidMachine(format!(
                "share `{}` must set exactly one of `socket` (external virtiofsd) \
                 and `path` (in-process)",
                share.tag
            )));
        }
        add(share.pci, &share.tag)?;
    }
    if let Some(display) = &desc.display {
        add(Some(display.pci), "display")?;
    }
    if let Some(tablet) = desc.tablet {
        add(Some(tablet), "tablet")?;
    }
    // The tablet and the passthrough controller are one xHCI function.
    match (desc.tablet, desc.usb) {
        (Some(tablet), Some(usb)) if tablet != usb => {
            return Err(VmmError::InvalidMachine(
                "the tablet and usb functions must share one xHCI controller".into(),
            ));
        }
        (None, Some(usb)) => add(Some(usb), "usb")?,
        _ => {}
    }
    if let Some(audio) = &desc.audio {
        match audio.device {
            vmlab_hv_proto::machine::AudioDevice::Hda => {
                // D11: q35's HDA sits at 00:1b.0, where real ICH9 places it.
                if audio
                    .pci
                    .is_none_or(|a| (a.bus, a.device, a.function) != (0, 0x1b, 0))
                {
                    return Err(VmmError::InvalidMachine("q35 HDA sits at 00:1b.0".into()));
                }
                add(audio.pci, "audio")?;
            }
            // virtio-snd is a plain virtio function: any free slot the
            // description names, like a NIC or a console.
            vmlab_hv_proto::machine::AudioDevice::VirtioSnd => {
                add(audio.pci, "audio")?;
            }
        }
    }
    for reserved in [(31, 0), (31, 3)] {
        if used.contains(&reserved) {
            return Err(VmmError::InvalidMachine(format!(
                "PCI address 00:{:02x}.{} is reserved for the chipset",
                reserved.0, reserved.1
            )));
        }
    }
    Ok(())
}

fn validate_legacy_features(desc: &MachineDescription) -> Result<(), VmmError> {
    use vmlab_hv_proto::machine::{DisplayDevice, NicModel};
    let invalid = |reason: &str| Err(VmmError::InvalidMachine(reason.into()));
    let pc = desc.machine == Machine::Pc;
    if let Some(model) = desc.cpu_model.as_deref()
        && !(model == "host"
            || (pc && model == "pentium3")
            || (desc.machine == Machine::Q35 && model == "Skylake-Client-v4"))
    {
        return invalid("cpu_model must be host, pentium3 (pc), or Skylake-Client-v4 (q35)");
    }
    if desc.tsc_khz == Some(0) {
        return invalid("tsc_khz must be positive");
    }
    if desc.machine != Machine::Q35
        && (desc.applesmc.is_some()
            || desc.usb_keyboard
            || desc.smbios_type2
            || desc.vmware_cpuid_freq
            || desc.invtsc
            || desc.tsc_khz.is_some()
            || !desc.msr_policy.is_fault())
    {
        return invalid("macOS platform features require machine=q35");
    }
    if desc.usb_keyboard && desc.tablet.or(desc.usb).is_none() {
        return invalid("usb_keyboard requires an xHCI PCI address in tablet or usb");
    }
    if let Some(smc) = &desc.applesmc {
        vmlab_hv_devices::applesmc::AppleSmc::new(&smc.osk).map_err(|_| {
            VmmError::InvalidMachine(
                "applesmc requires exactly 64 printable ASCII characters".into(),
            )
        })?;
    }
    if desc.vmware_cpuid_freq && !desc.hyperv.is_empty() {
        return invalid("vmware_cpuid_freq cannot share the Hyper-V CPUID range");
    }
    if desc.floppy.is_some() {
        return invalid(if pc {
            "pc floppy requires phase 4c"
        } else {
            "floppy requires machine=pc"
        });
    }
    if pc {
        if !matches!(
            desc.boot,
            Boot::Firmware {
                firmware: FirmwareKind::Seabios,
                ..
            }
        ) {
            return invalid("pc requires SeaBIOS firmware boot");
        }
        if desc.tablet.is_some() || desc.usb.is_some() {
            return invalid("pc UHCI tablet and USB require phase 4c");
        }
        if desc.ahci_pci.is_some() || desc.disks.iter().any(|disk| disk.bus == DiskBus::Ahci) {
            return invalid("pc uses PIIX IDE, not q35 AHCI");
        }
        if desc
            .display
            .as_ref()
            .is_some_and(|display| display.device != DisplayDevice::StdVga)
        {
            return invalid("pc requires std-vga; Cirrus requires phase 4d");
        }
        if desc.audio.is_some() || desc.tpm.is_some() || !desc.vfio.is_empty() {
            return invalid("audio, TPM and VFIO require machine=q35");
        }
    } else if desc.nics.iter().any(|nic| nic.model != NicModel::Virtio) {
        return invalid("e1000 and pcnet require machine=pc");
    }
    if desc
        .display
        .as_ref()
        .is_some_and(|display| display.device == DisplayDevice::CirrusVga)
    {
        return invalid("Cirrus requires machine=pc and phase 4d");
    }
    let mut slots = std::collections::BTreeSet::new();
    for disk in &desc.disks {
        if disk.bus == DiskBus::Ide {
            if !pc {
                return invalid("IDE requires machine=pc; IDE on q35 is unsupported");
            }
            if disk.channel.is_none_or(|channel| channel > 1)
                || disk.unit.is_none_or(|unit| unit > 1)
                || disk.pci.is_some()
                || disk.port.is_some()
            {
                return invalid("IDE requires channel and unit in 0..=1, without pci or port");
            }
            if !slots.insert((disk.channel.unwrap(), disk.unit.unwrap())) {
                return invalid("duplicate IDE slot; at most four IDE devices are supported");
            }
            if disk.cdrom && !disk.read_only {
                return invalid("CD-ROM must be read_only");
            }
        } else if disk.channel.is_some() || disk.unit.is_some() {
            return invalid("channel and unit require an IDE disk");
        }
    }
    let mut coms = std::collections::BTreeSet::from([desc.serial_log_port.io_base()]);
    let mut sockets = std::collections::BTreeSet::new();
    for port in &desc.ports {
        if !sockets.insert(&port.socket) {
            return invalid("duplicate serial socket path");
        }
        if let Some(com) = port.transport.com_port() {
            if !pc {
                return invalid("ISA serial agent ports require machine=pc");
            }
            if !coms.insert(com.io_base()) {
                return invalid("serial port is occupied by the log or another agent");
            }
        }
    }
    if pc && !coms.contains(&0x3f8) {
        return invalid("a COM2 serial log requires a COM1 agent socket on pc");
    }
    if !pc && !desc.serial_log_port.is_com1() {
        return invalid("selectable serial log ports require machine=pc");
    }
    Ok(())
}

fn hid_serial(address: PciAddress, prefix: u32, connector: u8) -> String {
    format!(
        "{prefix}-0000:{:02x}:{:02x}.{}-{connector}",
        address.bus, address.device, address.function
    )
}

/// Build the whole machine. With `paused`, the vCPUs stay parked until the
/// `resume` command (`--paused`, the `-S` equivalent).
pub fn build(
    desc: &MachineDescription,
    factory: &dyn DeviceFactory,
    paused: bool,
) -> Result<Vm, VmmError> {
    validate(desc)?;
    crate::vfio::preflight_machine(desc).map_err(VmmError::InvalidMachine)?;
    let kvm = KvmVm::new()?;
    crate::hyperv::configure_vm(&kvm.kvm, &kvm.vm, &desc.hyperv)?;
    if desc.vcpus as usize > kvm.max_vcpus() {
        return Err(VmmError::InvalidMachine(format!(
            "vcpus {} exceeds the host KVM limit {}",
            desc.vcpus,
            kvm.max_vcpus()
        )));
    }

    let q35 = desc.machine == Machine::Q35;
    let pc = desc.machine == Machine::Pc;
    let pc_com2 = pc
        && (!desc.serial_log_port.is_com1()
            || desc
                .ports
                .iter()
                .any(|port| port.transport == vmlab_hv_proto::machine::SerialTransport::Com2));
    let firmware_machine = q35 || pc;
    let ovmf = matches!(
        desc.boot,
        Boot::Firmware {
            firmware: FirmwareKind::Ovmf,
            ..
        }
    );
    let mut layout = if pc {
        MemLayout::pc(desc.memory_mib)
    } else if q35 {
        MemLayout::q35(desc.memory_mib)
    } else {
        MemLayout::new(desc.memory_mib)
    }
    .map_err(VmmError::InvalidMachine)?;
    let physical_low_end = layout.low_ram_top();
    let secure_smm = matches!(
        desc.boot,
        Boot::Firmware {
            firmware: FirmwareKind::Ovmf,
            secure_boot: true,
            ..
        }
    );
    if secure_smm {
        if physical_low_end < 128 << 20 {
            return Err(VmmError::InvalidMachine(
                "secure OVMF requires at least 128 MiB RAM".into(),
            ));
        }
        layout.regions[1].1 -= 64 << 20;
        layout.regions[0] = (0, 0x30000);
        layout.regions.insert(1, (0x50000, 0x50000));
    }
    // A machine with shares hands its RAM to virtiofsd: back it with a
    // shared memfd. Everything else maps anonymous private memory.
    let mut ram = if pc {
        GuestRam::pc(layout.clone(), kvm.vm.clone())?
    } else if desc.shares.is_empty() {
        GuestRam::anonymous(layout.clone())?
    } else {
        GuestRam::shared_memfd(layout.clone())?
    };
    register_memslots(&kvm.vm, &ram)?;
    if firmware_machine {
        if !kvm.kvm.check_extension(kvm_ioctls::Cap::X86Smm) {
            return Err(VmmError::KvmCap("x86 SMM for SeaBIOS initialization"));
        }
        crate::memory::register_smm_memslots(&kvm.vm, &ram)?;
    }

    // Platform: e820, ACPI, kernel.
    let boot = if let Boot::Kernel {
        kernel,
        initrd,
        cmdline,
    } = &desc.boot
    {
        let e820_map = e820::build_e820(&layout);
        let acpi = acpi::install(
            &ram.mem,
            &AcpiConfig {
                vcpus: desc.vcpus,
                layout: &layout,
            },
        )?;
        Some(loader::load(
            &ram.mem,
            kernel,
            initrd.as_deref(),
            cmdline,
            &e820_map,
            Some(acpi.rsdp),
            layout.low_ram_top(),
        )?)
    } else {
        None
    };

    // Buses and the PCI root.
    let mut io_bus = IoBus::new();
    let mut mmio_bus = MmioBus::new();
    let pci_root = Arc::new(Mutex::new(if firmware_machine {
        PciRoot::q35(layout.mmio64_base)
    } else {
        PciRoot::new(layout.mmio64_base)
    }));
    let config_io = Arc::new(Mutex::new(PciConfigIo::new(pci_root.clone())));
    io_bus
        .register(0xCF8, 8, config_io.clone())
        .map_err(VmmError::Other)?;
    if !firmware_machine {
        mmio_bus
            .register(
                MMCONFIG_BASE,
                MMCONFIG_SIZE,
                Arc::new(Mutex::new(PciEcam::new(pci_root.clone()))),
            )
            .map_err(VmmError::Other)?;
    }

    // Devices, in the machine description's order. Each built device also
    // hands back its snapshot handle; the registry keeps them in machine
    // order for snapshot orchestration.
    let vm_fd = kvm.vm.clone();
    let msi: Arc<dyn MsiSink> = Arc::new(KvmMsiSink::new(vm_fd.clone()));
    let chipset = if pc {
        Some(crate::chipset::Chipset::pc(
            vm_fd.clone(),
            &layout,
            crate::clock::rtc_base(desc.rtc_base.as_deref())?,
            ram.a20.as_ref().unwrap().clone(),
        )?)
    } else if q35 {
        Some(crate::chipset::Chipset::new(
            vm_fd.clone(),
            &layout,
            crate::clock::rtc_base(desc.rtc_base.as_deref())?,
            secure_smm,
        )?)
    } else {
        None
    };
    if let Some(chipset) = &chipset {
        if pc {
            chipset.lpc.lock().configure_piix4_legacy_io(pc_com2, false);
        }
        chipset
            .rtc
            .lock()
            .set_memory_layout(physical_low_end, layout.ram - physical_low_end);
    }
    let pirqs = chipset
        .as_ref()
        .map(|c| c.pirqs.clone())
        .unwrap_or_else(|| {
            crate::q35::PirqRouter::new(std::array::from_fn(|gsi| {
                Arc::new(KvmIrqLine::new(vm_fd.clone(), gsi as u32)) as Arc<dyn IrqLine>
            }))
        });
    let isa_irq = |id, gsi| -> Arc<dyn IrqLine> {
        chipset.as_ref().map_or_else(
            || Arc::new(KvmIrqLine::new(vm_fd.clone(), gsi)) as Arc<dyn IrqLine>,
            |chipset| chipset.irq_source(id, gsi),
        )
    };
    let wiring = |address: Option<PciAddress>,
                  vectors: u16,
                  transitional: bool|
     -> Result<crate::factory::PciWiring, VmmError> {
        Ok(crate::factory::PciWiring {
            msi: msi.clone(),
            intx: if firmware_machine {
                Some(
                    crate::q35::PirqRouter::line(
                        &pirqs,
                        address.ok_or_else(|| {
                            VmmError::InvalidMachine("missing PCI address".into())
                        })?,
                        1,
                    )
                    .map_err(VmmError::Other)?,
                )
            } else {
                None
            },
            msix_vectors: vectors,
            transitional: firmware_machine && transitional,
        })
    };
    let place = |function: Arc<Mutex<dyn vmlab_hv_devices::PciDevice>>,
                 address: Option<PciAddress>|
     -> Result<(), VmmError> {
        let mut root = pci_root.lock();
        if firmware_machine {
            let address = address.ok_or_else(|| {
                VmmError::InvalidMachine("q35 function requires PCI address".into())
            })?;
            root.add_device_at(address.device, address.function, function)
                .map_err(VmmError::Other)
        } else {
            root.add_device(function)
                .map(|_| ())
                .map_err(VmmError::Other)
        }
    };
    let mut snap_disks = Vec::new();
    let mut snap_nodes = vec![DeviceNode::Plain(config_io)];
    let mut pflash = None;
    if firmware_machine {
        let inventory = Arc::new(Mutex::new(
            vmlab_hv_devices::chipset::cpu::CpuInventory::new(desc.vcpus),
        ));
        io_bus
            .register(0xcd8, 12, inventory.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Plain(inventory));
    }
    let mch = if let Boot::Firmware {
        firmware,
        code,
        vars,
        secure_boot,
        ..
    } = &desc.boot
    {
        let image = std::fs::read(code)
            .map_err(|e| VmmError::Other(format!("reading firmware {}: {e}", code.display())))?;
        let first_slot = layout.regions.len() as u32;
        let debug_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&desc.serial_log)
            .map_err(|e| VmmError::Other(format!("opening firmware debug log: {e}")))?;
        io_bus
            .register(
                0x402,
                1,
                Arc::new(Mutex::new(crate::q35::DebugPort(debug_log))),
            )
            .map_err(VmmError::Other)?;
        ram.firmware_rom = Some(if *firmware == FirmwareKind::Ovmf {
            let flash = Arc::new(Mutex::new(
                vmlab_hv_devices::pflash::Pflash::open(code, vars.as_ref().unwrap(), *secure_boot)
                    .map_err(|e| VmmError::Other(e.to_string()))?,
            ));
            mmio_bus
                .register(
                    vmlab_hv_devices::pflash::BASE,
                    vmlab_hv_devices::pflash::SIZE as u64,
                    flash.clone(),
                )
                .map_err(VmmError::Other)?;
            snap_nodes.push(DeviceNode::Resettable(flash.clone()));
            pflash = Some(flash);
            crate::q35::map_pflash_code(&kvm.vm, &image, first_slot)?
        } else {
            crate::q35::map_bios(&kvm.vm, &image, first_slot)?
        });
        if let Some(a20) = &ram.a20 {
            a20.lock()
                .attach_rom(ram.firmware_rom.as_ref().unwrap().clone(), first_slot)?;
        }
        let pam = Arc::new(Mutex::new(if secure_smm {
            crate::q35::Pam::with_isolated_smram(&image, kvm.vm.clone(), first_slot + 1)?
        } else {
            crate::q35::Pam::new(&image, Some(kvm.vm.clone()), first_slot + 1)?
        }));
        ram.mem = ram
            .mem
            .insert_region(pam.lock().shadow_region())
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        if !secure_smm {
            ram.mem = ram
                .mem
                .insert_region(pam.lock().smram_region())
                .map_err(|e| VmmError::Memory(e.to_string()))?;
        }
        mmio_bus
            .register(0xc0000, 0x40000, pam.clone())
            .map_err(VmmError::Other)?;
        if let Some(a20) = &ram.a20 {
            a20.lock().attach_pam(pam.clone());
        }
        let mut mch = if pc {
            crate::q35::Mch::i440fx(pam, layout.ram)
        } else {
            crate::q35::Mch::new(pam)
        };
        if secure_smm {
            let control = crate::smm::SmramControl::new(physical_low_end, 64)?;
            let mut geometry = control.clone();
            geometry.write_config(0x9e, 7);
            geometry.write_config(0x9d, 8);
            let memory = Arc::new(Mutex::new(crate::smm::Smram::new(
                kvm.vm.clone(),
                &geometry.ranges(),
                first_slot + 15,
            )?));
            let control = Arc::new(Mutex::new(control));
            memory.lock().set_region_open(0, true)?;
            memory.lock().set_region_open(2, true)?;
            snap_nodes.push(DeviceNode::Plain(control.clone()));
            snap_nodes.push(DeviceNode::Plain(memory.clone()));
            mch = mch.with_smm(control, memory);
        }
        let mch = Arc::new(Mutex::new(mch));
        pci_root
            .lock()
            .add_device_at(0, 0, mch.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Plain(mch.clone()));
        Some(mch)
    } else {
        None
    };
    // Firmware construction adds PAM RAM to GuestMemory. Assign devices only
    // after those regions exist, so their IOMMU domain sees the complete RAM map.
    let vfio = crate::vfio::build(desc, &kvm, &ram.mem).map_err(VmmError::Other)?;
    for (device, config) in vfio.iter().zip(&desc.vfio) {
        place(device.clone(), Some(config.pci))?;
    }
    if let Some(chipset) = &chipset {
        snap_nodes.push(DeviceNode::Plain(Arc::new(Mutex::new(
            crate::chipset::ChipsetRegisters(chipset.clone()),
        ))));
        if let Some(bridge) = &chipset.bridge {
            pci_root
                .lock()
                .add_device_at(1, 0, bridge.clone())
                .map_err(VmmError::Other)?;
            pci_root
                .lock()
                .add_device_at(1, 3, chipset.lpc.clone())
                .map_err(VmmError::Other)?;
            snap_nodes.push(DeviceNode::Plain(bridge.clone()));
        } else {
            pci_root
                .lock()
                .add_device_at(31, 0, chipset.lpc.clone())
                .map_err(VmmError::Other)?;
            pci_root
                .lock()
                .add_device_at(31, 3, chipset.smbus.clone())
                .map_err(VmmError::Other)?;
        }
        mmio_bus
            .register(0xfed00000, 0x400, chipset.hpet.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.extend([
            DeviceNode::Plain(chipset.lpc.clone()),
            DeviceNode::Plain(chipset.smbus.clone()),
            DeviceNode::Resettable(chipset.rtc.clone()),
            DeviceNode::Plain(chipset.ps2.clone()),
            DeviceNode::Plain(chipset.hpet.clone()),
            DeviceNode::Plain(chipset.isa.clone()),
        ]);
    }

    if !firmware_machine {
        let rtc = Arc::new(Mutex::new(crate::clock::KvmRtc::new(
            desc.memory_mib << 20,
            crate::clock::rtc_base(desc.rtc_base.as_deref())?,
            Arc::new(KvmIrqLine::new(vm_fd.clone(), 8)),
            vm_fd.clone(),
        )?));
        io_bus
            .register(0x70, 8, rtc.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Resettable(rtc));
    }

    let mut serials = Vec::new();
    let irq4 = isa_irq(
        6 + u32::from(!desc.serial_log_port.is_com1()),
        desc.serial_log_port.irq(),
    );
    let serial =
        factory
            .serial(&desc.serial_log, irq4)
            .map_err(|e| VmmError::DeviceUnavailable {
                device: e.device,
                reason: format!("serial (COM1): {}", e.reason),
            })?;
    io_bus
        .register(desc.serial_log_port.io_base(), 8, serial.device)
        .map_err(VmmError::Other)?;
    if let Some(node) = serial.snapshot {
        snap_nodes.push(DeviceNode::Plain(node));
    }
    if let Some(clock) = serial.clock {
        serials.push(clock);
    }
    for port in desc.ports.iter().filter(|port| !port.transport.is_virtio()) {
        let com = port.transport.com_port().unwrap();
        let serial = Arc::new(Mutex::new(
            vmlab_hv_devices::serial::Serial::socket(
                format!("serial-{:?}", com),
                &port.socket,
                isa_irq(6 + u32::from(!com.is_com1()), com.irq()),
            )
            .map_err(|error| {
                VmmError::Other(format!("serial socket {}: {error}", port.socket.display()))
            })?,
        ));
        io_bus
            .register(com.io_base(), 8, serial.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Resettable(serial.clone()));
        serials.push(serial);
    }

    let vga = if let Some(display) = &desc.display {
        let virtio = display.device == vmlab_hv_proto::machine::DisplayDevice::VirtioVga;
        let path = display.rom.as_deref().unwrap_or(std::path::Path::new(
            if display.device == vmlab_hv_proto::machine::DisplayDevice::VmwareSvga {
                "/usr/share/qemu/vgabios-vmware.bin"
            } else if virtio {
                "/usr/share/qemu/vgabios-virtio.bin"
            } else {
                "/usr/share/qemu/vgabios-stdvga.bin"
            },
        ));
        let rom = std::fs::read(path)
            .map_err(|e| VmmError::Other(format!("reading VGA ROM {}: {e}", path.display())))?;
        Some(if virtio {
            let transport = wiring(Some(display.pci), 3, false)?;
            let vga = Arc::new(Mutex::new(
                vmlab_hv_devices::display::virtio_vga::VirtioVga::new(
                    "vga",
                    &rom,
                    ram.mem.clone(),
                    Some(transport.msi),
                    transport.intx,
                )
                .map_err(|e| VmmError::Other(e.to_string()))?,
            ));
            if let Some(mode) = display.gpu {
                let mode = match mode {
                    vmlab_hv_proto::machine::GpuMode::Virgl => {
                        vmlab_hv_devices::virtio::gpu::GpuMode::Virgl
                    }
                    vmlab_hv_proto::machine::GpuMode::Venus => {
                        vmlab_hv_devices::virtio::gpu::GpuMode::Venus
                    }
                };
                let node = display.render_node.clone().ok_or_else(|| {
                    VmmError::Other("GPU acceleration requires render_node".into())
                })?;
                let mapper = Arc::new(crate::gpu::Aperture::new(
                    vm_fd.clone(),
                    kvm.kvm.get_nr_memslots() as u32,
                ));
                vga.lock()
                    .enable_3d(mode, node, mapper)
                    .map_err(|e| VmmError::Other(e.to_string()))?;
            }
            place(vga.clone(), Some(display.pci))?;
            snap_nodes.push(DeviceNode::Resettable(vga.clone()));
            crate::display::Video::Virtio(vga)
        } else {
            let mut device = vmlab_hv_devices::display::StandardVga::new("vga", &rom)
                .map_err(|e| VmmError::Other(e.to_string()))?;
            if display.device == vmlab_hv_proto::machine::DisplayDevice::VmwareSvga {
                device.enable_vmware();
            }
            let vga = Arc::new(Mutex::new(device));
            place(vga.clone(), Some(display.pci))?;
            snap_nodes.push(DeviceNode::Resettable(vga.clone()));
            crate::display::Video::Standard(vga)
        })
    } else {
        None
    };
    if let Some(config) = &desc.applesmc {
        let smc = Arc::new(Mutex::new(
            vmlab_hv_devices::applesmc::AppleSmc::new(&config.osk)
                .map_err(|e| VmmError::Other(e.to_string()))?,
        ));
        io_bus
            .register(0x300, 0x20, smc.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Resettable(smc));
    }
    let add_pci = |_root: &Arc<Mutex<PciRoot>>,
                   built: Result<crate::factory::BuiltPci, crate::error::DeviceUnavailable>,
                   what: String|
     -> Result<crate::factory::BuiltPci, VmmError> {
        let built = built.map_err(|e| VmmError::DeviceUnavailable {
            device: e.device,
            reason: format!("{what}: {}", e.reason),
        })?;
        Ok(built)
    };

    let ide = if pc {
        let ide = Arc::new(Mutex::new(vmlab_hv_devices::ide::Ide::new(
            "piix3-ide",
            ram.mem.clone(),
            [isa_irq(8, 14), isa_irq(9, 15)],
        )));
        pci_root
            .lock()
            .add_device_at(1, 1, ide.clone())
            .map_err(VmmError::Other)?;
        for (channel, task, control) in [(0, 0x1f0, 0x3f6), (1, 0x170, 0x376)] {
            io_bus
                .register(
                    task,
                    8,
                    Arc::new(Mutex::new(vmlab_hv_devices::ide::IdePorts::new(
                        ide.clone(),
                        channel,
                        false,
                    ))),
                )
                .map_err(VmmError::Other)?;
            io_bus
                .register(
                    control,
                    1,
                    Arc::new(Mutex::new(vmlab_hv_devices::ide::IdePorts::new(
                        ide.clone(),
                        channel,
                        true,
                    ))),
                )
                .map_err(VmmError::Other)?;
        }
        snap_nodes.push(DeviceNode::Virtio(ide.clone()));
        Some(ide)
    } else {
        None
    };
    let ahci = if let Some(address) = desc.ahci_pci {
        let transport = wiring(Some(address), 1, false)?;
        let ahci = Arc::new(Mutex::new(vmlab_hv_devices::ahci::Ahci::new(
            "ahci",
            ram.mem.clone(),
            transport.intx.unwrap(),
            transport.msi,
        )));
        place(ahci.clone(), Some(address))?;
        snap_nodes.push(DeviceNode::Virtio(ahci.clone()));
        Some(ahci)
    } else {
        None
    };
    for disk in &desc.disks {
        if disk.bus == DiskBus::Ide {
            let image = disk
                .path
                .as_ref()
                .map(|_| crate::factory::open_image(disk))
                .transpose()
                .map_err(|error| VmmError::Other(error.to_string()))?;
            let mut controller = ide.as_ref().unwrap().lock();
            let channel = disk.channel.unwrap() as usize;
            let unit = disk.unit.unwrap() as usize;
            if disk.cdrom {
                controller.attach_cdrom(
                    channel,
                    unit,
                    image.as_ref().map(|image| {
                        Box::new(crate::factory::SharedDisk(image.clone()))
                            as Box<dyn vmlab_disk::DiskImage>
                    }),
                )
            } else {
                controller.attach_disk(
                    channel,
                    unit,
                    Box::new(crate::factory::SharedDisk(image.as_ref().unwrap().clone())),
                )
            }
            .map_err(|error| VmmError::Other(format!("IDE {}: {error}", disk.id)))?;
            if let Some(image) = image {
                snap_disks.push(DiskNode {
                    id: disk.id.clone(),
                    writable: !disk.read_only,
                    vmstate: disk.vmstate,
                    image,
                });
            }
            continue;
        }
        if disk.bus == DiskBus::Ahci {
            if disk.cdrom && disk.path.is_none() {
                ahci.as_ref()
                    .unwrap()
                    .lock()
                    .attach_cdrom(disk.port.unwrap() as usize, None)
                    .map_err(|e| VmmError::Other(format!("empty AHCI CD-ROM {}: {e}", disk.id)))?;
                continue;
            }
            let image =
                crate::factory::open_image(disk).map_err(|e| VmmError::DeviceUnavailable {
                    device: e.device,
                    reason: e.reason,
                })?;
            let port = disk.port.unwrap() as usize;
            let controller = ahci.as_ref().unwrap();
            let attach = if disk.cdrom {
                controller.lock().attach_cdrom(
                    port,
                    Some(Box::new(crate::factory::SharedDisk(image.clone()))),
                )
            } else {
                controller
                    .lock()
                    .attach_disk(port, Box::new(crate::factory::SharedDisk(image.clone())))
            };
            attach.map_err(|e| VmmError::Other(format!("AHCI port {port}: {e}")))?;
            snap_disks.push(DiskNode {
                id: disk.id.clone(),
                writable: !disk.read_only,
                vmstate: disk.vmstate,
                image,
            });
            continue;
        }
        let blk = factory
            .virtio_blk(
                disk,
                &ram.mem,
                wiring(disk.pci, (desc.vcpus + 1) as u16, true)?,
            )
            .map_err(|e| VmmError::DeviceUnavailable {
                device: e.device,
                reason: format!(
                    "disk `{}` (/dev/vd{}): {}",
                    disk.id,
                    disk_index_letter(disk, &desc.disks),
                    e.reason
                ),
            })?;
        place(blk.pci.function.clone(), disk.pci)?;
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
    let virtio_ports: Vec<_> = desc
        .ports
        .iter()
        .filter(|port| port.transport.is_virtio())
        .cloned()
        .collect();
    if !virtio_ports.is_empty() {
        let built = add_pci(
            &pci_root,
            factory.virtio_console(&virtio_ports, &ram.mem, wiring(desc.console_pci, 2, true)?),
            "virtio-console ports".to_string(),
        )?;
        place(built.function.clone(), desc.console_pci)?;
        if let Some(node) = built.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
    }
    // One NIC per interface.
    for nic in &desc.nics {
        let built = add_pci(
            &pci_root,
            factory.virtio_net(nic, &ram.mem, wiring(nic.pci, 4, true)?),
            format!("nic {}", nic.mac),
        )?;
        place(built.function.clone(), nic.pci)?;
        if let Some(node) = built.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
    }
    // One vhost-user-fs function per share.
    for share in &desc.shares {
        let built = add_pci(
            &pci_root,
            factory.fs_share(share, &ram.mem, wiring(share.pci, 2, false)?),
            format!("share `{}`", share.tag),
        )?;
        place(built.function.clone(), share.pci)?;
        if let Some(node) = built.snapshot {
            snap_nodes.push(DeviceNode::Virtio(node));
        }
    }

    // ACPI PM1a block on IRQ 9.
    let sleep_signal = Arc::new(SleepSignal::default());
    let sci: Arc<dyn IrqLine> = Arc::new(KvmIrqLine::new(vm_fd, SCI_GSI));
    let pm = Arc::new(Mutex::new(PmDevice::new(sci, sleep_signal.sink())));
    if !firmware_machine {
        io_bus
            .register(PM1A_EVT_BASE, PM_BLOCK_LEN, pm.clone())
            .map_err(VmmError::Other)?;
    }
    // One xHCI carries the absolute tablet and the host passthrough ports:
    // `tablet` places it for input, `usb` (vmlab's `usb = true`, the
    // controller it names `usbx`) additionally enables hot-plug.
    let xhci_address = match (desc.tablet, desc.usb) {
        (Some(tablet), Some(usb)) if tablet != usb => {
            return Err(VmmError::DeviceUnavailable {
                device: "usb controller",
                reason: "the tablet and usb functions must share one xHCI".into(),
            });
        }
        (tablet, usb) => tablet.or(usb),
    };
    let xhci = if let Some(address) = xhci_address {
        let transport = wiring(Some(address), 1, false)?;
        let xhci = Arc::new(Mutex::new(vmlab_hv_devices::usb::XhciController::new(
            "xhci",
            ram.mem.clone(),
            transport.msi,
            transport.intx,
        )));
        {
            let mut controller = xhci.lock();
            controller
                .detach(vmlab_hv_devices::usb::TABLET_PORT)
                .map_err(|e| VmmError::Other(e.to_string()))?;
            controller
                .attach(
                    vmlab_hv_devices::usb::TABLET_PORT + u8::from(desc.usb_keyboard),
                    Box::new(vmlab_hv_devices::usb::Tablet::new(hid_serial(
                        address,
                        28754,
                        if desc.usb_keyboard { 2 } else { 1 },
                    ))),
                )
                .map_err(|e| VmmError::Other(e.to_string()))?;
        }
        if desc.usb_keyboard {
            xhci.lock()
                .attach(
                    vmlab_hv_devices::usb::KEYBOARD_PORT,
                    Box::new(vmlab_hv_devices::usb::Keyboard::new(hid_serial(
                        address, 68284, 1,
                    ))),
                )
                .map_err(|e| VmmError::Other(e.to_string()))?;
        }
        place(xhci.clone(), Some(address))?;
        snap_nodes.push(DeviceNode::Virtio(xhci.clone()));
        Some(xhci)
    } else {
        None
    };
    if let Some(section) = &desc.audio {
        // The host backend is picked per machine from the description's
        // node identity; capture opens only when the microphone is on.
        let host = crate::audio::host(section);
        match section.device {
            vmlab_hv_proto::machine::AudioDevice::Hda => {
                let hda = Arc::new(Mutex::new(vmlab_hv_devices::hda::Hda::new(
                    "hda",
                    ram.mem.clone(),
                    host,
                    msi.clone(),
                    section.microphone,
                )));
                place(hda.clone(), section.pci)?;
                snap_nodes.push(DeviceNode::Plain(hda.clone()));
                chipset
                    .as_ref()
                    .expect("audio requires the q35 chipset")
                    .audio
                    .lock()
                    .replace(hda.clone());
            }
            vmlab_hv_proto::machine::AudioDevice::VirtioSnd => {
                // One device shared two ways: the transport owns the
                // PCI-facing handle, the chipset clock paces completions
                // through the other side of the same mutex.
                let (snd, pci_device) = vmlab_hv_devices::virtio::sound::VirtioSnd::new_shared(
                    "virtio-snd",
                    host,
                    section.microphone,
                );
                let interrupt = wiring(section.pci, 5, true)?;
                let function = Arc::new(Mutex::new(
                    vmlab_hv_devices::virtio::pci::VirtioPciDevice::new(
                        "virtio-snd",
                        Box::new(pci_device),
                        ram.mem.clone(),
                        Some(interrupt.msi),
                        interrupt.intx,
                    ),
                ));
                place(function.clone(), section.pci)?;
                snap_nodes.push(DeviceNode::Virtio(function.clone()));
                chipset
                    .as_ref()
                    .expect("audio requires the q35 chipset")
                    .audio
                    .lock()
                    .replace(snd);
            }
        }
    }
    let tpm = if let Some(tpm) = &desc.tpm {
        let device = Arc::new(Mutex::new(
            vmlab_hv_devices::tpm::TpmTis::connect(&tpm.socket, None)
                .map_err(|e| VmmError::Other(e.to_string()))?,
        ));
        mmio_bus
            .register(
                vmlab_hv_devices::tpm::MMIO_BASE,
                vmlab_hv_devices::tpm::MMIO_SIZE,
                device.clone(),
            )
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Resettable(device.clone()));
        Some(device)
    } else {
        None
    };
    let mut smi_fwcfg = None;
    if let Boot::Firmware { bootorder, .. } = &desc.boot {
        let mut ram_regions = vec![(0, physical_low_end)];
        ram_regions.extend(
            layout
                .regions
                .iter()
                .filter(|(base, _)| *base >= 1 << 32)
                .copied(),
        );
        let platform = vmlab_hv_firmware::Platform {
            applesmc: desc.applesmc.is_some(),
            amd_hypertransport: kvm.amd_hypertransport(desc.cpu_model.as_deref()),
            vcpus: desc.vcpus as u16,
            max_cpus: desc.vcpus as u16,
            ram_regions,
            pci_devices: pci_root
                .lock()
                .addresses()
                .into_iter()
                .map(|(device, function)| vmlab_hv_firmware::PciDevice {
                    device,
                    function,
                    vga: desc.display.as_ref().is_some_and(|d| {
                        d.device == vmlab_hv_proto::machine::DisplayDevice::StdVga
                            && d.pci.device == device
                            && d.pci.function == function
                    }),
                })
                .collect(),
            ecam_base: if ovmf { 0xe0000000 } else { 0xb0000000 },
            pci64_base: Some(if ovmf {
                kvm.ovmf_pci64_base(desc.cpu_model.as_deref())
            } else {
                kvm.seabios_pci64_base(layout.ram_top() > 1 << 32)
            }),
            tpm: tpm.is_some(),
            smm: secure_smm,
        };
        let mut options = vmlab_hv_firmware::FirmwareOptions {
            bootorder: if pc && bootorder.is_empty() {
                crate::pc::bootorder(&desc.disks)
            } else {
                bootorder.clone()
            },
            nographic: desc.display.is_none(),
            ..Default::default()
        };
        options.smbios.baseboard = desc.smbios_type2;
        if pc || desc.cpu_model.is_some() {
            options.smbios.processor_id =
                kvm.processor_id(desc.vcpus, desc.cpu, desc.cpu_model.as_deref())?;
        }
        let fw_cfg = if pc {
            let platform = vmlab_hv_firmware::pc::Platform {
                vcpus: platform.vcpus,
                max_cpus: platform.max_cpus,
                ram_regions: platform.ram_regions,
                pci_devices: platform.pci_devices.into_iter().map(Into::into).collect(),
                pci64_base: if layout.ram_top() > 1 << 32 {
                    platform.pci64_base
                } else {
                    None
                },
                com2: pc_com2,
                physical_address_bits: kvm.physical_address_bits(desc.cpu_model.as_deref()),
            };
            vmlab_hv_firmware::FwCfg::for_pc(ram.mem.clone(), &platform, &options)
        } else {
            vmlab_hv_firmware::FwCfg::for_platform(ram.mem.clone(), &platform, &options)
        }
        .map_err(VmmError::Other)?;
        let fw_cfg = Arc::new(Mutex::new(if pc {
            crate::smm::SmiFwCfg::without_negotiation(fw_cfg, ram.mem.clone())
        } else {
            crate::smm::SmiFwCfg::new(fw_cfg, ram.mem.clone()).map_err(VmmError::Other)?
        }));
        io_bus
            .register(0x510, 0xc, fw_cfg.clone())
            .map_err(VmmError::Other)?;
        snap_nodes.push(DeviceNode::Plain(fw_cfg.clone()));
        smi_fwcfg = Some(fw_cfg);
    }

    if pc {
        let mut disks = [None; 4];
        for disk in desc
            .disks
            .iter()
            .filter(|disk| disk.bus == DiskBus::Ide && !disk.cdrom)
        {
            let image = snap_disks.iter().find(|image| image.id == disk.id).unwrap();
            disks[(disk.channel.unwrap() * 2 + disk.unit.unwrap()) as usize] =
                Some(image.image.lock().size() / 512);
        }
        let cd_first = desc.disks.iter().any(|disk| disk.cdrom);
        chipset
            .as_ref()
            .unwrap()
            .rtc
            .lock()
            .configure_pc(disks, cd_first, desc.vcpus as u8);
    }
    if let Some(a20) = &ram.a20 {
        snap_nodes.push(DeviceNode::Plain(a20.clone()));
    }

    let shared = VmShared::new(
        kvm,
        io_bus,
        mmio_bus,
        pci_root,
        pm,
        sleep_signal,
        layout.mmio64_base,
    );
    *shared.mch.lock() = mch;
    *shared.serials.lock() = serials;
    *shared.vfio.lock() = vfio;
    *shared.chipset.lock() = chipset;
    *shared.vga.lock() = vga;
    *shared.xhci.lock() = xhci;
    shared
        .usb_hotplug
        .store(desc.usb.is_some(), std::sync::atomic::Ordering::SeqCst);
    *shared.pflash.lock() = pflash;
    *shared.tpm.lock() = tpm;
    *shared.smi_fwcfg.lock() = smi_fwcfg;
    *shared.msr_policy.lock() = desc.msr_policy;
    crate::macos::configure_msrs(&shared.kvm, desc.msr_policy)?;
    shared.set_reset_description(desc.clone());
    shared.set_snapshot_registry(Arc::new(SnapshotRegistry {
        digest: machine_digest(desc),
        disks: snap_disks,
        nodes: snap_nodes,
    }));

    // vCPUs: the BSP enters the kernel's 64-bit entry; the APs stay in
    // reset state until the kernel's INIT-SIPI-SIPI wakes them.
    let mut vcpu_fds = Vec::with_capacity(desc.vcpus as usize);
    for id in 0..desc.vcpus {
        let mut vcpu = shared.kvm.create_vcpu_with_model(
            id as u8,
            desc.vcpus,
            desc.cpu,
            desc.cpu_model.as_deref(),
        )?;
        crate::hyperv::configure_vcpu(&shared.kvm.kvm, &vcpu, &desc.hyperv)?;
        crate::macos::configure_tsc(&vcpu, desc, id == 0)?;
        if id == 0
            && let Some(boot) = &boot
        {
            loader::init_vcpu(&mut vcpu, boot)?;
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
    let bytes = serde_json::to_vec(&desc.redacted()).unwrap_or_default();
    Sha256::digest(&bytes).into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn vfio_requires_q35_unique_host_and_unoccupied_guest_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let mut machine = desc();
        machine.vfio.push(vmlab_hv_proto::machine::VfioDevice {
            host: "0000:65:00.0".into(),
            pci: PciAddress {
                bus: 0,
                device: 6,
                function: 0,
            },
        });
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("machine=q35")
        );
        machine.machine = Machine::Q35;
        machine.boot = Boot::Firmware {
            firmware: FirmwareKind::Seabios,
            code: tiny_bzimage(dir.path()),
            vars: None,
            secure_boot: false,
            bootorder: vec![],
        };
        validate(&machine).unwrap();
        machine.vfio.push(machine.vfio[0].clone());
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("duplicate VFIO host")
        );
        machine.vfio[1].host = "0000:65:00.1".into();
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("duplicate PCI address")
        );
        machine.vfio.pop();
        machine.vfio[0].pci.device = 31;
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("reserved for the chipset")
        );
    }
    #[test]
    fn tablet_serial_uses_the_explicit_controller_address() {
        assert_eq!(
            super::hid_serial(
                vmlab_hv_proto::machine::PciAddress {
                    bus: 0,
                    device: 5,
                    function: 0,
                },
                28754,
                1
            ),
            "28754-0000:00:05.0-1"
        );
        assert_eq!(
            super::hid_serial(
                vmlab_hv_proto::machine::PciAddress {
                    bus: 0,
                    device: 26,
                    function: 2,
                },
                28754,
                1
            ),
            "28754-0000:00:1a.2-1"
        );
    }
    use super::*;
    use vmlab_hv_proto::machine::{Boot, Disk, DiskFormat, MachineDescription};

    fn desc() -> MachineDescription {
        MachineDescription {
            exit_on_reset: false,
            vfio: vec![],
            machine: Default::default(),
            cpu: Default::default(),
            cpu_model: None,
            invtsc: false,
            tsc_khz: None,
            vmware_cpuid_freq: false,
            msr_policy: Default::default(),
            applesmc: None,
            usb_keyboard: false,
            smbios_type2: false,
            floppy: None,
            serial_log_port: Default::default(),
            hyperv: vec![],
            display: None,
            tablet: None,
            usb: None,
            tpm: None,
            console_pci: None,
            ahci_pci: None,
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
            audio: None,
            rtc_base: None,
        }
    }

    #[test]
    fn pc_accepts_each_nic_model_but_q35_keeps_virtio_only() {
        let mut machine = desc();
        machine.machine = Machine::Pc;
        machine.boot = Boot::Firmware {
            firmware: vmlab_hv_proto::machine::FirmwareKind::Seabios,
            code: "/bios".into(),
            vars: None,
            secure_boot: false,
            bootorder: vec![],
        };
        for model in ["virtio", "e1000", "pcnet"] {
            machine.nics =
                vec![serde_json::from_value(serde_json::json!({
                "model":model,"mac":"52:54:00:12:34:56","pci":{"bus":0,"device":3,"function":0},
                "attachment":{"kind":"stream","socket":"/switch"}
            })).unwrap()];
            assert!(validate_legacy_features(&machine).is_ok(), "pc NIC {model}");
            machine.machine = Machine::Q35;
            assert_eq!(
                validate_legacy_features(&machine).is_ok(),
                model == "virtio"
            );
            machine.machine = Machine::Pc;
        }
    }

    #[test]
    fn gpu_configuration_requires_virtio_vga_and_a_render_node() {
        let mut machine = desc();
        machine.display = Some(
            serde_json::from_value(serde_json::json!({
                "device": "std-vga", "gpu": "virgl", "render_node": "/dev/dri/renderD130",
                "pci": {"bus": 0, "device": 4, "function": 0}, "rom": null,
            }))
            .unwrap(),
        );
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("requires virtio-vga")
        );
        let display = machine.display.as_mut().unwrap();
        display.device = vmlab_hv_proto::machine::DisplayDevice::VirtioVga;
        display.render_node = None;
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("specified together")
        );
        let display = machine.display.as_mut().unwrap();
        display.gpu = None;
        display.render_node = Some("/dev/dri/renderD130".into());
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("specified together")
        );
    }

    #[test]
    fn microvm_rejects_q35_devices_before_building() {
        let mut machine = desc();
        machine.ahci_pci = Some(PciAddress {
            bus: 0,
            device: 31,
            function: 2,
        });
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("machine=q35")
        );
        machine.ahci_pci = None;
        machine.disks.push(Disk {
            channel: None,
            unit: None,
            pci: None,
            bus: DiskBus::Ahci,
            port: Some(0),
            cdrom: false,
            id: "disk0".into(),
            path: Some("/disk".into()),
            format: DiskFormat::Raw,
            read_only: false,
            vmstate: false,
        });
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("machine=q35")
        );
    }

    #[test]
    fn only_ahci_cdroms_can_omit_the_image_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut machine = desc();
        machine.machine = Machine::Q35;
        machine.boot = Boot::Firmware {
            firmware: FirmwareKind::Seabios,
            code: tiny_bzimage(dir.path()),
            vars: None,
            secure_boot: false,
            bootorder: vec![],
        };
        machine.ahci_pci = Some(PciAddress {
            bus: 0,
            device: 31,
            function: 2,
        });
        machine.disks.push(Disk {
            channel: None,
            unit: None,
            pci: None,
            bus: DiskBus::Ahci,
            port: Some(2),
            cdrom: true,
            id: "cdrom0".into(),
            path: None,
            format: DiskFormat::Raw,
            read_only: true,
            vmstate: false,
        });
        validate(&machine).unwrap();
        machine.disks[0].cdrom = false;
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("requires an image path")
        );
        machine.disks[0].cdrom = true;
        machine.disks[0].vmstate = true;
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("requires an image path")
        );
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

    #[test]
    fn audio_is_a_q35_hda_at_00_1b_0() {
        use vmlab_hv_proto::machine::{Audio, AudioDevice};

        fn section(device: AudioDevice, pci: Option<PciAddress>) -> Audio {
            Audio {
                pci,
                device,
                microphone: true,
                node_name: "vmlab.lab.machine".into(),
                node_description: "a machine".into(),
            }
        }
        const HDA_SLOT: PciAddress = PciAddress {
            bus: 0,
            device: 0x1b,
            function: 0,
        };

        // The microvm keeps phase 1's shape: no audio at all.
        let mut machine = desc();
        machine.audio = Some(section(AudioDevice::Hda, None));
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("machine=q35")
        );

        // q35 wants an explicit address, HDA exactly at ICH9's 00:1b.0 (D11).
        let dir = tempfile::tempdir().unwrap();
        let mut machine = desc();
        machine.machine = Machine::Q35;
        machine.boot = Boot::Firmware {
            firmware: FirmwareKind::Seabios,
            code: tiny_bzimage(dir.path()),
            vars: None,
            secure_boot: false,
            bootorder: vec![],
        };
        machine.audio = Some(section(AudioDevice::Hda, None));
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("00:1b.0"),
            "an address-less audio section must name the fixed slot"
        );
        machine.audio = Some(section(
            AudioDevice::Hda,
            Some(PciAddress {
                bus: 0,
                device: 5,
                function: 0,
            }),
        ));
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("00:1b.0")
        );
        machine.audio = Some(section(AudioDevice::Hda, Some(HDA_SLOT)));
        validate(&machine).unwrap();

        // virtio-snd landed: a plain virtio function that takes the
        // address the description names, like a NIC.
        machine.audio = Some(section(AudioDevice::VirtioSnd, None));
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("requires an explicit PCI address")
        );
        let slot = PciAddress {
            bus: 0,
            device: 5,
            function: 0,
        };
        machine.audio = Some(section(AudioDevice::VirtioSnd, Some(slot)));
        validate(&machine).unwrap();
        // It may not take a chipset slot, and the microvm still has none
        // of this.
        machine.audio = Some(section(
            AudioDevice::VirtioSnd,
            Some(PciAddress {
                bus: 0,
                device: 31,
                function: 3,
            }),
        ));
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("reserved for the chipset")
        );
        let mut microvm = desc();
        microvm.audio = Some(section(AudioDevice::VirtioSnd, Some(slot)));
        assert!(validate(&microvm).is_err());
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
        let dir = tempfile::tempdir().unwrap();
        let mut d = desc();
        d.boot = Boot::Kernel {
            kernel: dir.path().join("broken-vmlinuz"),
            initrd: None,
            cmdline: String::new(),
        };
        d.serial_log = dir.path().join("serial.log");
        std::fs::write(dir.path().join("broken-vmlinuz"), b"not a bzImage").unwrap();
        d.disks = vec![Disk {
            channel: None,
            unit: None,
            pci: None,
            bus: Default::default(),
            port: None,
            cdrom: false,
            id: "rootfs".into(),
            path: Some("/disk.raw".into()),
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
            kernel: tiny_bzimage(dir.path()),
            initrd: None,
            cmdline: String::new(),
        };
        let Err(err) = build(&d, &crate::factory::NoDevices, true) else {
            panic!("building without device models must fail");
        };
        let err = err.to_string();
        assert!(err.contains("serial") && err.contains("COM1"), "{err}");
    }
    #[test]
    fn usb_and_tablet_must_share_one_controller() {
        let dir = tempfile::tempdir().unwrap();
        let mut machine = desc();
        machine.machine = Machine::Q35;
        machine.boot = Boot::Firmware {
            firmware: FirmwareKind::Seabios,
            code: tiny_bzimage(dir.path()),
            vars: None,
            secure_boot: false,
            bootorder: vec![],
        };
        machine.tablet = Some(PciAddress {
            bus: 0,
            device: 5,
            function: 0,
        });
        machine.usb = Some(PciAddress {
            bus: 0,
            device: 6,
            function: 0,
        });
        assert!(
            validate(&machine)
                .unwrap_err()
                .to_string()
                .contains("one xHCI controller")
        );
        machine.usb = machine.tablet;
        validate(&machine).unwrap();
    }
}

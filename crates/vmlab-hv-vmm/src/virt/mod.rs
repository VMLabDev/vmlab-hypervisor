//! RISC-V virt with the Sail-derived interpreter and the existing device set.

mod cpu;
mod display;
mod fdt;
mod flash;
mod memory;
mod runtime;

use crate::{
    bus::MmioBus,
    factory::{DeviceFactory, PciWiring},
    pci::{PciEcam, PciRoot},
    snapshot::{DeviceNode, DiskNode},
};
use parking_lot::Mutex;
pub use runtime::VirtVm;
use std::{fs::File, os::unix::fs::FileExt, path::Path, sync::Arc};
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::riscv::{Clint, Plic, PlicIrqs};
use vmlab_hv_devices::{BusDevice, GuestMemory, MsiMessage, MsiSink, PciDevice};
use vmlab_hv_proto::machine::{
    Accel, Arch, Boot, DiskBus, DisplayDevice, FirmwareKind, Machine, MachineDescription,
};
use vmlab_hv_tcg::{Riscv64, Value};

pub const RAM_BASE: u64 = 0x8000_0000;
/// Validated subset of the pinned Sail configuration, per phase-3 D7.
pub const ISA: &str = "rv64imafdc_zicsr_zifencei";

struct NoMsi;
impl MsiSink for NoMsi {
    fn signal(&self, _message: MsiMessage) {}
}

fn wiring(irqs: &Arc<PlicIrqs>, slot: u8, transitional: bool, vectors: u16) -> PciWiring {
    PciWiring {
        msi: Arc::new(NoMsi),
        intx: Some(irqs.line(32 + slot as usize % 4)),
        transitional,
        msix_vectors: vectors,
    }
}

struct HostBridge;
impl PciDevice for HostBridge {
    fn config_read(&mut self, reg: usize) -> u32 {
        match reg {
            0 => 0x0008_1b36,
            2 => 0x0600_0000,
            11 => 0x1100_1af4,
            15 => 0xff,
            64.. => u32::MAX,
            _ => 0,
        }
    }
    fn config_write(&mut self, _reg: usize, _offset: u64, _data: &[u8]) {}
    fn bars(&self) -> Vec<vmlab_hv_devices::BarInfo> {
        Vec::new()
    }
    fn bar_read(&mut self, _bar: u8, _offset: u64, data: &mut [u8]) {
        data.fill(0xff);
    }
    fn bar_write(&mut self, _bar: u8, _offset: u64, _data: &[u8]) {}
}

struct TestDevice(Arc<std::sync::atomic::AtomicU32>);
impl BusDevice for TestDevice {
    fn read(&mut self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == 0 && data.len() == 4 {
            let value = u32::from_le_bytes(data.try_into().unwrap());
            if matches!(value & 0xffff, 0x5555 | 0x7777 | 0x3333) {
                let _ = self.0.compare_exchange(
                    0,
                    value,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        }
    }
}

pub fn validate(desc: &MachineDescription) -> Result<(), String> {
    if (desc.arch, desc.machine, desc.accel) != (Arch::Riscv64, Machine::Virt, Accel::Tcg) {
        return Err("RISC-V virt requires arch=riscv64, machine=virt, accel=tcg".into());
    }
    if desc.vcpus == 0 || desc.vcpus > 512 {
        return Err("virt vcpus must be in 1..=512".into());
    }
    if desc.memory_mib < 128 || desc.memory_mib > 14336 {
        return Err(
            "virt memory_mib must be in 128..=14336, below the captured PCI MMIO64 window".into(),
        );
    }
    if !desc.vfio.is_empty()
        || desc.audio.is_some()
        || desc.tpm.is_some()
        || desc.ahci_pci.is_some()
        || !desc.hyperv.is_empty()
    {
        return Err(
            "RISC-V virt does not expose x86 chipset, VFIO, audio, TPM, AHCI, or Hyper-V devices"
                .into(),
        );
    }
    if desc.display.as_ref().is_some_and(|d| {
        d.device != DisplayDevice::VirtioGpu
            || d.gpu.is_some()
            || d.render_node.is_some()
            || d.rom.is_some()
    }) {
        return Err(
            "RISC-V virt display requires virtio-gpu without VGA ROM or host GPU acceleration"
                .into(),
        );
    }
    match &desc.boot {
        Boot::Firmware {
            firmware: FirmwareKind::Edk2Riscv,
            code,
            vars: Some(vars),
            secure_boot: false,
            ..
        } => {
            for path in [code, vars] {
                if !path.is_file() {
                    return Err(format!("firmware {} does not exist", path.display()));
                }
            }
        }
        Boot::Kernel { kernel, initrd, .. } => {
            for path in std::iter::once(kernel).chain(initrd.iter()) {
                if !path.is_file() {
                    return Err(format!("boot image {} does not exist", path.display()));
                }
            }
        }
        _ => {
            return Err(
                "virt requires edk2_riscv CODE+VARS without secure_boot, or direct kernel boot"
                    .into(),
            );
        }
    }
    let mut used = std::collections::BTreeSet::from([(0, 0)]);
    if desc.disks.iter().filter(|disk| disk.vmstate).count() > 1
        || desc.disks.iter().any(|disk| {
            disk.vmstate
                && (disk.read_only || disk.format != vmlab_hv_proto::machine::DiskFormat::Qcow2)
        })
    {
        return Err("virt requires at most one writable qcow2 vmstate disk".into());
    }
    let mut address = |a: Option<vmlab_hv_proto::machine::PciAddress>| {
        if let Some(a) = a
            && (a.bus != 0
                || a.device >= 32
                || a.function >= 8
                || !used.insert((a.device, a.function)))
        {
            return Err("invalid or duplicate virt PCI address".to_string());
        }
        Ok(())
    };
    if !desc.ports.is_empty() {
        address(desc.console_pci)?;
    }
    for disk in &desc.disks {
        if disk.bus != DiskBus::Virtio
            || disk.port.is_some()
            || disk.path.as_ref().is_none_or(|p| !p.is_file())
        {
            return Err(format!(
                "virt disk {} requires an existing virtio image",
                disk.id
            ));
        }
        address(disk.pci)?;
    }
    for nic in &desc.nics {
        address(nic.pci)?;
    }
    for share in &desc.shares {
        if share.backing().is_none() {
            return Err(format!("invalid share {}", share.tag));
        }
        address(share.pci)?;
    }
    if let Some(d) = &desc.display {
        address(Some(d.pci))?;
    }
    address(desc.tablet.or(desc.usb))?;
    if desc.tablet.is_some() && desc.usb.is_some() && desc.tablet != desc.usb {
        return Err("tablet and USB must name the same xHCI controller".into());
    }
    Ok(())
}

/// Resolve the same distro fallback paths as vmlab. A machine description can
/// instead name the bundled CODE and its per-machine writable VARS copy.
pub fn firmware_paths() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let mut roots = Vec::new();
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        roots.push(std::path::PathBuf::from(data).join("vmlab/guest/firmware/riscv64"));
    } else if let Some(user) = std::env::var_os("HOME") {
        roots
            .push(std::path::PathBuf::from(user).join(".local/share/vmlab/guest/firmware/riscv64"));
    }
    roots.extend([
        "/usr/share/qemu-efi-riscv64".into(),
        "/usr/share/edk2/riscv64".into(),
        "/usr/share/edk2/riscv".into(),
    ]);
    roots.into_iter().find_map(|root| {
        let code = root.join("RISCV_VIRT_CODE.fd");
        let vars = root.join("RISCV_VIRT_VARS.fd");
        (code.is_file() && vars.is_file()).then_some((code, vars))
    })
}

pub fn build(desc: &MachineDescription, factory: &dyn DeviceFactory) -> Result<VirtVm, String> {
    validate(desc)?;
    let size = desc.memory_mib << 20;
    let ram = if desc.shares.iter().any(|share| share.socket.is_some()) {
        let file = crate::memory::create_memfd()?;
        file.set_len(size).map_err(|e| e.to_string())?;
        GuestMemory::from_ranges_with_files(std::iter::once((
            GuestAddress(RAM_BASE),
            size as usize,
            Some(vm_memory::FileOffset::new(file, 0)),
        )))
    } else {
        GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), size as usize)])
    }
    .map_err(|e| e.to_string())?;
    let clint = Arc::new(Mutex::new(Clint::new(desc.vcpus as usize)));
    let plic = Arc::new(Mutex::new(Plic::new(desc.vcpus as usize)));
    let irqs = PlicIrqs::new(plic.clone());
    let rtc_base = crate::clock::rtc_base(desc.rtc_base.as_deref()).map_err(|e| e.to_string())?;
    let rtc = Arc::new(Mutex::new(vmlab_hv_devices::riscv::rtc::GoldfishRtc::new(
        u64::try_from(rtc_base)
            .map_err(|_| "RTC base must be after the Unix epoch")?
            .checked_mul(1_000_000_000)
            .ok_or("RTC base overflows nanoseconds")?,
        irqs.line(11),
    )));
    let pci = Arc::new(Mutex::new(PciRoot::q35(0x4_0000_0000)));
    pci.lock()
        .add_device_at(0, 0, Arc::new(Mutex::new(HostBridge)))?;
    let mut bus = MmioBus::new();
    let mut nodes = vec![
        DeviceNode::Plain(clint.clone()),
        DeviceNode::Plain(plic.clone()),
    ];
    bus.register(0x200_0000, 0x10000, clint.clone())?;
    bus.register(0x101000, 0x1000, rtc.clone())?;
    nodes.push(DeviceNode::Plain(rtc.clone()));
    bus.register(0xc00_0000, 0x600000, plic.clone())?;
    bus.register(
        0x3000_0000,
        0x1000_0000,
        Arc::new(Mutex::new(PciEcam::new(pci.clone()))),
    )?;
    let serial = factory
        .serial(&desc.serial_log, irqs.line(10))
        .map_err(|e| e.to_string())?;
    bus.register(0x1000_0000, 0x100, serial.device)?;
    for slot in 1..=8 {
        bus.register(
            0x1000_0000 + slot * 0x1000,
            0x1000,
            Arc::new(Mutex::new(
                vmlab_hv_devices::riscv::virtio_mmio::EmptyVirtioMmio,
            )),
        )?;
    }
    if let Some(snapshot) = serial.snapshot {
        nodes.push(DeviceNode::Plain(snapshot));
    }
    let shutdown = Arc::new(std::sync::atomic::AtomicU32::new(0));
    bus.register(
        0x100000,
        0x1000,
        Arc::new(Mutex::new(TestDevice(shutdown.clone()))),
    )?;
    let mut fw = vmlab_hv_firmware::fw_cfg::FwCfg::new(ram.clone());
    fw.add_numeric(5, (desc.vcpus as u16).to_le_bytes().to_vec())?;
    fw.add_numeric(0x0f, (desc.vcpus as u16).to_le_bytes().to_vec())?;
    fw.add_numeric(3, size.to_le_bytes().to_vec())?;
    fw.add_file("etc/boot-fail-wait", 0u32.to_le_bytes().to_vec(), false)?;
    let fw = Arc::new(Mutex::new(vmlab_hv_firmware::fw_cfg::MmioFwCfg(fw)));
    bus.register(0x1010_0000, 0x18, fw.clone())?;
    nodes.push(DeviceNode::Plain(fw));
    let mut place = |built: crate::factory::BuiltPci,
                     address: Option<vmlab_hv_proto::machine::PciAddress>|
     -> Result<(), String> {
        let a = placement(&pci, address)?;
        pci.lock()
            .add_device_at(a.device, a.function, built.function)?;
        if let Some(snapshot) = built.snapshot {
            nodes.push(DeviceNode::Virtio(snapshot));
        }
        Ok(())
    };
    if !desc.ports.is_empty() {
        let next = desc
            .console_pci
            .map(|a| a.device)
            .map_or_else(|| next_slot(&pci), Ok)?;
        place(
            factory
                .virtio_console(&desc.ports, &ram, wiring(&irqs, next, true, 2))
                .map_err(|e| e.to_string())?,
            desc.console_pci,
        )?;
    }
    let mut disks = Vec::new();
    for disk in &desc.disks {
        let next = disk
            .pci
            .map(|a| a.device)
            .map_or_else(|| next_slot(&pci), Ok)?;
        let built = factory
            .virtio_blk(disk, &ram, wiring(&irqs, next, true, 2))
            .map_err(|e| e.to_string())?;
        disks.push(DiskNode {
            id: disk.id.clone(),
            writable: !disk.read_only,
            vmstate: disk.vmstate,
            image: built.disk,
        });
        place(built.pci, disk.pci)?;
    }
    for nic in &desc.nics {
        let next = placement(&pci, nic.pci)?;
        place(
            factory
                .virtio_net(nic, &ram, wiring(&irqs, next.device, true, 3))
                .map_err(|e| e.to_string())?,
            Some(next),
        )?;
    }
    for share in &desc.shares {
        let next = placement(&pci, share.pci)?;
        place(
            factory
                .fs_share(share, &ram, wiring(&irqs, next.device, false, 2))
                .map_err(|e| e.to_string())?,
            Some(next),
        )?;
    }
    let gpu = desc
        .display
        .as_ref()
        .map(|_| vmlab_hv_devices::virtio::gpu::Gpu::new("gpu0"));
    if let Some(gpu) = &gpu {
        let address = desc.display.as_ref().unwrap().pci;
        let wire = wiring(&irqs, address.device, false, 3);
        let device = Arc::new(Mutex::new(
            vmlab_hv_devices::virtio::pci::VirtioPciDevice::new_modern_gpu(
                "gpu0",
                Box::new(gpu.clone()),
                ram.clone(),
                Some(wire.msi),
                wire.intx,
            )
            .map_err(|e| e.to_string())?,
        ));
        place(
            crate::factory::BuiltPci {
                function: device.clone(),
                snapshot: Some(device),
            },
            Some(address),
        )?;
    }
    let xhci = if let Some(address) = desc.tablet.or(desc.usb) {
        let wire = wiring(&irqs, address.device, false, 1);
        let device = Arc::new(Mutex::new(vmlab_hv_devices::usb::XhciController::new(
            "xhci",
            ram.clone(),
            wire.msi,
            wire.intx,
        )));
        {
            let mut controller = device.lock();
            controller
                .detach(vmlab_hv_devices::usb::TABLET_PORT)
                .map_err(|e| e.to_string())?;
            if desc.tablet.is_some() {
                controller
                    .attach(
                        vmlab_hv_devices::usb::TABLET_PORT,
                        Box::new(vmlab_hv_devices::usb::Tablet::new(
                            crate::builder::tablet_serial(address),
                        )),
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        pci.lock()
            .add_device_at(address.device, address.function, device.clone())?;
        nodes.push(DeviceNode::Virtio(device.clone()));
        Some(device)
    } else {
        None
    };
    let mut flashes = Vec::new();
    let mut boot_files = Vec::new();
    let (next_entry, cmdline, initrd) = match &desc.boot {
        Boot::Firmware { code, vars, .. } => {
            let code = Arc::new(Mutex::new(flash::Flash::open("pflash-code", code, false)?));
            let vars = Arc::new(Mutex::new(flash::Flash::open(
                "pflash-vars",
                vars.as_ref().unwrap(),
                true,
            )?));
            bus.register(0x2000_0000, 32 << 20, code.clone())?;
            bus.register(0x2200_0000, 32 << 20, vars.clone())?;
            flashes.extend([(0x2000_0000, code.clone()), (0x2200_0000, vars.clone())]);
            nodes.extend([DeviceNode::Resettable(code), DeviceNode::Resettable(vars)]);
            (0x2000_0000, "", None)
        }
        Boot::Kernel {
            kernel,
            initrd,
            cmdline,
        } => {
            boot_files.push(load(
                &ram,
                kernel,
                RAM_BASE + (2 << 20),
                size / 2 - (2 << 20),
            )?);
            let initrd = if let Some(path) = initrd {
                let start = RAM_BASE + size / 2;
                let image = load(&ram, path, start, size / 4)?;
                let length = image.length;
                boot_files.push(image);
                Some((start, start + length))
            } else {
                None
            };
            (RAM_BASE + (2 << 20), cmdline.as_str(), initrd)
        }
    };
    boot_files.push(load(
        &ram,
        Path::new("/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin"),
        RAM_BASE,
        2 << 20,
    )?);
    let dtb = RAM_BASE + size - (2 << 20);
    let mut seed = [0; 32];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut seed))
        .map_err(|e| format!("device-tree entropy: {e}"))?;
    let tree = fdt::generate(desc.vcpus, size, cmdline, initrd, Some(&seed));
    ram.write_slice(&tree, GuestAddress(dtb))
        .map_err(|e| e.to_string())?;
    let dynamic = dtb + 0x100000;
    let mut dynamic_bytes = Vec::new();
    for value in [0x4942_534fu64, 2, next_entry, 1, 0, 0] {
        dynamic_bytes.extend_from_slice(&value.to_le_bytes());
    }
    ram.write_slice(&dynamic_bytes, GuestAddress(dynamic))
        .map_err(|e| e.to_string())?;
    let boot_images = BootImages {
        files: boot_files,
        data: vec![(dtb, tree), (dynamic, dynamic_bytes)],
    };
    let mut memory = memory::PhysicalMemory::new(ram, size, bus, pci.clone());
    for (base, flash) in flashes {
        memory.map_flash(base, flash);
    }
    let mut cpus = Vec::new();
    for hart in 0..desc.vcpus {
        let mut cpu = Riscv64::new(memory.clone()).map_err(|e| e.to_string())?;
        cpu.set_trace_cache_limit(64_000_000 / desc.vcpus as usize)
            .map_err(|e| e.to_string())?;
        cpu::configure_memory(&mut cpu, size)?;
        cpu.specialize = true;
        cpu.interpreter.operation_limit = u64::MAX;
        cpu.use_external_clock().map_err(|e| e.to_string())?;
        let misa = cpu.read_csr(0x301).map_err(|e| e.to_string())?;
        cpu.write_csr(0x301, misa & !((1 << 1) | (1 << 7) | (1 << 21)))
            .map_err(|e| e.to_string())?;
        cpu.set_pc(RAM_BASE);
        cpu.set_gpr(10, hart as u64).map_err(|e| e.to_string())?;
        cpu.set_gpr(11, dtb).map_err(|e| e.to_string())?;
        cpu.set_gpr(12, dynamic).map_err(|e| e.to_string())?;
        cpu.interpreter
            .globals
            .insert("zmhartid".into(), Value::bits(64, hart));
        cpu.interpreter
            .globals
            .insert("zplat_clint_sizze".into(), Value::bits(64, 0u64));
        cpu.interpreter
            .globals
            .insert("zplat_have_sig".into(), Value::Bool(false));
        cpus.push(Mutex::new(cpu));
    }
    VirtVm::new(
        desc,
        cpus,
        memory,
        clint,
        plic,
        shutdown,
        nodes,
        disks,
        pci,
        gpu,
        xhci,
        rtc,
        boot_images,
    )
}

struct BootFile {
    file: File,
    address: u64,
    length: u64,
}

impl BootFile {
    fn restore(&self, ram: &GuestMemory) -> Result<(), String> {
        let mut buffer = [0; 65536];
        let mut offset = 0;
        while offset < self.length {
            let length = (self.length - offset).min(buffer.len() as u64) as usize;
            self.file
                .read_exact_at(&mut buffer[..length], offset)
                .map_err(|e| e.to_string())?;
            ram.write_slice(&buffer[..length], GuestAddress(self.address + offset))
                .map_err(|e| e.to_string())?;
            offset += length as u64;
        }
        Ok(())
    }
}

struct BootImages {
    files: Vec<BootFile>,
    data: Vec<(u64, Vec<u8>)>,
}

impl BootImages {
    fn restore(&self, ram: &GuestMemory) -> Result<(), String> {
        for file in &self.files {
            file.restore(ram)?;
        }
        for (address, data) in &self.data {
            ram.write_slice(data, GuestAddress(*address))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn load(ram: &GuestMemory, path: &Path, address: u64, max: u64) -> Result<BootFile, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    if length > max {
        return Err(format!("{} exceeds boot memory range", path.display()));
    }
    let image = BootFile {
        file,
        address,
        length,
    };
    image.restore(ram)?;
    Ok(image)
}

fn next_slot(pci: &Arc<Mutex<PciRoot>>) -> Result<u8, String> {
    let used = pci.lock().addresses();
    (1..32)
        .find(|slot| !used.contains(&(*slot, 0)))
        .ok_or_else(|| "virt PCI slots exhausted".into())
}

fn placement(
    pci: &Arc<Mutex<PciRoot>>,
    address: Option<vmlab_hv_proto::machine::PciAddress>,
) -> Result<vmlab_hv_proto::machine::PciAddress, String> {
    match address {
        Some(address) => Ok(address),
        None => Ok(vmlab_hv_proto::machine::PciAddress {
            bus: 0,
            device: next_slot(pci)?,
            function: 0,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_boot_images_preserve_unrelated_ram() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let image = vec![0x5a; 70001];
        std::fs::write(file.path(), &image).unwrap();
        let ram = GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), 128 << 10)]).unwrap();
        let boot = BootImages {
            files: vec![load(&ram, file.path(), RAM_BASE, 80000).unwrap()],
            data: vec![(RAM_BASE + 90000, vec![1, 2, 3])],
        };
        ram.write_slice(&[0; 3], GuestAddress(RAM_BASE)).unwrap();
        ram.write_slice(&[0], GuestAddress(RAM_BASE + 70000))
            .unwrap();
        ram.write_slice(&[9], GuestAddress(RAM_BASE + 100000))
            .unwrap();
        boot.restore(&ram).unwrap();
        let mut restored = vec![0; image.len()];
        ram.read_slice(&mut restored, GuestAddress(RAM_BASE))
            .unwrap();
        assert_eq!(restored, image);
        let mut tree = [0; 3];
        ram.read_slice(&mut tree, GuestAddress(RAM_BASE + 90000))
            .unwrap();
        assert_eq!(tree, [1, 2, 3]);
        assert_eq!(
            ram.read_obj::<u8>(GuestAddress(RAM_BASE + 100000)).unwrap(),
            9
        );
    }

    #[test]
    fn reset_signal_keeps_the_first_request_until_serviced() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let signal = Arc::new(AtomicU32::new(0));
        let mut device = TestDevice(signal.clone());
        device.write(0, &0x7777u32.to_le_bytes());
        device.write(0, &0x5555u32.to_le_bytes());
        assert_eq!(signal.load(Ordering::SeqCst), 0x7777);
        signal.store(u32::MAX, Ordering::SeqCst);
        device.write(0, &0x3333u32.to_le_bytes());
        assert_eq!(signal.load(Ordering::SeqCst), u32::MAX);
    }

    #[test]
    fn host_bridge_matches_captured_configuration_header() {
        let mut bridge = HostBridge;
        let mut expected = [0; 16];
        expected[0] = 0x0008_1b36;
        expected[2] = 0x0600_0000;
        expected[11] = 0x1100_1af4;
        expected[15] = 0xff;
        for (reg, value) in expected.into_iter().enumerate() {
            assert_eq!(bridge.config_read(reg), value, "configuration dword {reg}");
        }
        bridge.config_write(4, 0, &u32::MAX.to_le_bytes());
        assert_eq!(bridge.config_read(4), 0);
        assert!(bridge.bars().is_empty());
        assert_eq!(bridge.config_read(64), u32::MAX);
    }
}

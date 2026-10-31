//! VFIO container, DMA, direct BAR mappings and KVM eventfd routing.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kvm_bindings::*;
use kvm_ioctls::VmFd;
use parking_lot::Mutex;
use vfio_bindings::bindings::vfio::*;
use vfio_ioctls::{VfioContainer, VfioDevice, VfioDeviceFd, VfioRegionInfoCap};
use vmlab_hv_devices::vfio::{Backend, InterruptMode, VfioPci, mapping_plan};
use vmlab_hv_devices::{BarInfo, BarKind, GuestMemory, MsiMessage};
use vmlab_hv_proto::machine::MachineDescription;
use vmm_sys_util::eventfd::EventFd;

pub(crate) type AssignedDevice = VfioPci<Host>;

pub(crate) fn validate_address(address: &str) -> Result<(), String> {
    let bytes = address.as_bytes();
    if bytes.len() != 12
        || bytes[4] != b':'
        || bytes[7] != b':'
        || bytes[10] != b'.'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7 | 10) || b.is_ascii_hexdigit())
        || u8::from_str_radix(&address[8..10], 16).map_or(true, |d| d >= 32)
        || bytes[11] > b'7'
        || !bytes[11].is_ascii_digit()
        || address != address.to_ascii_lowercase()
    {
        return Err(format!(
            "invalid VFIO host PCI address {address:?}; expected dddd:bb:dd.f"
        ));
    }
    Ok(())
}

fn preflight(root: &Path, address: &str) -> Result<PathBuf, String> {
    validate_address(address)?;
    let path = root.join(address);
    let driver = std::fs::read_link(path.join("driver")).map_err(|_| format!("VFIO {address} is not bound to vfio-pci; bind the designated device before starting vmlab-hv"))?;
    if driver.file_name().is_none_or(|name| name != "vfio-pci") {
        return Err(format!(
            "VFIO {address} is bound to {}, not vfio-pci; vmlab-hv never changes driver bindings",
            driver.display()
        ));
    }
    if !path.join("iommu_group").exists() {
        return Err(format!("VFIO {address} has no IOMMU group"));
    }
    Ok(path)
}

pub(crate) fn preflight_machine(desc: &MachineDescription) -> Result<(), String> {
    for device in &desc.vfio {
        preflight(Path::new("/sys/bus/pci/devices"), &device.host)?;
    }
    Ok(())
}

struct Routes {
    vm: Arc<VmFd>,
    msi: BTreeMap<u32, MsiMessage>,
    allocated: BTreeSet<u32>,
}

fn chip(gsi: u32, irqchip: u32, pin: u32) -> kvm_irq_routing_entry {
    kvm_irq_routing_entry {
        gsi,
        type_: KVM_IRQ_ROUTING_IRQCHIP,
        u: kvm_irq_routing_entry__bindgen_ty_1 {
            irqchip: kvm_irq_routing_irqchip { irqchip, pin },
        },
        ..Default::default()
    }
}

impl Routes {
    fn entries(&self) -> Vec<kvm_irq_routing_entry> {
        let mut entries = Vec::new();
        for gsi in 0..24 {
            if gsi < 16 && gsi != 2 {
                entries.push(chip(
                    gsi,
                    if gsi < 8 {
                        KVM_IRQCHIP_PIC_MASTER
                    } else {
                        KVM_IRQCHIP_PIC_SLAVE
                    },
                    gsi % 8,
                ));
            }
            entries.push(chip(
                gsi,
                KVM_IRQCHIP_IOAPIC,
                match gsi {
                    0 => 2,
                    2 => 0,
                    _ => gsi,
                },
            ));
        }
        for (&gsi, msg) in &self.msi {
            entries.push(kvm_irq_routing_entry {
                gsi,
                type_: KVM_IRQ_ROUTING_MSI,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    msi: kvm_irq_routing_msi {
                        address_lo: msg.address as u32,
                        address_hi: (msg.address >> 32) as u32,
                        data: msg.data,
                        ..Default::default()
                    },
                },
                ..Default::default()
            });
        }
        entries
    }

    fn commit(&mut self) -> Result<(), String> {
        let entries = self.entries();
        let mut table = KvmIrqRouting::new(entries.len()).map_err(|e| e.to_string())?;
        table.as_mut_slice().copy_from_slice(&entries);
        self.vm
            .set_gsi_routing(&table)
            .map_err(|e| format!("VFIO KVM IRQ routing: {e}"))
    }
}

struct Mapping {
    bar: u8,
    offset: u64,
    size: u64,
    host: usize,
    slot: u32,
    base: Option<u64>,
}

struct Domain {
    container: Arc<VfioContainer>,
    memory: GuestMemory,
    mapped: Mutex<bool>,
}

impl Domain {
    fn map(&self) -> Result<(), String> {
        let mut mapped = self.mapped.lock();
        if !*mapped {
            // SAFETY: Domain retains RAM until every device has closed and DMA
            // mappings have been removed in Drop.
            unsafe { self.container.vfio_map_guest_memory(&self.memory) }
                .map_err(|e| format!("VFIO guest DMA map: {e}"))?;
            *mapped = true;
        }
        Ok(())
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        if *self.mapped.lock() {
            let _ = self.container.vfio_unmap_guest_memory(&self.memory);
        }
    }
}

pub(crate) struct Host {
    device: VfioDevice,
    domain: Arc<Domain>,
    vm: Arc<VmFd>,
    slots: Arc<Mutex<BTreeSet<u32>>>,
    routes: Arc<Mutex<Routes>>,
    mappings: Vec<Mapping>,
    mapped_bars: BTreeSet<u8>,
    mode: InterruptMode,
    events: Vec<EventFd>,
    resample: EventFd,
    gsis: Vec<u32>,
    registered: Vec<bool>,
    messages: Vec<Option<MsiMessage>>,
    pirq: Option<u8>,
}

impl Host {
    fn register_mapping(&self, mapping: &Mapping, base: Option<u64>) -> Result<(), String> {
        for space in [0, 1 << 16] {
            let region = kvm_userspace_memory_region {
                slot: space | mapping.slot,
                guest_phys_addr: base.map_or(0, |b| b + mapping.offset),
                memory_size: base.map_or(0, |_| mapping.size),
                userspace_addr: mapping.host as u64,
                flags: 0,
            };
            // SAFETY: mapping remains alive until both KVM registrations are removed.
            unsafe { self.vm.set_user_memory_region(region) }
                .map_err(|e| format!("VFIO BAR KVM mapping: {e}"))?;
        }
        Ok(())
    }

    fn stop_interrupts(&mut self) -> Result<(), String> {
        if self.mode != InterruptMode::Disabled {
            self.device
                .disable_irq(irq_index(self.mode))
                .map_err(|e| e.to_string())?;
        }
        for (i, &registered) in self.registered.iter().enumerate() {
            if registered {
                self.vm
                    .unregister_irqfd(&self.events[i], self.gsis[i])
                    .map_err(|e| e.to_string())?;
            }
        }
        let mut routes = self.routes.lock();
        for gsi in &self.gsis {
            routes.msi.remove(gsi);
            routes.allocated.remove(gsi);
        }
        routes.commit()?;
        self.events.clear();
        self.registered.clear();
        self.gsis.clear();
        self.mode = InterruptMode::Disabled;
        self.messages.clear();
        Ok(())
    }
}

fn irq_index(mode: InterruptMode) -> u32 {
    match mode {
        InterruptMode::Intx => VFIO_PCI_INTX_IRQ_INDEX,
        InterruptMode::Msi => VFIO_PCI_MSI_IRQ_INDEX,
        InterruptMode::Msix => VFIO_PCI_MSIX_IRQ_INDEX,
        InterruptMode::Disabled => unreachable!("disabled mode has no VFIO IRQ index"),
    }
}

impl Backend for Host {
    fn read(&self, region: u8, offset: u64, data: &mut [u8]) {
        self.device.region_read(u32::from(region), data, offset);
    }
    fn write(&self, region: u8, offset: u64, data: &[u8]) {
        self.device.region_write(u32::from(region), data, offset);
    }

    fn reset(&mut self) -> Result<(), String> {
        self.stop_interrupts()?;
        // VFIO_DEVICE_RESET is _IO(';', 100 + 11), without a pointer argument.
        // SAFETY: the owned fd is a VFIO device, exclusively assigned by its
        // explicit machine-description address. The kernel checks reset scope.
        let result = unsafe {
            libc::ioctl(
                self.device.as_raw_fd(),
                (u64::from(VFIO_TYPE) << 8 | u64::from(VFIO_BASE + 11)) as libc::c_ulong,
            )
        };
        if result < 0 {
            Err(format!(
                "VFIO device reset failed: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    }

    fn map(&mut self, bar: u8, base: Option<u64>, traps: &[(u64, u64)]) -> Result<(), String> {
        if !self.mapped_bars.contains(&bar) {
            let size = self.device.get_region_size(u32::from(bar));
            let flags = self.device.get_region_flags(u32::from(bar));
            if flags & VFIO_REGION_INFO_FLAG_MMAP != 0 {
                let mut areas = vec![(0, size & !4095)];
                for cap in self.device.get_region_caps(u32::from(bar)) {
                    if let VfioRegionInfoCap::SparseMmap(sparse) = cap {
                        areas = sparse.areas.iter().map(|a| (a.offset, a.size)).collect();
                    }
                }
                for (offset, length) in mapping_plan(size, &areas, traps, 4096)? {
                    let slot = {
                        let mut slots = self.slots.lock();
                        let slot = (64..128)
                            .find(|s| !slots.contains(s))
                            .ok_or("VFIO KVM memory slots exhausted")?;
                        slots.insert(slot);
                        slot
                    };
                    let file_offset = self.device.get_region_offset(u32::from(bar)) + offset;
                    // SAFETY: VFIO advertises this page-aligned mmap area. The fd
                    // and mapping remain owned by Host through KVM unregistration.
                    let host = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            length as usize,
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_SHARED,
                            self.device.as_raw_fd(),
                            file_offset as libc::off_t,
                        )
                    };
                    if host == libc::MAP_FAILED {
                        self.slots.lock().remove(&slot);
                        return Err(format!(
                            "VFIO BAR mmap: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                    self.mappings.push(Mapping {
                        bar,
                        offset,
                        size: length,
                        host: host as usize,
                        slot,
                        base: None,
                    });
                }
            }
            self.mapped_bars.insert(bar);
        }
        for i in 0..self.mappings.len() {
            if self.mappings[i].bar != bar || self.mappings[i].base == base {
                continue;
            }
            let m = &self.mappings[i];
            if let Some(base) = base {
                let end = base
                    .checked_add(self.device.get_region_size(u32::from(bar)))
                    .ok_or("VFIO BAR overflow")?;
                use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};
                if base < 0x80000000
                    || end > 0xfec00000 && base < 1 << 32
                    || self
                        .domain
                        .memory
                        .iter()
                        .any(|r| base <= r.last_addr().0 && r.start_addr().0 < end)
                {
                    return Err("VFIO BAR overlaps RAM or platform MMIO".into());
                }
            }
            self.register_mapping(m, None)?;
            if let Err(error) = self.register_mapping(m, base) {
                let _ = self.register_mapping(m, None);
                return Err(error);
            }
            self.mappings[i].base = base;
        }
        Ok(())
    }

    fn pending(&self, vector: usize) -> bool {
        self.events.get(vector).is_some_and(|event| {
            let mut fd = libc::pollfd {
                fd: event.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd points to one valid pollfd, and timeout zero cannot block.
            unsafe { libc::poll(&mut fd, 1, 0) > 0 && fd.revents & libc::POLLIN != 0 }
        })
    }

    fn interrupts(
        &mut self,
        mode: InterruptMode,
        messages: &[Option<MsiMessage>],
    ) -> Result<(), String> {
        if self.mode == mode && self.messages == messages {
            return Ok(());
        }
        if self.mode != mode {
            self.stop_interrupts()?;
            if mode == InterruptMode::Disabled {
                return Ok(());
            }
            let count = if mode == InterruptMode::Intx {
                1
            } else {
                messages.len()
            };
            self.events = (0..count)
                .map(|_| EventFd::new(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            self.registered = vec![false; count];
            {
                let mut routes = self.routes.lock();
                for _ in 0..count {
                    let gsi = if mode == InterruptMode::Intx {
                        16 + u32::from(self.pirq.ok_or("missing VFIO INTx pin")?)
                    } else {
                        let gsi = (32..4096)
                            .find(|gsi| !routes.allocated.contains(gsi))
                            .ok_or("VFIO KVM interrupt routes exhausted")?;
                        routes.allocated.insert(gsi);
                        gsi
                    };
                    self.gsis.push(gsi);
                }
            }
            self.mode = mode;
            self.device
                .enable_irq(irq_index(mode), self.events.iter().collect())
                .map_err(|e| e.to_string())?;
            if mode == InterruptMode::Intx {
                self.device
                    .set_irq_resample_fd(VFIO_PCI_INTX_IRQ_INDEX, vec![&self.resample])
                    .map_err(|e| e.to_string())?;
            }
        }
        if mode == InterruptMode::Disabled {
            return Ok(());
        }
        let mut routes = self.routes.lock();
        for (i, &gsi) in self.gsis.iter().enumerate() {
            if self.registered[i] {
                self.vm
                    .unregister_irqfd(&self.events[i], gsi)
                    .map_err(|e| e.to_string())?;
                self.registered[i] = false;
            }
            routes.msi.remove(&gsi);
            if mode != InterruptMode::Intx
                && let Some(Some(message)) = messages.get(i)
            {
                routes.msi.insert(gsi, *message);
            }
        }
        routes.commit()?;
        for (i, &gsi) in self.gsis.iter().enumerate() {
            if mode == InterruptMode::Intx {
                self.vm
                    .register_irqfd_with_resample(&self.events[i], &self.resample, gsi)
                    .map_err(|e| e.to_string())?;
            } else if messages.get(i).is_some_and(Option::is_some) {
                self.vm
                    .register_irqfd(&self.events[i], gsi)
                    .map_err(|e| e.to_string())?;
            } else {
                continue;
            }
            self.registered[i] = true;
        }
        if mode == InterruptMode::Intx {
            self.device
                .unmask_irq(VFIO_PCI_INTX_IRQ_INDEX)
                .map_err(|e| e.to_string())?;
        }
        self.messages = messages.to_vec();
        Ok(())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.device
            .region_write(VFIO_PCI_CONFIG_REGION_INDEX, &[0, 0], 4);
        let _ = self.stop_interrupts();
        self.device.reset();
        for mapping in &self.mappings {
            if self.register_mapping(mapping, None).is_ok() {
                // SAFETY: no KVM mapping remains and no Rust reference aliases the mmap.
                unsafe {
                    libc::munmap(mapping.host as *mut libc::c_void, mapping.size as usize);
                }
                self.slots.lock().remove(&mapping.slot);
            }
        }
        // Domain outlives all device fds and removes DMA mappings before RAM drops.
    }
}

pub(crate) fn build(
    desc: &MachineDescription,
    kvm: &crate::kvm::KvmVm,
    memory: &GuestMemory,
) -> Result<Vec<Arc<Mutex<AssignedDevice>>>, String> {
    if desc.vfio.is_empty() {
        return Ok(Vec::new());
    }
    let paths = desc
        .vfio
        .iter()
        .map(|d| preflight(Path::new("/sys/bus/pci/devices"), &d.host))
        .collect::<Result<Vec<_>, _>>()?;
    let fd = kvm
        .vm
        .create_device(&mut kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
    let handle = Arc::new(VfioDeviceFd::new_from_kvm(fd));
    let routes = Arc::new(Mutex::new(Routes {
        vm: kvm.vm.clone(),
        msi: BTreeMap::new(),
        allocated: BTreeSet::new(),
    }));
    let slots = Arc::new(Mutex::new(BTreeSet::new()));
    let domain = Arc::new(Domain {
        container: Arc::new(VfioContainer::new(Some(handle)).map_err(|e| e.to_string())?),
        memory: memory.clone(),
        mapped: Mutex::new(false),
    });
    let mut devices = Vec::new();
    for (description, path) in desc.vfio.iter().zip(paths) {
        let device = VfioDevice::new(&path, domain.container.clone(), false).map_err(|e| {
            format!(
                "VFIO {}: {e}; check IOMMU group viability and permissions",
                description.host
            )
        })?;
        let mut config = vec![0; device.get_region_size(VFIO_PCI_CONFIG_REGION_INDEX) as usize];
        device.region_read(VFIO_PCI_CONFIG_REGION_INDEX, &mut config, 0);
        if config.len() < 64 {
            return Err("VFIO config region is too small".into());
        }
        let mut bars = Vec::new();
        let mut index = 0;
        while index < 6 {
            let value =
                u32::from_le_bytes(config[16 + index * 4..20 + index * 4].try_into().unwrap());
            let size = device.get_region_size(index as u32);
            if size != 0 {
                let kind = if value & 1 != 0 {
                    BarKind::Io
                } else if value & 6 == 4 {
                    BarKind::Mmio64 {
                        prefetchable: value & 8 != 0,
                    }
                } else {
                    BarKind::Mmio32
                };
                bars.push(BarInfo {
                    index: index as u8,
                    size,
                    kind,
                });
            }
            index += if value & 1 == 0 && value & 6 == 4 {
                2
            } else {
                1
            };
        }
        let rom = device.get_region_size(VFIO_PCI_ROM_REGION_INDEX);
        if rom != 0 {
            bars.push(BarInfo {
                index: 6,
                size: rom,
                kind: BarKind::Mmio32,
            });
        }
        let pin = config[0x3d];
        let host = Host {
            device,
            domain: domain.clone(),
            vm: kvm.vm.clone(),
            slots: slots.clone(),
            routes: routes.clone(),
            mappings: Vec::new(),
            mapped_bars: BTreeSet::new(),
            mode: InterruptMode::Disabled,
            events: Vec::new(),
            resample: EventFd::new(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)
                .map_err(|e| e.to_string())?,
            gsis: Vec::new(),
            registered: Vec::new(),
            messages: Vec::new(),
            pirq: crate::q35::pirq(description.pci.device, pin),
        };
        devices.push(Arc::new(Mutex::new(VfioPci::new(host, bars, config)?)));
    }
    // Every assigned function is reset with bus mastering disabled before
    // publishing any guest RAM in their shared IOMMU domain.
    domain.map()?;
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_bar_sizing_and_relocation_reach_the_vfio_mapping_backend() {
        use crate::pci::PciRoot;
        use vmlab_hv_devices::PciDevice;
        type Maps = Arc<Mutex<Vec<Option<u64>>>>;
        struct Mock(Maps);
        impl Backend for Mock {
            fn read(&self, _: u8, _: u64, bytes: &mut [u8]) {
                bytes.fill(0);
            }
            fn write(&self, _: u8, _: u64, _: &[u8]) {}
            fn map(&mut self, _: u8, base: Option<u64>, _: &[(u64, u64)]) -> Result<(), String> {
                self.0.lock().push(base);
                Ok(())
            }
            fn interrupts(
                &mut self,
                _: InterruptMode,
                _: &[Option<MsiMessage>],
            ) -> Result<(), String> {
                Ok(())
            }
            fn pending(&self, _: usize) -> bool {
                false
            }
            fn reset(&mut self) -> Result<(), String> {
                Ok(())
            }
        }
        let maps = Arc::new(Mutex::new(Vec::new()));
        let mut config = vec![0; 256];
        config[..4].copy_from_slice(&0x1234_8086u32.to_le_bytes());
        config[0x10] = 4;
        let device = Arc::new(Mutex::new(
            VfioPci::new(
                Mock(maps.clone()),
                vec![BarInfo {
                    index: 0,
                    size: 0x8000,
                    kind: BarKind::Mmio64 {
                        prefetchable: false,
                    },
                }],
                config,
            )
            .unwrap(),
        ));
        let mut root = PciRoot::q35(1 << 32);
        root.add_device_at(6, 0, device.clone()).unwrap();
        root.config_write(0, 6, 0, 0x10, &u32::MAX.to_le_bytes());
        assert_eq!(root.config_read(0, 6, 0, 4), 0xffff8004);
        assert_eq!(*maps.lock().last().unwrap(), None);
        root.config_write(0, 6, 0, 0x10, &0u32.to_le_bytes());
        root.config_write(0, 6, 0, 0x14, &1u32.to_le_bytes());
        assert_eq!(device.lock().config_read(4), 4);
        assert_eq!(*maps.lock().last().unwrap(), None);
        root.config_write(0, 6, 0, 4, &2u16.to_le_bytes());
        assert_eq!(*maps.lock().last().unwrap(), Some(1 << 32));
        root.config_write(0, 6, 0, 0x14, &2u32.to_le_bytes());
        assert_eq!(*maps.lock().last().unwrap(), Some(2 << 32));
        root.config_write(0, 6, 0, 4, &0u16.to_le_bytes());
        assert_eq!(*maps.lock().last().unwrap(), None);
    }

    #[test]
    fn addresses_are_canonical_and_cannot_escape_sysfs() {
        assert!(validate_address("0000:65:00.0").is_ok());
        for address in [
            "65:00.0",
            "0000:65:20.0",
            "0000:65:00.8",
            "0000:AA:00.0",
            "../../../tmp",
            "0000:65:00.a",
        ] {
            assert!(validate_address(address).is_err(), "{address}");
        }
    }

    #[test]
    fn preflight_refuses_missing_driver_and_never_changes_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("0000:65:00.0");
        std::fs::create_dir(&device).unwrap();
        assert!(
            preflight(dir.path(), "0000:65:00.0")
                .unwrap_err()
                .contains("not bound to vfio-pci")
        );
        std::os::unix::fs::symlink("/sys/bus/pci/drivers/nvidia", device.join("driver")).unwrap();
        assert!(
            preflight(dir.path(), "0000:65:00.0")
                .unwrap_err()
                .contains("never changes driver bindings")
        );
        assert_eq!(
            std::fs::read_link(device.join("driver")).unwrap(),
            Path::new("/sys/bus/pci/drivers/nvidia")
        );
    }

    #[test]
    fn routing_preserves_the_q35_timer_and_all_isa_routes() {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("SKIP VFIO IRQ routes: /dev/kvm absent");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let mut routes = Routes {
            vm: kvm.vm.clone(),
            msi: BTreeMap::from([(
                32,
                MsiMessage {
                    address: 0xfee0_0000,
                    data: 0x40,
                },
            )]),
            allocated: BTreeSet::from([32]),
        };
        let entries = routes.entries();
        assert_eq!(entries.len(), 40);
        assert!(
            entries
                .iter()
                .any(|e| e.gsi == 32 && e.type_ == KVM_IRQ_ROUTING_MSI)
        );
        let mut pins = BTreeSet::new();
        for entry in &entries {
            if entry.type_ == KVM_IRQ_ROUTING_IRQCHIP {
                // SAFETY: type_ selects the irqchip union member.
                let target = unsafe { entry.u.irqchip };
                assert!(pins.insert((target.irqchip, target.pin)));
                if entry.gsi == 0 && target.irqchip == KVM_IRQCHIP_IOAPIC {
                    assert_eq!(target.pin, 2);
                }
            }
        }
        routes.commit().unwrap();
        let event = EventFd::new(libc::EFD_CLOEXEC).unwrap();
        kvm.vm.register_irqfd(&event, 32).unwrap();
        event.write(1).unwrap();
        kvm.vm.unregister_irqfd(&event, 32).unwrap();
    }
}

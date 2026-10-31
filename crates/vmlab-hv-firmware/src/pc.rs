//! i440FX/PIIX firmware inputs from the captured pc-i440fx-11.1 machine.
//!
//! Static AML comes from guest-installed tables, not implementation source.
//! CPU, PCI, serial, and address-resource packages follow the supplied topology.

use crate::acpi::{
    AcpiTables, RSDP_FILE, TABLES_FILE, append, checksum, integer, madt, method, package,
    processor, put, table,
};
use crate::loader::{TableLoader, Zone};
use crate::platform::{
    FirmwareOptions, RESERVATIONS, common_files, e820_entry, numeric_items_for_ram,
};

const DSDT: &[u8] = include_bytes!("../../../docs/hwref/pc-templeos/acpi/DSDT");
const FADT: &[u8] = include_bytes!("../../../docs/hwref/pc-templeos/acpi/FACP");
const FACS: &[u8] = include_bytes!("../../../docs/hwref/pc-templeos/acpi/FACS");
const HPET: &[u8] = include_bytes!("../../../docs/hwref/pc-templeos/acpi/HPET");
const WAET: &[u8] = include_bytes!("../../../docs/hwref/pc-templeos/acpi/WAET");
const PCI64_SIZE: u64 = 2 << 30;

/// One function on the pc root bus, including its firmware hotplug policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciDevice {
    pub device: u8,
    pub function: u8,
    /// Standard VGA exposes the captured `_S1D`, `_S2D`, and `_S3D` methods.
    pub vga: bool,
    pub hotplug: bool,
}

impl From<crate::acpi::PciDevice> for PciDevice {
    fn from(device: crate::acpi::PciDevice) -> Self {
        Self {
            hotplug: (device.device > 1 && device.function == 0 && !device.vga)
                || (device.device, device.function) == (1, 2),
            device: device.device,
            function: device.function,
            vga: device.vga,
        }
    }
}

/// Firmware topology. RAM ranges describe installed RAM before ISA reservations.
#[derive(Clone, Debug)]
pub struct Platform {
    pub vcpus: u16,
    pub max_cpus: u16,
    pub ram_regions: Vec<(u64, u64)>,
    pub pci_devices: Vec<PciDevice>,
    /// None places the captured 2-GiB PCI window at the next GiB above RAM.
    pub pci64_base: Option<u64>,
    /// COM1 is always present. A legacy agent adds COM2 for the serial log.
    pub com2: bool,
    /// Effective guest physical width. Legacy CPUs omit unreachable reservations.
    pub physical_address_bits: u8,
}

impl Default for Platform {
    fn default() -> Self {
        Self {
            vcpus: 2,
            max_cpus: 2,
            ram_regions: vec![(0, 512 << 20)],
            pci_devices: [(0, 0), (1, 0), (1, 1), (1, 3), (2, 0)]
                .into_iter()
                .map(|(device, function)| PciDevice {
                    device,
                    function,
                    vga: device == 2,
                    hotplug: false,
                })
                .collect(),
            pci64_base: None,
            com2: false,
            physical_address_bits: 48,
        }
    }
}

fn validate(p: &Platform) -> Result<u64, String> {
    if p.vcpus == 0 || p.vcpus > p.max_cpus || p.max_cpus > 255 {
        return Err("pc firmware requires 1 <= vcpus <= max_cpus <= 255".into());
    }
    if !(32..=52).contains(&p.physical_address_bits) {
        return Err("pc physical address width must be in 32..=52".into());
    }
    let mut occupied = std::collections::BTreeSet::new();
    for d in &p.pci_devices {
        if d.device > 31
            || d.function > 7
            || !occupied.insert((d.device, d.function))
            || (d.vga && d.hotplug)
            || (d.hotplug && d.function != 0 && (d.device, d.function) != (1, 2))
            || (d.device <= 1 && d.hotplug && (d.device, d.function) != (1, 2))
        {
            return Err(
                "invalid, duplicate, or inconsistently hotpluggable pc PCI function".into(),
            );
        }
    }
    for address in [(0, 0), (1, 0), (1, 1), (1, 3)] {
        if !occupied.contains(&address) {
            return Err("pc requires i440FX and PIIX ISA, IDE, and PM functions".into());
        }
    }
    let mut end = 0;
    let mut ram = 0u64;
    for (index, &(base, size)) in p.ram_regions.iter().enumerate() {
        let next = base.checked_add(size).ok_or("RAM range overflow")?;
        if size == 0
            || base < end
            || (index == 0 && (base != 0 || size < 1 << 20 || next > 0xfec0_0000))
            || (index > 0 && base < 1 << 32)
            || next > 0xfd_0000_0000
            || next > 1 << p.physical_address_bits
        {
            return Err(
                "pc RAM must be sorted, nonempty, and outside reserved address ranges".into(),
            );
        }
        end = next;
        ram = ram.checked_add(size).ok_or("RAM size overflow")?;
    }
    if ram == 0 {
        return Err("pc requires low RAM".into());
    }
    let pci64 = p
        .pci64_base
        .unwrap_or_else(|| end.max(1 << 32).div_ceil(1 << 30) * (1 << 30));
    if pci64 < end.max(1 << 32)
        || pci64 & ((1 << 30) - 1) != 0
        || pci64.checked_add(PCI64_SIZE).is_none()
    {
        return Err("64-bit PCI window must be GiB aligned above RAM without overflow".into());
    }
    Ok(ram)
}

/// Build the pc ACPI 1.0 table set and firmware relocation program.
pub fn generate(p: &Platform) -> Result<AcpiTables, String> {
    validate(p)?;
    let mut tables = Vec::new();
    let mut loader = TableLoader::default();
    loader.allocate(RSDP_FILE, 16, Zone::Fseg);
    loader.allocate(TABLES_FILE, 64, Zone::High);
    let facs = append(&mut tables, FACS);
    let dsdt = append(&mut tables, &dsdt(p));
    checksum(&mut tables, &mut loader, dsdt);
    let mut fadt = FADT.to_vec();
    put(&mut fadt, 36, facs as u64, 4);
    put(&mut fadt, 40, dsdt as u64, 4);
    let fadt = append(&mut tables, &fadt);
    for offset in [36, 40] {
        loader.add_pointer(TABLES_FILE, TABLES_FILE, (fadt + offset) as u32, 4);
    }
    checksum(&mut tables, &mut loader, fadt);
    let apic = append(&mut tables, &madt(p.vcpus, p.max_cpus));
    checksum(&mut tables, &mut loader, apic);
    let hpet = append(&mut tables, HPET);
    checksum(&mut tables, &mut loader, hpet);
    let waet = append(&mut tables, WAET);
    checksum(&mut tables, &mut loader, waet);
    let entries = [fadt, apic, hpet, waet];
    let mut rsdt = table(*b"RSDT", 1, entries.len() * 4);
    for (index, &entry) in entries.iter().enumerate() {
        put(&mut rsdt, 36 + 4 * index, entry as u64, 4);
    }
    let rsdt = append(&mut tables, &rsdt);
    for index in 0..entries.len() {
        loader.add_pointer(TABLES_FILE, TABLES_FILE, (rsdt + 36 + 4 * index) as u32, 4);
    }
    checksum(&mut tables, &mut loader, rsdt);
    let mut rsdp = vec![0; 20];
    rsdp[..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"BOCHS ");
    put(&mut rsdp, 16, rsdt as u64, 4);
    loader.add_pointer(RSDP_FILE, TABLES_FILE, 16, 4);
    loader.add_checksum(RSDP_FILE, 8, 0, 20);
    tables.resize(tables.len().max(131072), 0);
    Ok(AcpiTables {
        tables,
        rsdp,
        loader: loader.finish(),
    })
}

/// Numeric fw_cfg items, including RAM size, topology, and reserved e820 ranges.
pub fn numeric_items(
    p: &Platform,
    options: &FirmwareOptions,
) -> Result<Vec<(u16, Vec<u8>)>, String> {
    let ram = validate(p)?;
    let mut items = numeric_items_for_ram(p.vcpus, p.max_cpus, ram, options);
    let reservations = reservations(p);
    let mut bytes = (reservations.len() as u32).to_le_bytes().to_vec();
    for (base, size) in reservations {
        e820_entry(&mut bytes, base, size, 2);
    }
    items.iter_mut().find(|(key, _)| *key == 0x8003).unwrap().1 = bytes;
    Ok(items)
}

/// pc SMBIOS, boot order, memory map, and firmware policy files.
pub fn ancillary_files(
    p: &Platform,
    options: &FirmwareOptions,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    validate(p)?;
    let mut e820 = Vec::new();
    for (base, size) in reservations(p) {
        e820_entry(&mut e820, base, size, 2);
    }
    for &(base, size) in &p.ram_regions {
        e820_entry(&mut e820, base, size, 1);
    }
    let smbios = crate::smbios::generate_pc_fw_cfg(p, &options.smbios)?;
    let mut files = common_files(options, smbios, e820)?;
    // SeaBIOS looks up this legacy file even without a TPM. D3 omits vapic ROMs.
    files.push(("etc/tpm/log".into(), Vec::new()));
    Ok(files)
}

fn reservations(p: &Platform) -> Vec<(u64, u64)> {
    RESERVATIONS
        .into_iter()
        .filter(|(base, size)| base + size <= 1 << p.physical_address_bits)
        .collect()
}

fn dsdt(p: &Platform) -> Vec<u8> {
    let mut source = DSDT.to_vec();
    let low_end = p.ram_regions[0].1;
    put(&mut source, 0x11dc, low_end, 4);
    put(&mut source, 0x11e8, 0xfec0_0000 - low_end, 4);
    let high_end = p
        .ram_regions
        .iter()
        .map(|(base, size)| base + size)
        .max()
        .unwrap()
        .max(1 << 32);
    let pci64 = p
        .pci64_base
        .unwrap_or_else(|| high_end.div_ceil(1 << 30) * (1 << 30));
    put(&mut source, 0x11fa, pci64, 8);
    put(&mut source, 0x1202, pci64 + PCI64_SIZE - 1, 8);

    // Rebuild both While packages because the shortest integer encoding changes
    // the method length when max_cpus crosses 1.
    let mut inner = source[0xecd..0xed3].to_vec();
    inner.extend(integer(p.max_cpus as u8));
    inner.extend_from_slice(&source[0xed5..0xf8a]);
    let mut outer = source[0xebb..0xeca].to_vec();
    outer.extend(package(&[0xa2], &inner));
    outer.extend_from_slice(&source[0xf8a..0x1019]);
    let mut scan = source[0xe8b..0xeb8].to_vec();
    scan.extend(package(&[0xa2], &outer));
    scan.extend_from_slice(&source[0x1019..0x102e]);
    let mut cpus = source[0xd7c..0xd9f].to_vec();
    let mut notify = Vec::new();
    for id in 0..p.max_cpus {
        let mut condition = vec![0x93, 0x68];
        condition.extend(integer(id as u8));
        condition.push(0x86);
        condition.extend_from_slice(format!("C{id:03X}").as_bytes());
        condition.push(0x69);
        notify.extend(package(&[0xa0], &condition));
    }
    cpus.extend(method(b"CTFY", 2, &notify));
    cpus.extend_from_slice(&source[0xdbc..0xe83]);
    cpus.extend(method(b"CSCN", 8, &scan));
    cpus.extend_from_slice(&source[0x102e..0x10cc]);
    for id in 0..p.max_cpus {
        cpus.extend(processor(id as u8, id < p.vcpus));
    }
    let mut cpu_scope = b"_SB_".to_vec();
    cpu_scope.extend_from_slice(&source[0xcdb..0xd78]);
    cpu_scope.extend(package(&[0x5b, 0x82], &cpus));
    let mut result = source[..0xcd4].to_vec();
    result.extend(package(&[0x10], &cpu_scope));
    result.extend_from_slice(&source[0x114d..0x1313]);
    result.extend(pci_scope(p, &source));
    result.extend_from_slice(&source[0x2222..]);
    let len = result.len() as u32;
    result[4..8].copy_from_slice(&len.to_le_bytes());
    result[9] = 0;
    result
}

fn pci_name(device: u8, function: u8) -> Vec<u8> {
    format!("S{:02X}_", device * 8 + function).into_bytes()
}

fn address(device: u8, function: u8) -> Vec<u8> {
    let mut result = b"\x08_ADR".to_vec();
    result.extend(aml_integer(u32::from(device) << 16 | u32::from(function)));
    result
}

fn aml_integer(value: u32) -> Vec<u8> {
    if let Ok(value) = u8::try_from(value) {
        integer(value)
    } else if let Ok(value) = u16::try_from(value) {
        [vec![0x0b], value.to_le_bytes().to_vec()].concat()
    } else {
        [vec![0x0c], value.to_le_bytes().to_vec()].concat()
    }
}

fn hotplug(slot: u8, source: &[u8]) -> Vec<u8> {
    let mut body = b"\x08ASUN".to_vec();
    body.extend(integer(slot));
    body.extend_from_slice(&source[0x1590..0x15ba]);
    body.extend_from_slice(b"\x08_SUN");
    body.extend(integer(slot));
    body.extend_from_slice(&source[0x15c0..0x15d3]);
    body
}

fn pci_scope(p: &Platform, source: &[u8]) -> Vec<u8> {
    let mut pci = b"PCI0".to_vec();
    let mut devices = p.pci_devices.clone();
    devices.sort_by_key(|d| (d.device, d.function));
    for d in &devices {
        let mut node = pci_name(d.device, d.function);
        node.extend(address(d.device, d.function));
        if (d.device, d.function) == (1, 0) {
            node.extend_from_slice(&source[0x1341..0x1379]);
            if p.com2 {
                node.extend_from_slice(&source[0x1379..0x14f7]);
            } else {
                node.extend_from_slice(&source[0x13e4..0x14f7]);
                node.extend_from_slice(&source[0x1379..0x13ae]);
            }
            node.extend_from_slice(&source[0x14f7..0x151e]);
        }
        if d.vga {
            for name in [b"_S1D", b"_S2D", b"_S3D"] {
                node.extend(method(name, 0, &[0xa4, 0]));
            }
        }
        pci.extend(package(&[0x5b, 0x82], &node));
    }
    pci.extend_from_slice(b"\x08BSEL\x00");
    if devices
        .iter()
        .any(|d| (d.device, d.function, d.hotplug) == (1, 2, true))
    {
        let mut scope = pci_name(1, 2);
        scope.extend(hotplug(1, source));
        pci.extend(package(&[0x10], &scope));
    }
    let mut notify = Vec::new();
    for slot in 1..32 {
        let present = devices.iter().any(|d| d.device == slot);
        let enabled = if slot == 1 {
            devices.iter().any(|d| d.device == 1 && d.hotplug)
        } else {
            !present || devices.iter().any(|d| d.device == slot && d.hotplug)
        };
        if !enabled {
            continue;
        }
        if slot > 1 {
            let mut node = pci_name(slot, 0);
            if !present {
                node.extend(address(slot, 0));
            }
            node.extend(hotplug(slot, source));
            pci.extend(package(
                if present { &[0x10] } else { &[0x5b, 0x82] },
                &node,
            ));
        }
        let mut condition = vec![0x7b, 0x68];
        condition.extend(aml_integer(1 << slot));
        condition.extend_from_slice(&[0, 0x86]);
        condition.extend(pci_name(slot, 0));
        condition.push(0x69);
        notify.extend(package(&[0xa0], &condition));
    }
    pci.extend(method(b"DVNT", 2, &notify));
    let mut scope = b"\\_SB_".to_vec();
    scope.extend(package(&[0x10], &pci));
    package(&[0x10], &scope)
}

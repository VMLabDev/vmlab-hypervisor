//! q35 firmware ACPI data and relocation commands.
//!
//! Static AML is the guest-observed hardware-reference capture, not QEMU
//! implementation source. CPU/PCI packages and memory resources are rebuilt
//! from the platform description. Offsets below address this pinned capture.

use crate::loader::{TableLoader, Zone};

const DSDT: &[u8] = include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/acpi/DSDT");
const FADT: &[u8] = include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/acpi/FACP");
const FACS: &[u8] = include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/acpi/FACS");
const HPET: &[u8] = include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/acpi/HPET");
const WAET: &[u8] = include_bytes!("../../../docs/hwref/q35-seabios-linux-generic/guest/acpi/WAET");
const TPM_DSDT: &[u8] = include_bytes!("../../../docs/hwref/q35-ovmf-win11/guest/acpi/DSDT");
pub const TABLES_FILE: &str = "etc/acpi/tables";
pub const RSDP_FILE: &str = "etc/acpi/rsdp";

/// A function on the flat q35 PCI root bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciDevice {
    pub device: u8,
    pub function: u8,
    pub vga: bool,
}

/// Guest-visible topology; RAM ranges contain `(base, length)` in bytes.
#[derive(Clone, Debug)]
pub struct Platform {
    pub vcpus: u16,
    pub max_cpus: u16,
    pub ram_regions: Vec<(u64, u64)>,
    pub pci_devices: Vec<PciDevice>,
    pub ecam_base: u64,
    /// Firmware-selected 64-bit PCI window; None places it above RAM.
    pub pci64_base: Option<u64>,
    pub tpm: bool,
}

impl Default for Platform {
    fn default() -> Self {
        Self {
            vcpus: 2,
            max_cpus: 2,
            ram_regions: vec![(0, 2 << 30)],
            pci_devices: [
                (0, 0),
                (1, 0),
                (2, 0),
                (3, 0),
                (4, 0),
                (5, 0),
                (31, 0),
                (31, 2),
                (31, 3),
            ]
            .into_iter()
            .map(|(device, function)| PciDevice {
                device,
                function,
                vga: device == 4,
            })
            .collect(),
            ecam_base: 0xb000_0000,
            pci64_base: None,
            tpm: false,
        }
    }
}

/// Files installed by SeaBIOS or OVMF, with pointers initially file-relative.
pub struct AcpiTables {
    pub tables: Vec<u8>,
    pub rsdp: Vec<u8>,
    pub loader: Vec<u8>,
}

/// Build all phase-2a tables and a firmware relocation/checksum program.
pub fn generate(platform: &Platform) -> Result<AcpiTables, String> {
    validate(platform)?;
    let mut tables = Vec::new();
    let mut loader = TableLoader::default();
    loader.allocate(RSDP_FILE, 16, Zone::Fseg);
    loader.allocate(TABLES_FILE, 64, Zone::High);
    let facs = append(&mut tables, FACS);
    let dsdt = append(&mut tables, &dsdt(platform));
    checksum(&mut tables, &mut loader, dsdt);
    let mut fadt = FADT.to_vec();
    put(&mut fadt, 36, facs as u64, 4);
    put(&mut fadt, 40, dsdt as u64, 4);
    put(&mut fadt, 140, dsdt as u64, 8);
    let fadt = append(&mut tables, &fadt);
    for (offset, width) in [(36, 4), (40, 4), (140, 8)] {
        loader.add_pointer(TABLES_FILE, TABLES_FILE, (fadt + offset) as u32, width);
    }
    checksum(&mut tables, &mut loader, fadt);
    let madt = append(&mut tables, &madt(platform));
    checksum(&mut tables, &mut loader, madt);
    let hpet = append(&mut tables, HPET);
    checksum(&mut tables, &mut loader, hpet);
    let mut mcfg = table(*b"MCFG", 1, 24);
    put(&mut mcfg, 44, platform.ecam_base, 8);
    mcfg[55] = 255;
    let mcfg = append(&mut tables, &mcfg);
    checksum(&mut tables, &mut loader, mcfg);
    let waet = append(&mut tables, WAET);
    checksum(&mut tables, &mut loader, waet);
    let mut entries = vec![fadt, madt, hpet, mcfg, waet];
    if platform.tpm {
        let mut tpm = table(*b"TPM2", 4, 40);
        put(&mut tpm, 48, 6, 4);
        let tpm = append(&mut tables, &tpm);
        checksum(&mut tables, &mut loader, tpm);
        entries.push(tpm);
    }
    let mut rsdt = table(*b"RSDT", 1, entries.len() * 4);
    for (index, address) in entries.iter().enumerate() {
        put(&mut rsdt, 36 + index * 4, *address as u64, 4);
    }
    let rsdt = append(&mut tables, &rsdt);
    for index in 0..entries.len() {
        loader.add_pointer(TABLES_FILE, TABLES_FILE, (rsdt + 36 + index * 4) as u32, 4);
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

fn validate(p: &Platform) -> Result<(), String> {
    if p.vcpus == 0 || p.vcpus > p.max_cpus || p.max_cpus > 255 {
        return Err("q35 firmware requires 1 <= vcpus <= max_cpus <= 255".into());
    }
    if p.ecam_base & 0x0fff_ffff != 0 || !(0x9000_0000..=0xe000_0000).contains(&p.ecam_base) {
        return Err("ECAM must be 256-MiB aligned between 0x90000000 and 0xe0000000".into());
    }
    let mut occupied = std::collections::BTreeSet::new();
    for d in &p.pci_devices {
        if d.device > 31 || d.function > 7 || !occupied.insert((d.device, d.function)) {
            return Err("invalid or duplicate PCI function".into());
        }
    }
    if !occupied.contains(&(31, 0)) {
        return Err("q35 requires the ICH9 LPC function at 00:1f.0".into());
    }
    let mut previous_end = 0;
    for (index, &(base, size)) in p.ram_regions.iter().enumerate() {
        let end = base.checked_add(size).ok_or("RAM range overflow")?;
        if size == 0
            || base < previous_end
            || (index > 0 && base < 1 << 32)
            || (base < 1 << 32 && end >= p.ecam_base)
            || end > 0x00f0_0000_0000
        {
            return Err("RAM ranges must be sorted, nonempty and outside ECAM".into());
        }
        previous_end = end;
    }
    if p.pci64_base.is_some_and(|base| {
        base < previous_end.max(1 << 32)
            || base & ((1 << 30) - 1) != 0
            || base.checked_add(32 << 30).is_none()
    }) {
        return Err("64-bit PCI window must be GiB aligned above RAM without overflow".into());
    }
    if p.ram_regions
        .first()
        .is_none_or(|r| r.0 != 0 || r.1 < 1 << 20)
    {
        return Err("q35 requires at least 1 MiB of low RAM at zero".into());
    }
    Ok(())
}

fn append(blob: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let start = blob.len();
    blob.extend_from_slice(bytes);
    start
}

fn checksum(tables: &mut [u8], loader: &mut TableLoader, start: usize) {
    tables[start + 9] = 0;
    let length = u32::from_le_bytes(tables[start + 4..start + 8].try_into().unwrap());
    loader.add_checksum(TABLES_FILE, (start + 9) as u32, start as u32, length);
}

fn table(signature: [u8; 4], revision: u8, body_len: usize) -> Vec<u8> {
    let mut bytes = vec![0; 36 + body_len];
    bytes[..4].copy_from_slice(&signature);
    let length = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes[8] = revision;
    bytes[10..16].copy_from_slice(b"BOCHS ");
    bytes[16..24].copy_from_slice(b"BXPC    ");
    bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
    bytes[28..32].copy_from_slice(b"BXPC");
    bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
    bytes
}

fn madt(p: &Platform) -> Vec<u8> {
    let mut b = table(*b"APIC", 3, 8);
    put(&mut b, 36, 0xfee0_0000, 4);
    b[40] = 1;
    for id in 0..p.max_cpus {
        b.extend_from_slice(&[0, 8, id as u8, id as u8, u8::from(id < p.vcpus), 0, 0, 0]);
    }
    b.extend_from_slice(&[1, 12, 0, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0]);
    for irq in [0, 5, 9, 10, 11] {
        b.extend_from_slice(&[
            2,
            10,
            0,
            irq,
            if irq == 0 { 2 } else { irq },
            0,
            0,
            0,
            if irq == 0 { 0 } else { 13 },
            0,
        ]);
    }
    b.extend_from_slice(&[4, 6, 255, 0, 0, 1]);
    let length = b.len() as u32;
    b[4..8].copy_from_slice(&length.to_le_bytes());
    b
}

fn put(bytes: &mut [u8], offset: usize, value: u64, width: usize) {
    bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

/// Encode an AML package including the length field itself (ACPI 6.5 §20.2.4).
fn package(op: &[u8], body: &[u8]) -> Vec<u8> {
    let width = (1..=4)
        .find(|n| body.len() + n < (if *n == 1 { 64 } else { 1 << (4 + 8 * (n - 1)) }))
        .unwrap();
    let length = body.len() + width;
    let mut b = op.to_vec();
    if width == 1 {
        b.push(length as u8);
    } else {
        b.push((((width - 1) << 6) | (length & 15)) as u8);
        for i in 0..width - 1 {
            b.push((length >> (4 + i * 8)) as u8);
        }
    }
    b.extend_from_slice(body);
    b
}

fn integer(value: u8) -> Vec<u8> {
    if value <= 1 {
        vec![value]
    } else {
        vec![0x0a, value]
    }
}

fn method(name: &[u8; 4], flags: u8, body: &[u8]) -> Vec<u8> {
    let mut b = name.to_vec();
    b.push(flags);
    b.extend_from_slice(body);
    package(&[0x14], &b)
}

fn dsdt(p: &Platform) -> Vec<u8> {
    let mut source = DSDT.to_vec();
    // DRAC's DWordMemory descriptor: ECAM base/end.
    put(&mut source, 0x198, p.ecam_base, 4);
    put(&mut source, 0x19c, p.ecam_base + 0x0fff_ffff, 4);
    // PCI0._CRS's three memory windows, after VGA legacy memory.
    let low_end = p.ram_regions[0].1;
    for (offset, value) in [
        (0x1e2a, low_end),
        (0x1e2e, p.ecam_base - 1),
        (0x1e36, p.ecam_base - low_end),
        (0x1e44, p.ecam_base + 0x1000_0000),
        (0x1e50, 0xfec0_0000 - p.ecam_base - 0x1000_0000),
    ] {
        put(&mut source, offset, value, 4);
    }
    let high_end = p
        .ram_regions
        .iter()
        .map(|(base, size)| base + size)
        .max()
        .unwrap()
        .max(1 << 32);
    let high_base = p
        .pci64_base
        .unwrap_or_else(|| high_end.div_ceil(1 << 30) * (1 << 30));
    put(&mut source, 0x1e62, high_base, 8);
    put(&mut source, 0x1e6a, high_base + (32 << 30) - 1, 8);
    let mut cpus = source[0x19ca..0x19ed].to_vec();
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
    let mut control = source[0x1a0a..0x1d1a].to_vec();
    let position = control
        .windows(4)
        .position(|w| w == [0x95, 0x63, 0x0a, 2])
        .unwrap();
    control[position + 3] = p.max_cpus as u8;
    cpus.extend(control);
    for id in 0..p.max_cpus {
        let mut cpu = format!("C{id:03X}").into_bytes();
        cpu.extend_from_slice(&[id as u8, 0, 0, 0, 0, 0]);
        let mut status = vec![0xa4];
        status.extend_from_slice(b"CSTA");
        status.extend(integer(id as u8));
        cpu.extend(method(b"_STA", 8, &status));
        cpu.extend_from_slice(b"\x08_MAT");
        let mut mat = vec![
            0x0a,
            8,
            0,
            8,
            id as u8,
            id as u8,
            u8::from(id < p.vcpus),
            0,
            0,
            0,
        ];
        cpu.extend(package(&[0x11], &mat));
        if id > 0 {
            mat = b"CEJ0".to_vec();
            mat.extend(integer(id as u8));
            cpu.extend(method(b"_EJ0", 1, &mat));
        }
        let mut ost = b"COST".to_vec();
        ost.extend(integer(id as u8));
        ost.extend_from_slice(&[0x68, 0x69, 0x6a]);
        cpu.extend(method(b"_OST", 11, &ost));
        cpus.extend(package(&[0x5b, 0x83], &cpu));
    }
    let mut scope = b"_SB_".to_vec();
    scope.extend_from_slice(&source[0x1929..0x19c6]);
    scope.extend(package(&[0x5b, 0x82], &cpus));
    let mut b = source[..0x1922].to_vec();
    b.extend(package(&[0x10], &scope));
    b.extend_from_slice(&source[0x1d9b..0x1f7b]);
    let mut pci = b"PCI0".to_vec();
    let mut devices = p.pci_devices.clone();
    devices.sort_by_key(|d| (d.device, d.function));
    for d in devices {
        let mut node = format!("S{:02X}_", d.device * 8 + d.function).into_bytes();
        node.extend_from_slice(b"\x08_ADR");
        let adr = (u32::from(d.device) << 16) | u32::from(d.function);
        if adr == 0 {
            node.push(0);
        } else {
            node.push(0x0c);
            node.extend_from_slice(&adr.to_le_bytes());
        }
        if d.vga {
            for name in [b"_S1D", b"_S2D", b"_S3D"] {
                node.extend(method(name, 0, &[0xa4, 0]));
            }
        }
        if d.device == 31 && d.function == 0 {
            node.extend_from_slice(&source[0x205d..0x2198]);
            if p.tpm {
                node.extend(tpm_device());
            }
        }
        pci.extend(package(&[0x5b, 0x82], &node));
    }
    let mut scope = b"\\_SB_".to_vec();
    scope.extend(package(&[0x10], &pci));
    b.extend(package(&[0x10], &scope));
    b.extend_from_slice(&source[0x21ba..]);
    let length = b.len() as u32;
    b[4..8].copy_from_slice(&length.to_le_bytes());
    b[9] = 0;
    b
}

fn tpm_device() -> Vec<u8> {
    // Complete TIS and physical-presence namespace observed under OVMF.
    TPM_DSDT[0x2138..0x23a4].to_vec()
}

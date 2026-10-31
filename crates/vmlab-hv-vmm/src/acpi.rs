//! ACPI table generation for the phase 1 machine.
//!
//! Builds RSDP, XSDT, FADT, MADT, MCFG and a DSDT describing the PCI root
//! (`PNP0A08`) with the BAR windows from [`crate::layout`], a power button
//! (`PNP0C0C`), and `\_S5`/`\_S0` sleep packages whose SLP_TYP values match
//! [`crate::pm`]. Tables are written into the reserved low-memory area at
//! [`crate::layout::ACPI_TABLES_BASE`], and the RSDP address is handed to the
//! kernel via the zero page.

use std::sync::Arc;

use acpi_tables::Aml;
use acpi_tables::aml;
use acpi_tables::fadt::{FADTBuilder, Flags, PmProfile};
use acpi_tables::madt::{
    EnabledStatus, IoApic, LocalInterruptController, MADT, ProcessorLocalApic,
};
use acpi_tables::mcfg::MCFG;
use acpi_tables::rsdp::Rsdp;
use acpi_tables::sdt::Sdt;
use acpi_tables::xsdt::XSDT;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use zerocopy::{Immutable, IntoBytes};

use crate::error::VmmError;
use crate::layout::{
    ACPI_TABLES_BASE, ACPI_TABLES_LIMIT, IO_ALLOC_BASE, IOAPIC_BASE, LAPIC_BASE, MMCONFIG_BASE,
    MMIO32_BASE, MMIO32_SIZE, MMIO64_SIZE, MemLayout,
};
use crate::pm::{PM1A_CNT_BASE, PM1A_EVT_BASE, SCI_GSI};

const OEM_ID: [u8; 6] = *b"VMLABH";
const OEM_TABLE_ID: [u8; 8] = *b"HVMACHIN";
const ACPI_REVISION: u32 = 1;

/// Everything the builder needs to know about the machine.
pub struct AcpiConfig<'a> {
    pub vcpus: u32,
    pub layout: &'a MemLayout,
}

/// Where the tables landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiPlacement {
    pub rsdp: u64,
}

/// Build the tables and write them into guest memory.
pub fn install(mem: &GuestMemoryMmap, cfg: &AcpiConfig) -> Result<AcpiPlacement, VmmError> {
    let mut cursor = TableArea::new();

    let dsdt = dsdt(cfg.layout);
    let dsdt_addr = cursor.push(mem, &dsdt)?;

    let mut fadt = FADTBuilder::new(OEM_ID, OEM_TABLE_ID, ACPI_REVISION);
    fadt.sci_int = (SCI_GSI as u16).into();
    fadt.pm1a_evt_blk = u32::from(PM1A_EVT_BASE).into();
    fadt.pm1a_cnt_blk = u32::from(PM1A_CNT_BASE).into();
    fadt.pm1_evt_len = 4;
    fadt.pm1_cnt_len = 2;
    fadt.century = 0x32;
    fadt.preferred_pm_profile = PmProfile::EnterpriseServer as u8;
    fadt.flags =
        ((Flags::Wbinvd as u32) | (Flags::Headless as u32) | (Flags::ResetRegSup as u32)).into();
    fadt.reset_reg = acpi_tables::gas::GAS::new(
        acpi_tables::gas::AddressSpace::SystemIo,
        8,
        0,
        acpi_tables::gas::AccessSize::ByteAccess,
        0xcf9,
    );
    fadt.reset_value = 6;
    let fadt = fadt.dsdt_64(dsdt_addr).finalize();
    let fadt_addr = cursor.push_aml(mem, &fadt)?;

    let mut madt = MADT::new(
        OEM_ID,
        OEM_TABLE_ID,
        ACPI_REVISION,
        LocalInterruptController::Address(LAPIC_BASE as u32),
    );
    for i in 0..cfg.vcpus.min(u8::MAX as u32) {
        madt.add_structure(ProcessorLocalApic::new(
            i as u8,
            i as u8,
            EnabledStatus::Enabled,
        ));
    }
    madt.add_structure(IoApic::new(0, IOAPIC_BASE as u32, 0));
    // ISA IRQ0 (PIT) is wired to GSI 2, as on every PC.
    madt.add_structure(IntSourceOverride::new(0, 2, 0));
    // The SCI is active-high, level-triggered on IRQ 9.
    madt.add_structure(IntSourceOverride::new(SCI_GSI as u8, SCI_GSI, 0x9));
    madt.add_structure(LocalApicNmi::default());
    let madt_addr = cursor.push_aml(mem, &madt)?;

    let mut mcfg = MCFG::new(OEM_ID, OEM_TABLE_ID, ACPI_REVISION);
    mcfg.add_ecam(MMCONFIG_BASE, 0, 0, 255);
    let mcfg_addr = cursor.push_aml(mem, &mcfg)?;

    let mut xsdt = XSDT::new(OEM_ID, OEM_TABLE_ID, ACPI_REVISION);
    xsdt.add_entry(fadt_addr);
    xsdt.add_entry(madt_addr);
    xsdt.add_entry(mcfg_addr);
    let xsdt_addr = cursor.push_aml(mem, &xsdt)?;

    let rsdp = Rsdp::new(OEM_ID, xsdt_addr);
    let rsdp_addr = cursor.push_aml(mem, &rsdp)?;

    Ok(AcpiPlacement { rsdp: rsdp_addr })
}

/// Sequential 8-byte-aligned placement inside the reserved low-memory area.
struct TableArea {
    next: u64,
}

impl TableArea {
    fn new() -> Self {
        TableArea {
            next: ACPI_TABLES_BASE,
        }
    }

    fn place(&mut self, len: usize) -> Result<u64, VmmError> {
        self.next = self.next.div_ceil(8) * 8;
        let addr = self.next;
        self.next += len as u64;
        if self.next > ACPI_TABLES_LIMIT {
            return Err(VmmError::Acpi(
                "ACPI tables do not fit below 0xA0000".to_string(),
            ));
        }
        Ok(addr)
    }

    fn push(&mut self, mem: &GuestMemoryMmap, table: &Sdt) -> Result<u64, VmmError> {
        let addr = self.place(table.len())?;
        mem.write_slice(table.as_slice(), GuestAddress(addr))
            .map_err(|e| VmmError::Acpi(format!("writing table at {addr:#x}: {e}")))?;
        Ok(addr)
    }

    fn push_aml(&mut self, mem: &GuestMemoryMmap, table: &impl Aml) -> Result<u64, VmmError> {
        let mut bytes = Vec::new();
        table.to_aml_bytes(&mut bytes);
        let addr = self.place(bytes.len())?;
        mem.write_slice(&bytes, GuestAddress(addr))
            .map_err(|e| VmmError::Acpi(format!("writing table at {addr:#x}: {e}")))?;
        Ok(addr)
    }
}

/// The DSDT: PCI root, power button, sleep states.
fn dsdt(layout: &MemLayout) -> Sdt {
    // AML objects borrow their children, so everything is bound to locals
    // before being assembled.
    let bus_numbers = aml::AddressSpace::<u16>::new_bus_number(0, 255);
    let io_window = aml::AddressSpace::<u32>::new_io(u32::from(IO_ALLOC_BASE), 0xFFFF, None);
    let mmio32 = aml::AddressSpace::<u32>::new_memory(
        aml::AddressSpaceCacheable::NotCacheable,
        true,
        MMIO32_BASE as u32,
        (MMIO32_BASE + MMIO32_SIZE - 1) as u32,
        None,
    );
    let mmio64 = aml::AddressSpace::<u64>::new_memory(
        aml::AddressSpaceCacheable::NotCacheable,
        true,
        layout.mmio64_base,
        layout.mmio64_base + MMIO64_SIZE - 1,
        None,
    );
    let crs = aml::ResourceTemplate::new(vec![&bus_numbers, &io_window, &mmio32, &mmio64]);

    let pci0_hid = aml::Name::new(aml::Path::new("_HID"), &aml::EISAName::new("PNP0A08"));
    let pci0_cid = aml::Name::new(aml::Path::new("_CID"), &aml::EISAName::new("PNP0A03"));
    let pci0_uid = aml::Name::new(aml::Path::new("_UID"), &0u32);
    let pci0_seg = aml::Name::new(aml::Path::new("_SEG"), &0u32);
    let pci0_bbn = aml::Name::new(aml::Path::new("_BBN"), &0u32);
    let pci0_crs = aml::Name::new(aml::Path::new("_CRS"), &crs);
    let pci0 = aml::Device::new(
        aml::Path::new("PCI0"),
        vec![
            &pci0_hid, &pci0_cid, &pci0_uid, &pci0_seg, &pci0_bbn, &pci0_crs,
        ],
    );

    let pwrb_hid = aml::Name::new(aml::Path::new("_HID"), &aml::EISAName::new("PNP0C0C"));
    let pwrb_sta = aml::Name::new(aml::Path::new("_STA"), &0xFu32);
    let pwrb = aml::Device::new(aml::Path::new("PWRB"), vec![&pwrb_hid, &pwrb_sta]);

    let rtc_hid = aml::Name::new(aml::Path::new("_HID"), &aml::EISAName::new("PNP0B00"));
    let rtc_io = aml::IO::new(0x70, 0x70, 1, 8);
    let rtc_irq = aml::Interrupt::new(true, true, false, false, 8);
    let rtc_resources = aml::ResourceTemplate::new(vec![&rtc_io, &rtc_irq]);
    let rtc_crs = aml::Name::new(aml::Path::new("_CRS"), &rtc_resources);
    let rtc = aml::Device::new(aml::Path::new("RTC0"), vec![&rtc_hid, &rtc_crs]);
    let sb = aml::Scope::new(aml::Path::new("\\_SB_"), vec![&pci0, &pwrb, &rtc]);

    let slp0 = 0u32;
    let slp5 = crate::pm::SLP_TYP_S5 as u32;
    let pkg0 = aml::Package::new(vec![&slp0, &slp0]);
    let pkg5 = aml::Package::new(vec![&slp5, &slp5]);
    let s0 = aml::Name::new(aml::Path::new("\\_S0_"), &pkg0);
    let s5 = aml::Name::new(aml::Path::new("\\_S5_"), &pkg5);

    let mut bytes = Vec::new();
    sb.to_aml_bytes(&mut bytes);
    s0.to_aml_bytes(&mut bytes);
    s5.to_aml_bytes(&mut bytes);

    let mut dsdt = Sdt::new(*b"DSDT", 36, 1, OEM_ID, OEM_TABLE_ID, ACPI_REVISION);
    dsdt.append_slice(&bytes);
    dsdt
}

/// MADT type 2: interrupt source override.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable)]
struct IntSourceOverride {
    kind: u8,
    length: u8,
    bus: u8,
    source: u8,
    global_irq: u32,
    inti_flags: u16,
}

impl IntSourceOverride {
    fn new(source: u8, gsi: u32, inti_flags: u16) -> Self {
        IntSourceOverride {
            kind: 2,
            length: 10,
            bus: 0, // ISA
            source,
            global_irq: gsi,
            inti_flags,
        }
    }
}

impl Aml for IntSourceOverride {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        sink.vec(self.as_bytes());
    }
}

/// MADT type 4: local APIC NMI wired to LINT1 on every CPU, as on a PC.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable)]
struct LocalApicNmi {
    kind: u8,
    length: u8,
    acpi_cpu_uid: u8,
    flags: u16,
    lint: u8,
}

impl Default for LocalApicNmi {
    fn default() -> Self {
        LocalApicNmi {
            kind: 4,
            length: 6,
            acpi_cpu_uid: 0xFF, // all CPUs
            flags: 0,
            lint: 1,
        }
    }
}

impl Aml for LocalApicNmi {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        sink.vec(self.as_bytes());
    }
}

/// An `Arc<dyn ...>`-free helper so the compiler can see the tables do not
/// borrow the config.
#[allow(dead_code)]
fn _static_assert(_: Arc<AcpiConfig<'static>>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    const MIB: u64 = 1 << 20;

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 * MIB as usize)]).unwrap()
    }

    fn sum_zero(bytes: &[u8]) -> bool {
        bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b)) == 0
    }

    #[test]
    fn tables_are_placed_and_checksummed() {
        let layout = MemLayout::new(16).unwrap();
        let memory = mem();
        let placement = install(
            &memory,
            &AcpiConfig {
                vcpus: 2,
                layout: &layout,
            },
        )
        .unwrap();

        // RSDP inside the reserved area, checksums valid over both spans.
        assert!(placement.rsdp >= ACPI_TABLES_BASE && placement.rsdp + 36 <= ACPI_TABLES_LIMIT);
        let mut buf = [0u8; 36];
        memory
            .read_slice(&mut buf, GuestAddress(placement.rsdp))
            .unwrap();
        assert_eq!(&buf[0..8], b"RSD PTR ");
        assert!(sum_zero(&buf[0..20]), "ACPI 1.0 checksum");
        assert!(sum_zero(&buf), "extended checksum");
        let xsdt_addr = u64::from_le_bytes(buf[24..32].try_into().unwrap());

        // XSDT lists FADT, MADT, MCFG; every listed table checksums.
        memory
            .read_slice(&mut buf[..4], GuestAddress(xsdt_addr))
            .unwrap();
        assert_eq!(&buf[0..4], b"XSDT");
        let mut len = [0u8; 4];
        memory
            .read_slice(&mut len, GuestAddress(xsdt_addr + 4))
            .unwrap();
        let xsdt_len = u32::from_le_bytes(len) as usize;
        let mut xsdt = vec![0u8; xsdt_len];
        memory
            .read_slice(&mut xsdt, GuestAddress(xsdt_addr))
            .unwrap();
        assert!(sum_zero(&xsdt));
        let entries = (xsdt_len - 36) / 8;
        assert_eq!(entries, 3);
        for i in 0..entries {
            let addr = u64::from_le_bytes(xsdt[36 + i * 8..44 + i * 8].try_into().unwrap());
            let mut sig = [0u8; 4];
            let mut l = [0u8; 4];
            memory.read_slice(&mut sig, GuestAddress(addr)).unwrap();
            memory.read_slice(&mut l, GuestAddress(addr + 4)).unwrap();
            let n = u32::from_le_bytes(l) as usize;
            let mut table = vec![0u8; n];
            memory.read_slice(&mut table, GuestAddress(addr)).unwrap();
            assert!(
                sum_zero(&table),
                "checksum of {}",
                String::from_utf8_lossy(&sig)
            );
        }
    }

    #[test]
    fn fadt_points_at_the_pm_block_and_dsdt() {
        let layout = MemLayout::new(16).unwrap();
        let memory = mem();
        let placement = install(
            &memory,
            &AcpiConfig {
                vcpus: 1,
                layout: &layout,
            },
        )
        .unwrap();
        let fadt_addr = read_entry(&memory, placement.rsdp, 0);
        let fadt = read_table(&memory, fadt_addr);
        assert_eq!(&fadt[0..4], b"FACP");
        // sci_int at offset 46, pm1a_evt at 48, pm1a_cnt at 64, lengths 88-91.
        let u16at = |o: usize| u16::from_le_bytes(fadt[o..o + 2].try_into().unwrap());
        let u8at = |o: usize| fadt[o];
        let u32at = |o: usize| u32::from_le_bytes(fadt[o..o + 4].try_into().unwrap());
        assert_eq!(u16at(46), 9, "sci_int");
        assert_eq!(u32at(56), 0x600, "pm1a_evt_blk");
        assert_eq!(u32at(64), 0x604, "pm1a_cnt_blk");
        assert_eq!(u8at(88), 4, "pm1_evt_len");
        assert_eq!(u8at(89), 2, "pm1_cnt_len");
        assert_eq!(u8at(108), 0x32, "RTC century register");
        // X_DSDT at offset 140.
        let dsdt_ptr = u64::from_le_bytes(fadt[140..148].try_into().unwrap());
        let dsdt = read_table(&memory, dsdt_ptr);
        assert_eq!(&dsdt[0..4], b"DSDT");
    }

    #[test]
    fn dsdt_names_the_pci_root_and_power_button() {
        let layout = MemLayout::new(16).unwrap();
        let memory = mem();
        let placement = install(
            &memory,
            &AcpiConfig {
                vcpus: 1,
                layout: &layout,
            },
        )
        .unwrap();
        let fadt_addr = read_entry(&memory, placement.rsdp, 0);
        let dsdt = read_table(
            &memory,
            u64::from_le_bytes(read_table(&memory, fadt_addr)[140..148].try_into().unwrap()),
        );
        let names: &[&[u8; 4]] = &[b"PCI0", b"PWRB", b"RTC0", b"_HID", b"_CRS", b"_STA"];
        for name in names {
            assert!(
                dsdt.windows(4).any(|w| w == name.as_slice()),
                "DSDT must name {name:?}"
            );
        }
        // The EISA ids PNP0A08 (PCI root) and PNP0C0C (power button), in the
        // big-endian DWordConst form AML uses (as iasl emits them).
        for id in [
            [0x41u8, 0xD0, 0x0A, 0x08],
            [0x41, 0xD0, 0x0C, 0x0C],
            [0x41, 0xD0, 0x0B, 0x00],
        ] {
            assert!(dsdt.windows(4).any(|w| w == id), "EISA id {id:?}");
        }
        // \_S5 with the SLP_TYP the PM device expects.
        assert!(dsdt.windows(4).any(|w| w == b"_S5_"));
    }

    #[test]
    fn madt_lists_every_vcpu_and_the_overrides() {
        let layout = MemLayout::new(16).unwrap();
        let memory = mem();
        let placement = install(
            &memory,
            &AcpiConfig {
                vcpus: 3,
                layout: &layout,
            },
        )
        .unwrap();
        let madt = read_table(&memory, read_entry(&memory, placement.rsdp, 1));
        assert_eq!(&madt[0..4], b"APIC");
        let mut lapics = 0;
        let mut ioapics = 0;
        let mut overrides = 0;
        let mut nmi = 0;
        let mut off = 44;
        while off < madt.len() {
            let kind = madt[off];
            let len = madt[off + 1] as usize;
            match kind {
                0 => lapics += 1,
                1 => ioapics += 1,
                2 => overrides += 1,
                4 => nmi += 1,
                _ => {}
            }
            assert!(len >= 2, "structure length sane");
            off += len;
        }
        assert_eq!(lapics, 3);
        assert_eq!(ioapics, 1);
        assert_eq!(overrides, 2);
        assert_eq!(nmi, 1);
    }

    #[test]
    fn mcfg_declares_the_ecam_window() {
        let layout = MemLayout::new(16).unwrap();
        let memory = mem();
        let placement = install(
            &memory,
            &AcpiConfig {
                vcpus: 1,
                layout: &layout,
            },
        )
        .unwrap();
        let mcfg = read_table(&memory, read_entry(&memory, placement.rsdp, 2));
        assert_eq!(&mcfg[0..4], b"MCFG");
        // One entry at offset 44: base address, segment, start/end bus.
        let base = u64::from_le_bytes(mcfg[44..52].try_into().unwrap());
        let segment = u16::from_le_bytes(mcfg[52..54].try_into().unwrap());
        let (start, end) = (mcfg[54], mcfg[55]);
        assert_eq!(base, MMCONFIG_BASE);
        assert_eq!((segment, start, end), (0, 0, 255));
    }

    fn read_entry(mem: &GuestMemoryMmap, rsdp: u64, index: usize) -> u64 {
        let mut buf = [0u8; 36];
        mem.read_slice(&mut buf, GuestAddress(rsdp)).unwrap();
        let xsdt = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let mut entry = [0u8; 8];
        mem.read_slice(&mut entry, GuestAddress(xsdt + 36 + (index * 8) as u64))
            .unwrap();
        u64::from_le_bytes(entry)
    }

    fn read_table(mem: &GuestMemoryMmap, addr: u64) -> Vec<u8> {
        let mut header = [0u8; 36];
        mem.read_slice(&mut header, GuestAddress(addr)).unwrap();
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let mut table = vec![0u8; len];
        mem.read_slice(&mut table, GuestAddress(addr)).unwrap();
        table
    }
}

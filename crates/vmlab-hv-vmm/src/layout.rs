//! Guest physical memory layout for the phase 1 x86_64 machine.
//!
//! RAM is split around a PCI hole in `[3 GiB, 4 GiB)`: low RAM is capped at
//! 3 GiB and the remainder (if any) is mapped at 4 GiB. The hole carries the
//! 32-bit MMIO BAR window, the ECAM (MMCONFIG) window, the I/O APIC at
//! `0xFEC0_0000` and the local APIC at `0xFEE0_0000`. 64-bit MMIO BARs are
//! placed above the top of RAM.

/// Start of the low RAM region.
pub const RAM_BASE: u64 = 0;
/// Low RAM never crosses this; the PCI hole starts here.
pub const PCI_HOLE_START: u64 = 0xC000_0000;
/// End of the PCI hole (top of the 32-bit space).
pub const PCI_HOLE_END: u64 = 0x1_0000_0000;
/// Base of the I/O APIC (in-kernel, described to the guest by the MADT).
pub const IOAPIC_BASE: u64 = 0xFEC0_0000;
/// Base of the local APIC (in-kernel LAPIC).
pub const LAPIC_BASE: u64 = 0xFEE0_0000;
/// Window used for 32-bit MMIO BARs.
pub const MMIO32_BASE: u64 = 0xC000_0000;
/// Size of the 32-bit MMIO BAR window.
pub const MMIO32_SIZE: u64 = 0x2000_0000;
/// ECAM (MMCONFIG) window: 256 MiB covers buses 0-255.
pub const MMCONFIG_BASE: u64 = 0xE000_0000;
pub const MMCONFIG_SIZE: u64 = 0x1000_0000;
/// Fixed legacy I/O ranges; the BAR allocator starts above them.
pub const IO_ALLOC_BASE: u16 = 0x1000;
/// ACPI tables are placed in reserved low memory below the VGA hole.
pub const ACPI_TABLES_BASE: u64 = 0x9_E000;
pub const ACPI_TABLES_LIMIT: u64 = 0xA_0000;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// The RAM regions of one machine, split around the PCI hole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemLayout {
    /// `(base, size)` pairs, sorted, all page aligned.
    pub regions: Vec<(u64, u64)>,
    /// Total RAM in bytes.
    pub ram: u64,
    /// Base of the 64-bit MMIO BAR window (top of RAM, 2 MiB aligned).
    pub mmio64_base: u64,
}

impl MemLayout {
    /// Lay out `ram_mib` MiB of RAM around the PCI hole.
    pub fn new(ram_mib: u64) -> Result<Self, String> {
        let ram = ram_mib
            .checked_mul(MIB)
            .filter(|r| *r >= 4 * MIB)
            .ok_or_else(|| format!("memory_mib {ram_mib} is out of range (4..=1<<20 MiB)"))?;
        if ram > (1 << 40) {
            return Err(format!("memory_mib {ram_mib} exceeds the 1 TiB phase 1 limit"));
        }
        let low = ram.min(PCI_HOLE_START);
        let high = ram - low;
        let mut regions = vec![(RAM_BASE, low)];
        if high > 0 {
            regions.push((PCI_HOLE_END, high));
        }
        let mmio64_base = (PCI_HOLE_END + high).div_ceil(2 * MIB) * 2 * MIB;
        Ok(MemLayout {
            regions,
            ram,
            mmio64_base,
        })
    }

    /// Top of the highest RAM region (address, exclusive).
    pub fn ram_top(&self) -> u64 {
        self.regions
            .last()
            .map(|(base, size)| base + size)
            .unwrap_or(0)
    }

    /// Top of the low RAM region (below the hole), for initrd placement.
    pub fn low_ram_top(&self) -> u64 {
        self.regions[0].0 + self.regions[0].1
    }

    /// A view of the regions as `(base, size)` for memslot registration.
    pub fn region_slices(&self) -> &[(u64, u64)] {
        &self.regions
    }
}

/// Size of the 64-bit MMIO BAR window.
pub const MMIO64_SIZE: u64 = 1 * GIB;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_ram_is_one_region() {
        let l = MemLayout::new(256).unwrap();
        assert_eq!(l.regions, vec![(0, 256 * MIB)]);
        assert_eq!(l.ram_top(), 256 * MIB);
        assert_eq!(l.mmio64_base, 0x1_0000_0000);
    }

    #[test]
    fn ram_above_the_hole_is_split() {
        let l = MemLayout::new(4096).unwrap();
        assert_eq!(
            l.regions,
            vec![(0, 0xC000_0000), (0x1_0000_0000, 0x4000_0000)]
        );
        // 64-bit BARs start above high RAM, 2 MiB aligned.
        assert_eq!(l.mmio64_base, 0x1_4000_0000);
        assert_eq!(l.low_ram_top(), 0xC000_0000);
    }

    #[test]
    fn ram_exactly_at_the_hole_edge() {
        let l = MemLayout::new(3072).unwrap();
        assert_eq!(l.regions, vec![(0, 0xC000_0000)]);
        assert_eq!(l.mmio64_base, 0x1_0000_0000);
    }

    #[test]
    fn rejects_tiny_and_huge_ram() {
        assert!(MemLayout::new(0).is_err());
        assert!(MemLayout::new(1).is_err());
        assert!(MemLayout::new(1 << 21).is_err());
    }
}

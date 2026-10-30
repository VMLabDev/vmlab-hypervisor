//! The e820 memory map handed to the guest in the zero page.
//!
//! Types are the `struct boot_e820_entry` values of the Linux boot protocol.

pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;

/// One e820 entry: `(addr, size, type)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

/// Build the map for a layout with ACPI tables at
/// [`crate::layout::ACPI_TABLES_BASE`].
///
/// The map marks the legacy region `[0xA0000, 0x100000)` reserved, the ACPI
/// table area reserved, the PCI hole reserved, and every RAM region usable.
pub fn build_e820(layout: &crate::layout::MemLayout) -> Vec<E820Entry> {
    use crate::layout::{ACPI_TABLES_BASE, ACPI_TABLES_LIMIT, PCI_HOLE_END, PCI_HOLE_START};

    let mut map = Vec::with_capacity(6);
    map.push(E820Entry {
        addr: 0,
        size: ACPI_TABLES_BASE,
        kind: E820_RAM,
    });
    map.push(E820Entry {
        addr: ACPI_TABLES_BASE,
        size: ACPI_TABLES_LIMIT - ACPI_TABLES_BASE,
        kind: E820_RESERVED,
    });
    map.push(E820Entry {
        addr: ACPI_TABLES_LIMIT,
        size: 0x10_0000 - ACPI_TABLES_LIMIT,
        kind: E820_RESERVED,
    });

    let (low_base, low_size) = layout.regions[0];
    let low_top = low_base + low_size;
    // The first RAM region always covers the low megabytes; report what is
    // left of it above 1 MiB.
    if low_top > 0x10_0000 {
        map.push(E820Entry {
            addr: 0x10_0000,
            size: low_top - 0x10_0000,
            kind: E820_RAM,
        });
    }
    map.push(E820Entry {
        addr: low_top.max(PCI_HOLE_START),
        size: PCI_HOLE_END - low_top.max(PCI_HOLE_START),
        kind: E820_RESERVED,
    });
    if let Some((high_base, high_size)) = layout.regions.get(1).copied() {
        map.push(E820Entry {
            addr: high_base,
            size: high_size,
            kind: E820_RAM,
        });
    }
    map.retain(|e| e.size > 0);
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::MemLayout;

    #[test]
    fn covers_all_of_physical_space_in_order() {
        let layout = MemLayout::new(256).unwrap();
        let map = build_e820(&layout);
        assert!(
            map.windows(2).all(|w| w[0].addr + w[0].size <= w[1].addr),
            "entries must be sorted and non-overlapping: {map:?}"
        );
        // RAM below the hole is fully covered.
        assert!(
            map.iter()
                .filter(|e| e.kind == E820_RAM)
                .any(|e| e.addr == 0x10_0000)
        );
        assert_eq!(map.first().unwrap().addr, 0);
        // The hole is described as reserved through 4 GiB.
        let last = map.last().unwrap();
        assert_eq!((last.addr, last.size), (0xC000_0000, 0x4000_0000));
    }

    #[test]
    fn marks_the_hole_and_legacy_regions_reserved() {
        let layout = MemLayout::new(256).unwrap();
        let map = build_e820(&layout);
        let kind_at = |addr: u64| {
            map.iter()
                .find(|e| e.addr <= addr && addr < e.addr + e.size)
                .unwrap()
                .kind
        };
        assert_eq!(kind_at(0), E820_RAM);
        assert_eq!(kind_at(0x9_E001), E820_RESERVED, "ACPI tables");
        assert_eq!(kind_at(0xB_0000), E820_RESERVED, "VGA hole");
        assert_eq!(kind_at(0xC000_0000), E820_RESERVED, "PCI hole");
        assert_eq!(kind_at(0x1_0000_0000 - 1), E820_RESERVED, "top of hole");
    }

    #[test]
    fn split_ram_has_two_ram_entries() {
        let layout = MemLayout::new(4096).unwrap();
        let map = build_e820(&layout);
        let ram: Vec<_> = map
            .iter()
            .filter(|e| e.kind == E820_RAM)
            .map(|e| (e.addr, e.size))
            .collect();
        assert_eq!(
            ram,
            vec![
                (0, 0x9_E000),
                (0x10_0000, 0xC000_0000 - 0x10_0000),
                (0x1_0000_0000, 0x4000_0000)
            ]
        );
        // No RAM entry straddles the hole.
        assert!(
            map.iter()
                .filter(|e| e.kind == E820_RAM)
                .all(|e| e.addr >= 0x1_0000_0000 || e.addr + e.size <= 0xC000_0000)
        );
    }
}

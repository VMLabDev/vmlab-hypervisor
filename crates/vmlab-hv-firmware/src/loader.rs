//! SeaBIOS/OVMF romfile-loader records. All integer fields are little-endian.

/// Allocation pool understood by both firmware implementations.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Zone {
    High = 1,
    Fseg = 2,
}

/// Encoder for the 128-byte `etc/table-loader` command stream.
#[derive(Default)]
pub struct TableLoader(Vec<u8>);

impl TableLoader {
    fn record(&mut self, command: u32) -> &mut [u8] {
        let start = self.0.len();
        self.0.resize(start + 128, 0);
        self.0[start..start + 4].copy_from_slice(&command.to_le_bytes());
        &mut self.0[start + 4..]
    }

    /// Allocate and copy a firmware file before applying relocations.
    pub fn allocate(&mut self, file: &str, alignment: u32, zone: Zone) {
        assert!(alignment.is_power_of_two());
        let r = self.record(1);
        name(&mut r[..56], file);
        r[56..60].copy_from_slice(&alignment.to_le_bytes());
        r[60] = zone as u8;
    }

    /// Add the allocated source address to an existing destination pointer.
    pub fn add_pointer(&mut self, dest: &str, source: &str, offset: u32, size: u8) {
        assert!(matches!(size, 1 | 2 | 4 | 8));
        let r = self.record(2);
        name(&mut r[..56], dest);
        name(&mut r[56..112], source);
        r[112..116].copy_from_slice(&offset.to_le_bytes());
        r[116] = size;
    }

    /// Subtract the range's byte sum from its checksum byte after relocation.
    pub fn add_checksum(&mut self, file: &str, offset: u32, start: u32, length: u32) {
        let r = self.record(3);
        name(&mut r[..56], file);
        r[56..60].copy_from_slice(&offset.to_le_bytes());
        r[60..64].copy_from_slice(&start.to_le_bytes());
        r[64..68].copy_from_slice(&length.to_le_bytes());
    }

    /// DMA-write an installed address back into a writable fw_cfg file.
    pub fn write_pointer(
        &mut self,
        dest: &str,
        source: &str,
        dest_offset: u32,
        source_offset: u32,
        size: u8,
    ) {
        assert!(matches!(size, 1 | 2 | 4 | 8));
        let r = self.record(4);
        name(&mut r[..56], dest);
        name(&mut r[56..112], source);
        r[112..116].copy_from_slice(&dest_offset.to_le_bytes());
        r[116..120].copy_from_slice(&source_offset.to_le_bytes());
        r[120] = size;
    }

    /// Pad with ignorable command-zero records to a firmware page boundary.
    pub fn finish(mut self) -> Vec<u8> {
        self.0.resize(self.0.len().div_ceil(4096) * 4096, 0);
        self.0
    }
}

fn name(field: &mut [u8], value: &str) {
    assert!(value.len() < 56 && !value.as_bytes().contains(&0));
    field[..value.len()].copy_from_slice(value.as_bytes());
}

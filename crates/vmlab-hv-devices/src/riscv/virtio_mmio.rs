//! Captured legacy virtio-MMIO transports with no attached device.

use crate::BusDevice;

#[derive(Default)]
pub struct EmptyVirtioMmio;

impl BusDevice for EmptyVirtioMmio {
    fn read(&mut self, mut offset: u64, mut data: &mut [u8]) {
        while !data.is_empty() {
            let mut width = 4.min(data.len());
            while !width.is_power_of_two() || !offset.is_multiple_of(width as u64) {
                width -= 1;
            }
            let value: u32 = match offset {
                0 => 0x7472_6976,
                4 => 1,
                12 => 0x554d_4551,
                _ => 0,
            };
            data[..width].copy_from_slice(&value.to_le_bytes()[..width]);
            data = &mut data[width..];
            offset = offset.saturating_add(width as u64);
        }
    }

    fn write(&mut self, _offset: u64, _data: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_transport_matches_the_black_box_identification_and_ignores_writes() {
        let mut device = EmptyVirtioMmio;
        for (offset, expected) in [
            (0, 0x74726976u32),
            (4, 1),
            (8, 0),
            (12, 0x554d4551),
            (16, 0),
            (20, 0),
            (32, 0),
            (36, 0),
            (48, 0),
            (52, 0),
            (56, 0),
            (60, 0),
            (68, 0),
            (96, 0),
            (100, 0),
            (112, 0),
            (252, 0),
            (256, 0),
        ] {
            let mut bytes = [0xff; 4];
            device.read(offset, &mut bytes);
            assert_eq!(u32::from_le_bytes(bytes), expected);
            device.write(offset, &u32::MAX.to_le_bytes());
            device.read(offset, &mut bytes);
            assert_eq!(u32::from_le_bytes(bytes), expected);
        }
    }

    #[test]
    fn narrow_unaligned_and_split_reads_match_the_black_box_capture() {
        let mut device = EmptyVirtioMmio;
        for (width, offset, expected) in [
            (1, 0, 0x76u64),
            (1, 1, 0),
            (1, 4, 1),
            (1, 12, 0x51),
            (2, 0, 0x6976),
            (2, 1, 0),
            (2, 2, 0),
            (2, 12, 0x4551),
            (4, 1, 0x01000000),
            (4, 2, 0x00010000),
            (8, 0, 0x0000000174726976),
            (8, 1, 0x01000000),
            (8, 2, 0x00010000),
            (8, 4, 1),
            (8, 8, 0x554d455100000000),
            (8, 12, 0x554d4551),
        ] {
            let mut bytes = [0; 8];
            device.read(offset, &mut bytes[..width]);
            assert_eq!(
                u64::from_le_bytes(bytes),
                expected,
                "width={width}, offset={offset}"
            );
        }
    }
}

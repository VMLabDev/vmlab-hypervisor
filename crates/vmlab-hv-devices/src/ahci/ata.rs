use vmlab_disk::DiskImage;

use super::atapi::Optical;

pub(super) struct AtaDisk {
    pub image: Box<dyn DiskImage>,
    pub write_cache: bool,
    pub transfer_mode: u8,
}

pub(super) enum Drive {
    Disk(AtaDisk),
    Cdrom(Optical),
}

impl Drive {
    pub fn signature(&self) -> u32 {
        match self {
            Self::Disk(_) => 0x0000_0101,
            Self::Cdrom(_) => 0xeb14_0101,
        }
    }

    pub fn identify(&self) -> Vec<u8> {
        let mut data = vec![0; 512];
        let (packet, sectors) = match self {
            Self::Disk(disk) => (false, disk.image.size() / 512),
            Self::Cdrom(_) => (true, 0),
        };
        let mut word = |index: usize, value: u16| {
            data[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
        };
        word(0, if packet { 0x8580 } else { 0x0040 });
        word(1, 16383);
        word(3, 16);
        word(6, 63);
        word(47, 0x8001);
        word(49, 0x0300); // DMA and LBA.
        word(53, 7);
        word(54, 16383);
        word(55, 16);
        word(56, 63);
        word(60, sectors.min(0x0fff_ffff) as u16);
        word(61, (sectors.min(0x0fff_ffff) >> 16) as u16);
        let mode = match self {
            Self::Disk(disk) => disk.transfer_mode,
            _ => 0x22,
        };
        let cache = matches!(self, Self::Disk(disk) if disk.write_cache);
        word(
            63,
            7 | if mode & 0xf8 == 0x20 {
                1 << (8 + (mode & 7))
            } else {
                0
            },
        );
        word(64, 3);
        word(75, 0); // No NCQ.
        word(76, 2); // SATA Gen1.
        word(80, 0x007e);
        word(81, 0x0016);
        word(82, 1 << 5); // Write cache and FLUSH CACHE.
        word(83, 0x7400); // Valid, LBA48, FLUSH CACHE and FLUSH CACHE EXT.
        word(84, 0x4000);
        word(85, u16::from(cache) << 5);
        word(86, 0x3400);
        word(87, 0x4000);
        word(
            88,
            0x3f | if mode & 0xf8 == 0x40 {
                1 << (8 + (mode & 7))
            } else {
                0
            },
        );
        for i in 0..4 {
            word(100 + i, (sectors >> (16 * i)) as u16);
        }
        word(106, 0x4000);
        for (start, len, text) in [
            (10, 20, "VMLAB000000000000001"),
            (23, 8, "0.1"),
            (
                27,
                40,
                if packet {
                    "VMLAB SATA CD-ROM"
                } else {
                    "VMLAB SATA DISK"
                },
            ),
        ] {
            let mut bytes = vec![b' '; len];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            for pair in bytes.as_chunks_mut::<2>().0 {
                pair.swap(0, 1);
            }
            data[start * 2..start * 2 + len].copy_from_slice(&bytes);
        }
        data
    }
}

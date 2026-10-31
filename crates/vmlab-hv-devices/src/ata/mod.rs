use vmlab_disk::DiskImage;

pub(crate) mod atapi;

pub use atapi::MediaState;
use atapi::Optical;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Sata,
    Ide,
}

pub(crate) enum Payload {
    Bytes(Vec<u8>),
    Read { offset: u64, len: usize },
}

impl Payload {
    pub fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::Read { len, .. } => *len,
        }
    }
}

pub(crate) struct AtaDisk {
    pub image: Box<dyn DiskImage>,
    pub write_cache: bool,
    pub transfer_mode: u8,
    pub ncq_error: [u8; 24],
}

pub(crate) enum Drive {
    Disk(AtaDisk),
    Cdrom(Optical),
}

impl Drive {
    pub fn read(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), u8> {
        match self {
            Self::Disk(disk) => disk.image.read_at(offset, bytes).map_err(|_| 0x40),
            Self::Cdrom(optical) => {
                if optical
                    .image
                    .as_mut()
                    .ok_or(4)?
                    .read_at(offset, bytes)
                    .is_err()
                {
                    optical.sense = [3, 0x11, 0];
                    Err(0x34)
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn transfer(
        &mut self,
        offset: u64,
        len: usize,
        input: Option<&[u8]>,
        fua: bool,
    ) -> Result<Payload, u8> {
        let Self::Disk(disk) = self else {
            return Err(4);
        };
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > disk.image.size())
        {
            return Err(0x10);
        }
        if let Some(input) = input {
            if disk.image.read_only() || input.len() != len {
                return Err(4);
            }
            disk.image.write_at(offset, input).map_err(|_| 0x40)?;
            if !disk.write_cache || fua {
                disk.image.flush().map_err(|_| 0x40)?;
            }
            Ok(Payload::Bytes(Vec::new()))
        } else {
            Ok(Payload::Read { offset, len })
        }
    }

    pub fn flush(&mut self) -> Result<(), u8> {
        let Self::Disk(disk) = self else {
            return Err(4);
        };
        disk.image.flush().map_err(|_| 0x40)
    }

    pub fn set_features(&mut self, feature: u8, mode: u8) -> Result<(), u8> {
        if feature == 3 && matches!(mode, 0 | 1 | 0x08..=0x0c | 0x20..=0x22 | 0x40..=0x45) {
            match self {
                Self::Disk(disk) => disk.transfer_mode = mode,
                Self::Cdrom(optical) => optical.transfer_mode = mode,
            }
            return Ok(());
        }
        let Self::Disk(disk) = self else {
            return Err(4);
        };
        match feature {
            2 => disk.write_cache = true,
            0x82 => {
                disk.image.flush().map_err(|_| 0x40)?;
                disk.write_cache = false;
            }
            _ => return Err(4),
        }
        Ok(())
    }

    pub fn signature(&self) -> u32 {
        match self {
            Self::Disk(_) => 0x0000_0101,
            Self::Cdrom(_) => 0xeb14_0101,
        }
    }

    pub fn identify(&self, port: usize, transport: Transport) -> Vec<u8> {
        let mut data = vec![0; 512];
        let (packet, sectors) = match self {
            Self::Disk(disk) => (false, disk.image.size() / 512),
            Self::Cdrom(_) => (true, 0),
        };
        let mode = match self {
            Self::Disk(disk) => disk.transfer_mode,
            Self::Cdrom(optical) => optical.transfer_mode,
        };
        let cache = matches!(self, Self::Disk(disk) if disk.write_cache);
        let mut word = |index: usize, value: u16| {
            data[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
        };
        if packet {
            for (index, value) in [
                (0, 0x85c0),
                (20, 3),
                (21, 512),
                (22, 4),
                (48, 1),
                (49, 0x300),
                (53, 7),
                (62, 7),
                (64, 3),
                (65, 180),
                (66, 180),
                (67, 300),
                (68, 180),
                (71, 30),
                (72, 30),
                (75, 31),
                (76, 0x100),
                (80, 0x1e),
            ] {
                word(index, value);
            }
            word(
                63,
                7 | if mode & 0xf8 == 0x20 {
                    1 << (8 + (mode & 7))
                } else {
                    0
                },
            );
            word(
                88,
                0x3f | if mode & 0xf8 == 0x40 {
                    1 << (8 + (mode & 7))
                } else {
                    0
                },
            );
        } else {
            word(0, 0x0040);
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
            word(
                63,
                7 | if mode & 0xf8 == 0x20 {
                    1 << (8 + (mode & 7))
                } else {
                    0
                },
            );
            word(64, 3);
            word(75, if packet { 0 } else { 31 });
            word(76, 2 | if packet { 0 } else { 1 << 8 }); // SATA Gen1 and NCQ.
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
        }
        if transport == Transport::Ide {
            for index in [75, 76, 77, 78, 79] {
                data[index * 2..index * 2 + 2].fill(0);
            }
            if !packet {
                let cylinders = (sectors / (16 * 63)).clamp(1, 16383) as u16;
                for (index, value) in [(1, cylinders), (54, cylinders), (47, 0x8010)] {
                    data[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
                }
                let chs_sectors = u32::from(cylinders) * 16 * 63;
                data[114..118].copy_from_slice(&chs_sectors.to_le_bytes());
            }
        }
        let serial = format!(
            "QM{:05}",
            if transport == Transport::Sata {
                port * 2 + 1
            } else {
                port + 1
            }
        );
        for (start, len, text) in [
            (10, 20, serial.as_str()),
            (23, 8, "2.5+"),
            (
                27,
                40,
                if packet {
                    "QEMU DVD-ROM"
                } else {
                    "QEMU HARDDISK"
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

//! Read-only MMC CD-ROM commands, with SPC fixed-format sense data.

use vmlab_disk::DiskImage;

use super::Payload;

pub(super) struct Optical {
    pub image: Option<Box<dyn DiskImage>>,
    pub tray_open: bool,
    pub locked: bool,
    pub attention: bool,
    pub event: u8,
    pub sense: [u8; 3],
    pub generation: u64,
}

impl Optical {
    pub fn new(image: Option<Box<dyn DiskImage>>) -> Self {
        Self {
            image,
            tray_open: false,
            locked: false,
            attention: false,
            event: 0,
            sense: [0; 3],
            generation: 0,
        }
    }

    pub fn present(&self) -> bool {
        self.image.is_some() && !self.tray_open
    }

    pub fn changed(&mut self, event: u8) {
        self.attention = true;
        self.event = event;
    }

    fn fail<T>(&mut self, key: u8, asc: u8, ascq: u8) -> Result<T, u8> {
        self.sense = [key, asc, ascq];
        Err((key << 4) | 4)
    }

    fn ready(&mut self) -> Result<(), u8> {
        if !self.present() {
            return self.fail(2, 0x3a, if self.tray_open { 2 } else { 1 });
        }
        Ok(())
    }

    pub fn packet(&mut self, cdb: &[u8; 16]) -> Result<Payload, u8> {
        if self.attention && !matches!(cdb[0], 0x03 | 0x12 | 0x4a) {
            self.attention = false;
            return self.fail(6, 0x28, 0);
        }
        let data = match cdb[0] {
            0x00 => {
                self.ready()?;
                Vec::new()
            }
            0x03 => {
                if self.attention && self.sense[0] == 0 {
                    self.attention = false;
                    self.sense = [6, 0x28, 0];
                }
                let mut data = vec![0; 18];
                data[0] = 0x70;
                data[2] = self.sense[0];
                data[7] = 10;
                data[12] = self.sense[1];
                data[13] = self.sense[2];
                self.sense = [0; 3];
                data.truncate(cdb[4] as usize);
                data
            }
            0x12 => {
                if cdb[1] & 3 != 0 || cdb[2] != 0 {
                    return self.fail(5, 0x24, 0);
                }
                let mut data = vec![0; 36];
                data[..8].copy_from_slice(&[5, 0x80, 5, 2, 31, 0, 0, 0]);
                data[8..16].copy_from_slice(b"VMLAB   ");
                data[16..32].copy_from_slice(b"SATA CD-ROM     ");
                data[32..36].copy_from_slice(b"0.1 ");
                data.truncate(u16::from_be_bytes([cdb[3], cdb[4]]) as usize);
                data
            }
            0x1b => {
                if cdb[4] & 2 != 0 {
                    let open = cdb[4] & 1 == 0;
                    if open && self.locked {
                        return self.fail(5, 0x53, 2);
                    }
                    if open != self.tray_open {
                        self.tray_open = open;
                        self.changed(if open { 3 } else { 2 });
                    }
                }
                Vec::new()
            }
            0x1e => {
                self.locked = cdb[4] & 1 != 0;
                Vec::new()
            }
            0x25 => {
                self.ready()?;
                let sectors = self.image.as_ref().unwrap().size() / 2048;
                let last = sectors.saturating_sub(1).min(u32::MAX as u64) as u32;
                [last.to_be_bytes(), 2048u32.to_be_bytes()].concat()
            }
            0x28 | 0xa8 => {
                self.ready()?;
                let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap()) as u64;
                let count = if cdb[0] == 0x28 {
                    u16::from_be_bytes([cdb[7], cdb[8]]) as u64
                } else {
                    u32::from_be_bytes(cdb[6..10].try_into().unwrap()) as u64
                };
                let image = self.image.as_ref().unwrap();
                if lba + count > image.size() / 2048 {
                    return self.fail(5, 0x21, 0);
                }
                return Ok(Payload::Read {
                    offset: lba * 2048,
                    len: count as usize * 2048,
                });
            }
            0x1a | 0x5a => {
                let ten = cdb[0] == 0x5a;
                let page = cdb[2] & 0x3f;
                if cdb[2] >> 6 == 3 {
                    return self.fail(5, 0x39, 0);
                }
                if !matches!(page, 0x01 | 0x2a | 0x3f) || cdb[3] != 0 {
                    return self.fail(5, 0x24, 0);
                }
                let mut data = vec![0; if ten { 8 } else { 4 }];
                data[if ten { 3 } else { 2 }] = 0x80;
                let changeable = cdb[2] >> 6 == 1;
                if page == 1 || page == 0x3f {
                    let mut recovery = vec![0; 8];
                    recovery[0] = 1;
                    recovery[1] = 6;
                    data.extend(recovery);
                }
                if page == 0x2a || page == 0x3f {
                    let mut capabilities = vec![0; 28];
                    capabilities[0] = 0x2a;
                    capabilities[1] = 26;
                    if !changeable {
                        capabilities[2] = 1; // CD-R read.
                        capabilities[6] = 0x29 | (u8::from(self.locked) << 1);
                        capabilities[8..10].copy_from_slice(&706u16.to_be_bytes());
                        capabilities[14..16].copy_from_slice(&706u16.to_be_bytes());
                    }
                    data.extend(capabilities);
                }
                if ten {
                    let len = (data.len() - 2) as u16;
                    data[..2].copy_from_slice(&len.to_be_bytes());
                } else {
                    data[0] = (data.len() - 1) as u8;
                }
                let alloc = if ten {
                    u16::from_be_bytes([cdb[7], cdb[8]]) as usize
                } else {
                    cdb[4] as usize
                };
                data.truncate(alloc);
                data
            }
            0x46 => {
                let request = cdb[1] & 3;
                if request == 3 {
                    return self.fail(5, 0x24, 0);
                }
                let start = u16::from_be_bytes([cdb[2], cdb[3]]);
                let present = self.present();
                let profile = if present { 8u16 } else { 0 };
                let mut data = vec![0; 8];
                data[6..8].copy_from_slice(&profile.to_be_bytes());
                // Profile list, core, removable medium, random readable, CD read.
                for (code, current, payload) in [
                    (0u16, true, vec![0, 8, u8::from(present), 0]),
                    (1, true, vec![0, 0, 0, 2, 0, 0, 0, 0]),
                    (3, true, vec![0x29, 0, 0, 0]),
                    (0x10, present, vec![0, 0, 8, 0, 0, 1, 0, 0]),
                    (0x1e, present, vec![0, 0, 0, 0]),
                ] {
                    if code < start || (request == 1 && !current) || (request == 2 && code != start)
                    {
                        continue;
                    }
                    data.extend(code.to_be_bytes());
                    data.extend([2 | u8::from(current), payload.len() as u8]);
                    data.extend(payload);
                }
                let len = (data.len() - 4) as u32;
                data[..4].copy_from_slice(&len.to_be_bytes());
                data.truncate(u16::from_be_bytes([cdb[7], cdb[8]]) as usize);
                data
            }
            0x4a => {
                if cdb[1] & 1 == 0 {
                    return self.fail(5, 0x24, 0);
                }
                let mut data = vec![0, 2, 0x80, 0x10];
                if cdb[4] & 0x10 != 0 {
                    data = vec![
                        0,
                        6,
                        4,
                        0x10,
                        self.event,
                        u8::from(self.tray_open) | (u8::from(self.present()) << 1),
                        0,
                        0,
                    ];
                    self.event = 0;
                }
                data.truncate(u16::from_be_bytes([cdb[7], cdb[8]]) as usize);
                data
            }
            0x43 => {
                self.ready()?;
                if cdb[2] & 0xf != 0 || !matches!(cdb[6], 0 | 1 | 0xaa) {
                    return self.fail(5, 0x24, 0);
                }
                let blocks =
                    (self.image.as_ref().unwrap().size() / 2048).min(u32::MAX as u64) as u32;
                let mut data = vec![0, 0, 1, 1];
                for (track, lba) in [(1, 0), (0xaa, blocks)] {
                    if cdb[6] == 0xaa && track == 1 {
                        continue;
                    }
                    data.extend([0, 0x14, track, 0]);
                    if cdb[1] & 2 != 0 {
                        let frames = lba + 150;
                        data.extend([
                            0,
                            (frames / 4500) as u8,
                            (frames / 75 % 60) as u8,
                            (frames % 75) as u8,
                        ]);
                    } else {
                        data.extend(lba.to_be_bytes());
                    }
                }
                let len = (data.len() - 2) as u16;
                data[..2].copy_from_slice(&len.to_be_bytes());
                data.truncate(u16::from_be_bytes([cdb[7], cdb[8]]) as usize);
                data
            }
            0x2a | 0xaa => return self.fail(7, 0x27, 0),
            _ => return self.fail(5, 0x20, 0),
        };
        Ok(Payload::Bytes(data))
    }
}

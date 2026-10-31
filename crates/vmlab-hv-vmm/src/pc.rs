//! i440fx machine-specific register wiring. PAM and interrupt aggregation
//! use the same engines as q35; their PCI identities and register maps differ.

use vmlab_hv_devices::{BarInfo, DeviceError, PciDevice, Snapshot};

pub fn bootorder(disks: &[vmlab_hv_proto::machine::Disk]) -> Vec<String> {
    [true, false]
        .into_iter()
        .flat_map(|cdrom| {
            disks.iter().filter(move |disk| {
                disk.bus == vmlab_hv_proto::machine::DiskBus::Ide && disk.cdrom == cdrom
            })
        })
        .map(|disk| {
            format!(
                "/pci@i0cf8/ide@1,1/drive@{}/disk@{}",
                disk.channel.unwrap(),
                disk.unit.unwrap()
            )
        })
        .collect()
}

pub struct Piix3 {
    config: [u8; 256],
}

impl Default for Piix3 {
    fn default() -> Self {
        let mut config = [0; 256];
        config[..4].copy_from_slice(&0x7000_8086u32.to_le_bytes());
        config[4..8].copy_from_slice(&[0, 0, 0, 2]);
        config[0xa..0xc].copy_from_slice(&[1, 6]);
        config[0xe] = 0x80;
        config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
        config[0x4c] = 0x4d;
        config[0x4e] = 3;
        config[0x60..0x64].fill(0x80);
        config[0x69] = 2;
        config[0x70] = 0x80;
        config[0x76..0x78].fill(0x0c);
        config[0x78] = 2;
        config[0xa0] = 8;
        config[0xa8] = 0x0f;
        Self { config }
    }
}

impl Piix3 {
    pub fn route(&self, pirq: usize) -> u8 {
        self.config
            .get(0x60 + pirq)
            .copied()
            .filter(|_| pirq < 4)
            .unwrap_or(0x80)
    }
}

impl PciDevice for Piix3 {
    fn config_read(&mut self, reg: usize) -> u32 {
        reg.checked_mul(4)
            .and_then(|offset| self.config.get(offset..offset.checked_add(4)?))
            .map_or(u32::MAX, |bytes| {
                u32::from_le_bytes(bytes.try_into().unwrap())
            })
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if offset > 3 || data.len() > 4 - offset as usize {
            return;
        }
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|reg| reg.checked_add(offset as usize))
        else {
            return;
        };
        for (i, byte) in data.iter().copied().enumerate() {
            let Some(pos) = start.checked_add(i).filter(|pos| *pos < 256) else {
                break;
            };
            let mask = match pos {
                4 => 7,
                5 => 5,
                0x0c | 0x3c | 0x40..=0xff => 0xff,
                _ => 0,
            };
            self.config[pos] = (self.config[pos] & !mask) | (byte & mask);
        }
    }

    fn bars(&self) -> Vec<BarInfo> {
        vec![]
    }
    fn bar_read(&mut self, _: u8, _: u64, data: &mut [u8]) {
        data.fill(0xff);
    }
    fn bar_write(&mut self, _: u8, _: u64, _: &[u8]) {}
}

impl Snapshot for Piix3 {
    fn state_id(&self) -> String {
        "piix3-isa".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok([b"PIX1".as_slice(), &self.config].concat())
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        if bytes.len() != 260 || &bytes[..4] != b"PIX1" {
            return Err(DeviceError::State("invalid PIIX3 state".into()));
        }
        self.config.copy_from_slice(&bytes[4..]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_bytes(encoded: &str) -> Vec<u8> {
        assert_eq!(encoded.len(), 512);
        encoded
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn default_boot_order_places_cds_before_disks_at_their_ide_paths() {
        let disks=serde_json::from_value::<Vec<vmlab_hv_proto::machine::Disk>>(serde_json::json!([
            {"id":"disk0","bus":"ide","channel":0,"unit":0,"path":"/disk0","format":"qcow2","read_only":false},
            {"id":"disk1","bus":"ide","channel":0,"unit":1,"path":"/disk1","format":"qcow2","read_only":false},
            {"id":"cd","bus":"ide","channel":1,"unit":0,"path":null,"format":"raw","cdrom":true,"read_only":true}
        ])).unwrap();
        assert_eq!(
            bootorder(&disks),
            [
                "/pci@i0cf8/ide@1,1/drive@1/disk@0",
                "/pci@i0cf8/ide@1,1/drive@0/disk@0",
                "/pci@i0cf8/ide@1,1/drive@0/disk@1",
            ]
        );
    }

    #[test]
    fn piix3_reset_config_matches_qemu_qtest() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/hwref/pc-chipset-reset.json"))
                .unwrap();
        let encoded = oracle["chipset_reset_config_hex"]["01.0"].as_str().unwrap();
        let expected = config_bytes(encoded);
        let mut bridge = Piix3::default();
        for offset in (0..256).step_by(4) {
            assert_eq!(
                bridge.config_read(offset / 4).to_le_bytes(),
                expected[offset..offset + 4],
                "reset config at {offset:#x}"
            );
        }
    }

    #[test]
    fn piix3_config_write_replay_matches_qemu_qtest() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/hwref/pc-chipset-write-masks.json"
        ))
        .unwrap();
        let mut bridge = Piix3::default();
        for step in oracle["chipset_config_write_replay"].as_array().unwrap() {
            if step["bdf"] != "01.0" {
                continue;
            }
            let written = step["written_dword"].as_u64().unwrap() as u32;
            for reg in 0..64 {
                bridge.config_write(reg, 0, &written.to_le_bytes());
            }
            let expected = config_bytes(step["config_hex"].as_str().unwrap());
            let differences: Vec<_> = (0..256)
                .filter_map(|offset| {
                    let actual = bridge.config_read(offset / 4).to_le_bytes()[offset % 4];
                    (actual != expected[offset])
                        .then(|| format!("{offset:#x}: {actual:#x} != {:#x}", expected[offset]))
                })
                .collect();
            assert!(
                differences.is_empty(),
                "write {written:#x}: {differences:?}"
            );
        }
    }

    #[test]
    fn piix3_identity_write_masks_and_snapshot() {
        let mut bridge = Piix3::default();
        assert_eq!(bridge.config_read(0), 0x7000_8086);
        assert_eq!(bridge.config_read(2), 0x0601_0000);
        assert_eq!(bridge.config_read(1), 0x0200_0000);
        bridge.config_write(1, 0, &[0xff; 2]);
        assert_eq!(bridge.config_read(1), 0x0200_0507);
        bridge.config_write(1, 0, &[0; 2]);
        assert_eq!(bridge.config_read(1), 0x0200_0000);
        bridge.config_write(0, 0, &[0xff; 4]);
        assert_eq!(bridge.config_read(0), 0x7000_8086);
        bridge.config_write(0x60 / 4, 0, &[0xff, 11, 10, 9]);
        assert_eq!(bridge.route(0), 0xff);
        let state = bridge.save_state().unwrap();
        bridge.config_write(0x60 / 4, 0, &[0; 4]);
        bridge.restore_state(&state).unwrap();
        assert_eq!(bridge.route(1), 11);
        assert!(bridge.restore_state(&state[..259]).is_err());
    }

    #[test]
    fn pc_swizzle_and_shared_pic_ioapic_destination() {
        use crate::q35::PirqRouter;
        use parking_lot::Mutex;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use vmlab_hv_devices::IrqLine;
        struct Line(AtomicBool);
        impl IrqLine for Line {
            fn set_level(&self, value: bool) {
                self.0.store(value, Ordering::SeqCst);
            }
        }
        let outputs: [Arc<Line>; 24] =
            std::array::from_fn(|_| Arc::new(Line(AtomicBool::new(false))));
        let router: Arc<Mutex<PirqRouter>> = PirqRouter::pc(std::array::from_fn(|i| {
            outputs[i].clone() as Arc<dyn IrqLine>
        }));
        router.lock().set_route(1, 11);
        let source = PirqRouter::line(
            &router,
            vmlab_hv_proto::machine::PciAddress {
                bus: 0,
                device: 2,
                function: 0,
            },
            1,
        )
        .unwrap();
        source.set_level(true);
        assert!(outputs[11].0.load(Ordering::SeqCst));
        assert!(!outputs[17].0.load(Ordering::SeqCst));
        router.lock().set_route(1, 0x8b);
        assert!(!outputs[11].0.load(Ordering::SeqCst));
        source.set_level(false);
    }
}

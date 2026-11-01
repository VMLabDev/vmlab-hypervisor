//! Dynamic fw_cfg callbacks used by edk2 SmmControl2Dxe/SmiFeatures.c.

use super::*;
use serde::{Deserialize, Serialize};
use vmlab_hv_firmware::fw_cfg::FwCfg;

#[derive(Default, Serialize, Deserialize)]
struct Negotiation {
    selector: u16,
    offset: u64,
    dma_high: u32,
    requested: [u8; 8],
    locked: Option<u64>,
}

/// Wrap the platform fw_cfg before registering it on the I/O bus. Only
/// broadcast, CPU-hotplug and CPU-hot-unplug SMI protocols are advertised.
/// The hotplug bits request firmware notification through SMI. The machine's
/// CPU inventory is fixed, so no insertion or removal events are generated.
/// Their snapshot state is the negotiated bitmap, not a mutable CPU inventory.
pub struct SmiFwCfg {
    inner: FwCfg,
    memory: vmlab_hv_devices::GuestMemory,
    requested_key: u16,
    ok_key: u16,
    negotiation: Negotiation,
}

impl SmiFwCfg {
    pub fn without_negotiation(inner: FwCfg, memory: vmlab_hv_devices::GuestMemory) -> Self {
        Self {
            inner,
            memory,
            requested_key: u16::MAX,
            ok_key: u16::MAX,
            negotiation: Negotiation::default(),
        }
    }

    pub fn new(mut inner: FwCfg, memory: vmlab_hv_devices::GuestMemory) -> Result<Self, String> {
        inner.add_file(
            "etc/smi/supported-features",
            SMI_SUPPORTED.to_le_bytes().to_vec(),
            false,
        )?;
        let requested_key = inner.add_file("etc/smi/requested-features", vec![0; 8], true)?;
        let ok_key = inner.add_file("etc/smi/features-ok", vec![0], false)?;
        Ok(Self {
            inner,
            memory,
            requested_key,
            ok_key,
            negotiation: Negotiation::default(),
        })
    }

    pub fn broadcast(&self) -> bool {
        self.negotiation
            .locked
            .is_some_and(|v| v & SMI_BROADCAST != 0)
    }

    fn select(&mut self, key: u16) {
        self.negotiation.selector = key & !0x4000;
        self.negotiation.offset = 0;
    }

    fn validate(&mut self) -> u8 {
        let requested = u64::from_le_bytes(self.negotiation.requested);
        if requested & !SMI_SUPPORTED != 0
            || self.negotiation.locked.is_some_and(|old| old != requested)
        {
            return 0;
        }
        self.negotiation.locked = Some(requested);
        1
    }

    fn dynamic_read(&mut self, data: &mut [u8]) {
        for byte in data {
            let offset = self.negotiation.offset;
            if self.negotiation.selector == self.ok_key {
                *byte = if offset == 0 { self.validate() } else { 0 };
            } else {
                *byte = self
                    .negotiation
                    .requested
                    .get(offset as usize)
                    .copied()
                    .unwrap_or(0);
            }
            self.negotiation.offset = offset.saturating_add(1);
        }
    }

    fn dynamic_write(&mut self, data: &[u8]) {
        for byte in data {
            if let Some(target) = self
                .negotiation
                .requested
                .get_mut(self.negotiation.offset as usize)
            {
                *target = *byte;
            }
            self.negotiation.offset = self.negotiation.offset.saturating_add(1);
        }
    }
}

impl BusDevice for SmiFwCfg {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.inner.read(offset, data);
        if offset == 1 && data.len() == 1 {
            if self.negotiation.selector == self.ok_key
                || self.negotiation.selector == self.requested_key
            {
                self.dynamic_read(data);
            } else {
                self.negotiation.offset = self.negotiation.offset.saturating_add(1);
            }
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        // Read the original descriptor before the delegated transfer overwrites
        // its control word with completion status.
        let descriptor = match (offset, data.len()) {
            (4, 8) => Some(u64::from_be_bytes(data.try_into().unwrap())),
            (8, 4) => Some(
                (u64::from(self.negotiation.dma_high) << 32)
                    | u64::from(u32::from_be_bytes(data.try_into().unwrap())),
            ),
            _ => None,
        };
        let header = descriptor.and_then(|address| {
            let mut bytes = [0; 16];
            self.memory
                .read_slice(&mut bytes, GuestAddress(address))
                .ok()
                .map(|()| (address, bytes))
        });
        self.inner.write(offset, data);
        match (offset, data.len()) {
            (0, 2) => self.select(u16::from_le_bytes(data.try_into().unwrap())),
            (1, 1) if self.negotiation.selector == self.requested_key => self.dynamic_write(data),
            (4, 4) => self.negotiation.dma_high = u32::from_be_bytes(data.try_into().unwrap()),
            _ => {}
        }
        if let Some((address, header)) = header {
            self.dma_completed(address, header);
        }
    }
}

impl SmiFwCfg {
    fn dma_completed(&mut self, descriptor: u64, header: [u8; 16]) {
        let mut completion = [0; 4];
        if self
            .memory
            .read_slice(&mut completion, GuestAddress(descriptor))
            .is_err()
        {
            return;
        }
        self.dma_request(descriptor, header, completion);
    }

    fn dma_request(&mut self, descriptor: u64, header: [u8; 16], completion: [u8; 4]) {
        let control = u32::from_be_bytes(header[..4].try_into().unwrap());
        if control & 8 != 0 {
            self.select((control >> 16) as u16);
        }
        if completion != [0; 4] {
            return;
        }
        let length = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let address = u64::from_be_bytes(header[8..].try_into().unwrap());
        if self.negotiation.selector != self.ok_key
            && self.negotiation.selector != self.requested_key
        {
            self.negotiation.offset = self.negotiation.offset.saturating_add(length as u64);
            return;
        }
        if control & 2 != 0 {
            for done in (0..length).step_by(4096) {
                let mut bytes = vec![0; (length - done).min(4096)];
                self.dynamic_read(&mut bytes);
                if self
                    .memory
                    .write_slice(&bytes, GuestAddress(address + done as u64))
                    .is_err()
                {
                    let _ = self
                        .memory
                        .write_slice(&1u32.to_be_bytes(), GuestAddress(descriptor));
                    return;
                }
            }
        } else if control & 16 != 0 && self.negotiation.selector == self.requested_key {
            let mut bytes = [0; 8];
            if length <= 8
                && self
                    .memory
                    .read_slice(&mut bytes[..length], GuestAddress(address))
                    .is_ok()
            {
                self.dynamic_write(&bytes[..length]);
            }
        } else if control & 4 != 0 {
            self.negotiation.offset = self.negotiation.offset.saturating_add(length as u64);
        }
    }
}

impl Snapshot for SmiFwCfg {
    fn state_id(&self) -> String {
        "smm-fw-cfg".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&(self.inner.save_state()?, &self.negotiation))
            .map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let (inner, negotiation): (Vec<u8>, Negotiation) =
            serde_json::from_slice(state).map_err(|e| DeviceError::State(e.to_string()))?;
        if negotiation.locked.is_some_and(|v| v & !SMI_SUPPORTED != 0) {
            return Err(DeviceError::State("unsupported SMI features".into()));
        }
        self.inner.restore_state(&inner)?;
        self.negotiation = negotiation;
        Ok(())
    }
}

#[cfg(test)]
mod pc_tests {
    use super::*;

    #[test]
    fn macos_p2_fwcfg_inventory_and_e820_match_reference_with_named_differences() {
        use vmlab_hv_devices::BusDevice;
        let memory = vmlab_hv_devices::GuestMemory::new();
        let platform = vmlab_hv_firmware::Platform {
            applesmc: true,
            amd_hypertransport: false,
            vcpus: 4,
            max_cpus: 4,
            ram_regions: vec![(0, 0x80000000), (0x100000000, 0x180000000)],
            ecam_base: 0xe0000000,
            pci64_base: Some(32 << 30),
            pci_devices: [
                (0, 0),
                (1, 0),
                (2, 0),
                (3, 0),
                (4, 0),
                (31, 0),
                (31, 2),
                (31, 3),
            ]
            .into_iter()
            .map(|(device, function)| vmlab_hv_firmware::PciDevice {
                device,
                function,
                vga: false,
            })
            .collect(),
            tpm: false,
            smm: false,
        };
        let options = vmlab_hv_firmware::FirmwareOptions {
            smbios: vmlab_hv_firmware::smbios::SmbiosConfig {
                baseboard: true,
                processor_id: [0xe3, 6, 5, 0, 0xff, 0xfb, 0x8b, 0x17],
                ..Default::default()
            },
            ..Default::default()
        };
        let inner = FwCfg::for_platform(memory.clone(), &platform, &options).unwrap();
        let mut device = SmiFwCfg::new(inner, memory).unwrap();
        device.write(0, &0x19u16.to_le_bytes());
        let mut count = [0; 4];
        for byte in &mut count {
            device.read(1, std::slice::from_mut(byte));
        }
        let mut entries = vec![0; u32::from_be_bytes(count) as usize * 64];
        for byte in &mut entries {
            device.read(1, std::slice::from_mut(byte));
        }
        let mut actual = std::collections::BTreeMap::new();
        for entry in entries.as_chunks::<64>().0 {
            let end = entry[8..].iter().position(|byte| *byte == 0).unwrap();
            let name = std::str::from_utf8(&entry[8..8 + end]).unwrap().to_owned();
            actual.insert(
                name,
                u32::from_be_bytes(entry[..4].try_into().unwrap()) as usize,
            );
        }
        let reference: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../../docs/hwref/q35-macos-tahoe/fwcfg-files.json"
        ))
        .unwrap();
        for entry in reference {
            let name = entry["name"].as_str().unwrap();
            let size = entry["size"].as_u64().unwrap() as usize;
            match name {
                "genroms/kvmvapic.bin" => {
                    // QEMU's optional APIC option ROM is not shipped or used.
                    assert_eq!(size, 9216);
                    assert!(!actual.contains_key(name));
                }
                "etc/tpm/log" => {
                    // A zero-length log with no TPM has no consumer.
                    assert_eq!(size, 0);
                    assert!(!actual.contains_key(name));
                }
                _ => assert_eq!(actual.remove(name), Some(size), "fw_cfg {name}"),
            }
        }
        assert!(actual.is_empty(), "unexpected fw_cfg files: {actual:?}");
        let e820 = device.inner.file("etc/e820").unwrap();
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../docs/hwref/q35-macos-tahoe/smc-e820.json"
        ))
        .unwrap();
        let captured: Vec<_> = capture["e820"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(e820, captured);
        let ranges: Vec<_> = e820
            .as_chunks::<20>()
            .0
            .iter()
            .map(|entry| {
                (
                    u64::from_le_bytes(entry[..8].try_into().unwrap()),
                    u64::from_le_bytes(entry[8..16].try_into().unwrap()),
                    u32::from_le_bytes(entry[16..].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            ranges,
            vec![
                (0xfeffc000, 0x4000, 2),
                (0, 0x80000000, 1),
                (0x100000000, 0x180000000, 1)
            ]
        );
    }

    #[test]
    fn pc_preserves_fw_cfg_inventory_without_q35_smi_negotiation() {
        let memory = vmlab_hv_devices::GuestMemory::new();
        let config = vmlab_hv_firmware::FwCfg::for_pc(
            memory.clone(),
            &vmlab_hv_firmware::pc::Platform::default(),
            &vmlab_hv_firmware::FirmwareOptions::default(),
        )
        .unwrap();
        let wrapped = SmiFwCfg::without_negotiation(config, memory);
        for name in [
            "etc/smi/supported-features",
            "etc/smi/requested-features",
            "etc/smi/features-ok",
        ] {
            assert!(wrapped.inner.file(name).is_none());
        }
        assert!(wrapped.inner.file("etc/acpi/rsdp").is_some());
        assert!(!wrapped.broadcast());
    }
}

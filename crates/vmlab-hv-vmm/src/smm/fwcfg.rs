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
/// broadcast and the CPU-hotplug SMI protocol are advertised. The machine's
/// CPU inventory is fixed, so no insertion or removal events are generated.
pub struct SmiFwCfg {
    inner: FwCfg,
    memory: vmlab_hv_devices::GuestMemory,
    requested_key: u16,
    ok_key: u16,
    negotiation: Negotiation,
}

impl SmiFwCfg {
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
        if negotiation.locked.is_some_and(|v| v & !SMI_BROADCAST != 0) {
            return Err(DeviceError::State("unsupported SMI features".into()));
        }
        self.inner.restore_state(&inner)?;
        self.negotiation = negotiation;
        Ok(())
    }
}

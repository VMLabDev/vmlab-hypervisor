//! Firmware configuration protocol: QEMU's published `specs/fw_cfg.rst`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::{BusDevice, DeviceError, GuestMemory, Snapshot};

pub const IO_BASE: u16 = 0x510;
pub const IO_SIZE: u16 = 12;
const DIRECTORY: u16 = 0x19;
const FIRST_FILE: u16 = 0x20;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    data: Vec<u8>,
    writable: bool,
    name: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    selector: u16,
    offset: u64,
    dma_address: u64,
    entries: BTreeMap<u16, Entry>,
}

/// Register on I/O ports 0x510..0x51c. DMA uses the supplied guest memory.
pub struct FwCfg {
    memory: GuestMemory,
    state: State,
}

impl FwCfg {
    pub fn new(memory: GuestMemory) -> Self {
        let mut device = Self {
            memory,
            state: State {
                version: 1,
                selector: 0,
                offset: 0,
                dma_address: 0,
                entries: BTreeMap::new(),
            },
        };
        device.insert(0, b"QEMU".to_vec(), false, None);
        device.insert(1, 3u32.to_le_bytes().to_vec(), false, None);
        device.rebuild_directory();
        device
    }

    /// Generate and register the complete q35 firmware input set.
    pub fn for_platform(
        memory: GuestMemory,
        platform: &crate::Platform,
        options: &crate::FirmwareOptions,
    ) -> Result<Self, String> {
        let mut device = Self::new(memory);
        for (key, data) in crate::platform::numeric_items(platform, options)? {
            device.add_numeric(key, data)?;
        }
        let acpi = crate::acpi::generate(platform)?;
        let mut files: BTreeMap<_, _> = crate::platform::ancillary_files(platform, options)?
            .into_iter()
            .collect();
        files.insert("etc/acpi/rsdp".into(), acpi.rsdp);
        files.insert("etc/acpi/tables".into(), acpi.tables);
        files.insert("etc/table-loader".into(), acpi.loader);
        for (name, data) in files {
            device.add_file(&name, data, false)?;
        }
        Ok(device)
    }

    fn insert(&mut self, key: u16, data: Vec<u8>, writable: bool, name: Option<String>) {
        self.state.entries.insert(
            key,
            Entry {
                data,
                writable,
                name,
            },
        );
    }

    /// Numeric items are read-only; architecture-local keys retain bit 15.
    pub fn add_numeric(&mut self, key: u16, data: Vec<u8>) -> Result<(), String> {
        if key & 0x4000 != 0 || key & 0x3fff >= FIRST_FILE || matches!(key, 0 | 1 | DIRECTORY) {
            return Err("reserved fw_cfg numeric key".into());
        }
        self.insert(key, data, false, None);
        Ok(())
    }

    /// Add a fixed-size file. DMA writes are permitted only when `writable`.
    pub fn add_file(&mut self, name: &str, data: Vec<u8>, writable: bool) -> Result<u16, String> {
        if name.is_empty() || name.len() >= 56 || !name.is_ascii() || name.contains('\0') {
            return Err("fw_cfg filename must contain 1..55 non-NUL ASCII bytes".into());
        }
        if data.len() > u32::MAX as usize {
            return Err("fw_cfg file exceeds u32 length".into());
        }
        if self
            .state
            .entries
            .values()
            .any(|e| e.name.as_deref() == Some(name))
        {
            return Err(format!("duplicate fw_cfg file {name}"));
        }
        let key = (FIRST_FILE..0x4000)
            .find(|k| !self.state.entries.contains_key(k))
            .ok_or("fw_cfg file directory full")?;
        self.insert(key, data, writable, Some(name.into()));
        self.rebuild_directory();
        Ok(key)
    }

    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.state
            .entries
            .values()
            .find(|e| e.name.as_deref() == Some(name))
            .map(|e| e.data.as_slice())
    }

    fn rebuild_directory(&mut self) {
        let mut files: Vec<_> = self
            .state
            .entries
            .iter()
            .filter(|(_, e)| e.name.is_some())
            .collect();
        files.sort_by_key(|(_, e)| e.name.as_deref());
        let mut data = (files.len() as u32).to_be_bytes().to_vec();
        for (key, entry) in files {
            data.extend_from_slice(&(entry.data.len() as u32).to_be_bytes());
            data.extend_from_slice(&key.to_be_bytes());
            data.extend_from_slice(&[0; 2]);
            let mut name = [0; 56];
            let bytes = entry.name.as_ref().unwrap().as_bytes();
            name[..bytes.len()].copy_from_slice(bytes);
            data.extend_from_slice(&name);
        }
        self.insert(DIRECTORY, data, false, None);
    }

    fn select(&mut self, selector: u16) {
        self.state.selector = selector & !0x4000;
        self.state.offset = 0;
    }

    fn stream_read(&mut self, data: &mut [u8]) {
        data.fill(0);
        if let Some(entry) = self.state.entries.get(&self.state.selector)
            && let Ok(start) = usize::try_from(self.state.offset)
            && start < entry.data.len()
        {
            let n = data.len().min(entry.data.len() - start);
            data[..n].copy_from_slice(&entry.data[start..start + n]);
        }
        self.state.offset = self.state.offset.saturating_add(data.len() as u64);
    }

    fn dma(&mut self, descriptor: u64) {
        let result = self.transfer(descriptor);
        // An inaccessible descriptor cannot receive completion status.
        let _ = self.memory.write_slice(
            &(u32::from(result.is_err())).to_be_bytes(),
            GuestAddress(descriptor),
        );
    }

    fn transfer(&mut self, descriptor: u64) -> Result<(), ()> {
        let mut header = [0; 16];
        self.memory
            .read_slice(&mut header, GuestAddress(descriptor))
            .map_err(|_| ())?;
        let control = u32::from_be_bytes(header[..4].try_into().unwrap());
        let length = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let address = u64::from_be_bytes(header[8..].try_into().unwrap());
        if control & 8 != 0 {
            self.select((control >> 16) as u16);
        }
        if control & 2 != 0 {
            address.checked_add(length as u64).ok_or(())?;
            let mut buffer = [0; 4096];
            let mut done = 0;
            while done < length {
                let n = buffer.len().min(length - done);
                self.stream_read(&mut buffer[..n]);
                self.memory
                    .write_slice(&buffer[..n], GuestAddress(address + done as u64))
                    .map_err(|_| ())?;
                done += n;
            }
        } else if control & 16 != 0 {
            let entry = self.state.entries.get_mut(&self.state.selector).ok_or(())?;
            let start = usize::try_from(self.state.offset).map_err(|_| ())?;
            let end = start.checked_add(length).ok_or(())?;
            if !entry.writable || end > entry.data.len() {
                return Err(());
            }
            let mut buffer = vec![0; length];
            self.memory
                .read_slice(&mut buffer, GuestAddress(address))
                .map_err(|_| ())?;
            entry.data[start..end].copy_from_slice(&buffer);
            self.state.offset = end as u64;
        } else if control & 4 != 0 {
            self.state.offset = self.state.offset.saturating_add(length as u64);
        }
        Ok(())
    }
}

impl BusDevice for FwCfg {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if offset == 1 && data.len() == 1 {
            self.stream_read(data);
        } else if offset >= 4
            && offset
                .checked_add(data.len() as u64)
                .is_some_and(|end| end <= 12)
        {
            let signature = b"QEMU CFG";
            data.copy_from_slice(&signature[offset as usize - 4..offset as usize - 4 + data.len()]);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match (offset, data.len()) {
            (0, 2) => self.select(u16::from_le_bytes(data.try_into().unwrap())),
            (4, 8) => {
                let address = u64::from_be_bytes(data.try_into().unwrap());
                self.state.dma_address = 0;
                self.dma(address);
            }
            (4, 4) => {
                self.state.dma_address = (u32::from_be_bytes(data.try_into().unwrap()) as u64) << 32
            }
            (8, 4) => {
                let address =
                    self.state.dma_address | u32::from_be_bytes(data.try_into().unwrap()) as u64;
                self.state.dma_address = 0;
                self.dma(address);
            }
            _ => {}
        }
    }
}

impl Snapshot for FwCfg {
    fn state_id(&self) -> String {
        "fw-cfg".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&self.state).map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, data: &[u8]) -> Result<(), DeviceError> {
        let state: State =
            serde_json::from_slice(data).map_err(|e| DeviceError::State(e.to_string()))?;
        if state.version != 1
            || state.entries.len() != self.state.entries.len()
            || state.entries.iter().any(|(key, entry)| {
                self.state.entries.get(key).is_none_or(|old| {
                    old.name != entry.name
                        || old.writable != entry.writable
                        || old.data.len() != entry.data.len()
                        || (!old.writable && old.data != entry.data)
                })
            })
        {
            return Err(DeviceError::State(
                "incompatible fw_cfg configuration".into(),
            ));
        }
        self.state = state;
        Ok(())
    }
}

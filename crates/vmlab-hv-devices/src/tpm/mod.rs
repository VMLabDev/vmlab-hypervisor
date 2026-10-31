//! TCG TIS 1.3 FIFO interface, with TPM 2.0 commands executed by swtpm.

mod backend;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{BusDevice, DeviceError, IrqLine, Snapshot};
use backend::{Backend, MAX_PACKET};

pub const MMIO_BASE: u64 = 0xfed4_0000;
/// Five locality pages followed by the captured physical-presence page.
pub const MMIO_SIZE: u64 = 0x6000;
pub const PPI_BASE: u64 = MMIO_BASE + 0x5000;
const READY: u8 = 0x40;
const VALID: u8 = 0x80;
const AVAILABLE: u8 = 0x10;
const EXPECT: u8 = 8;
const IRQ_MASK: u32 = 0x87;

#[derive(Clone, Serialize, Deserialize)]
struct State {
    version: u32,
    active: Option<u8>,
    pending: u8,
    seized: u8,
    status: u8,
    command: Vec<u8>,
    response: Vec<u8>,
    cursor: usize,
    enable: [u32; 5],
    interrupts: [u32; 5],
    vector: [u8; 5],
    interface_locked: bool,
    hashing: bool,
    ppi: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    tis: State,
    blobs: Vec<Vec<u8>>,
}

/// Map at [`MMIO_BASE`] for [`MMIO_SIZE`] bytes and wire the optional IRQ.
/// Command execution completes before the MMIO write returns.
pub struct TpmTis {
    backend: Mutex<Backend>,
    state: State,
    irq: Option<Arc<dyn IrqLine>>,
    error: Option<String>,
}

impl TpmTis {
    pub fn connect(
        path: impl AsRef<Path>,
        irq: Option<Arc<dyn IrqLine>>,
    ) -> Result<Self, DeviceError> {
        Ok(Self {
            backend: Mutex::new(Backend::connect(path.as_ref())?),
            state: State {
                version: 1,
                active: None,
                pending: 0,
                seized: 0,
                status: 0,
                command: Vec::new(),
                response: Vec::new(),
                cursor: 0,
                enable: [8; 5],
                interrupts: [0; 5],
                vector: [0; 5],
                interface_locked: false,
                hashing: false,
                ppi: vec![0; 0x1000],
            },
            irq,
            error: None,
        })
    }

    pub fn shutdown(&mut self) -> Result<(), DeviceError> {
        self.backend.get_mut().unwrap().shutdown()?;
        Ok(())
    }

    /// Most recent backend failure; transport errors produce TPM_RC_FAILURE.
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn update_irq(&self) {
        if let Some(irq) = &self.irq {
            irq.set_level((0..5).any(|i| {
                self.state.enable[i] & (1 << 31) != 0
                    && self.state.enable[i] & self.state.interrupts[i] & IRQ_MASK != 0
            }));
        }
    }

    fn event(&mut self, locality: usize, bits: u32) {
        self.state.interrupts[locality] |= bits;
        self.update_irq();
    }

    fn activate(&mut self, locality: u8) {
        let delayed = self.state.pending & (1 << locality) != 0;
        self.state.active = Some(locality);
        self.state.pending &= !(1 << locality);
        if delayed {
            for i in 0..5 {
                self.event(i, 4);
            }
        }
    }

    fn ready(&mut self, locality: usize) {
        self.state.command.clear();
        self.state.response.clear();
        self.state.cursor = 0;
        self.state.status = READY;
        self.state.hashing = false;
        self.event(locality, 0x80);
    }

    fn expected_length(&self) -> Option<usize> {
        (self.state.command.len() >= 6)
            .then(|| u32::from_be_bytes(self.state.command[2..6].try_into().unwrap()) as usize)
    }

    fn fifo_write(&mut self, value: u8) {
        if self.state.status & READY != 0 {
            self.state.status = VALID | EXPECT;
            if let Some(locality) = self.state.active {
                self.event(locality as usize, 2);
            }
        }
        if self.state.status & EXPECT == 0 {
            return;
        }
        if self.state.command.len() < MAX_PACKET {
            self.state.command.push(value);
        }
        if self
            .expected_length()
            .is_some_and(|len| !(10..=MAX_PACKET).contains(&len) || self.state.command.len() >= len)
            || self.state.command.len() == MAX_PACKET
        {
            self.state.status &= !EXPECT;
        }
    }

    fn go(&mut self, locality: u8) {
        if self.state.status != VALID
            || self.expected_length() != Some(self.state.command.len())
            || self.state.command.len() < 10
        {
            return;
        }
        let response = self
            .backend
            .get_mut()
            .unwrap()
            .execute(locality, &self.state.command);
        self.state.response = match response {
            Ok(bytes) => bytes,
            Err(error) => {
                self.error = Some(error.to_string());
                vec![0x80, 1, 0, 0, 0, 10, 0, 0, 1, 1]
            }
        };
        self.state.cursor = 0;
        self.state.status = VALID | AVAILABLE;
        self.event(locality as usize, 3);
    }

    fn read_byte(&mut self, locality: usize, register: u64) -> u8 {
        match register {
            0 => {
                let established = match self.backend.get_mut().unwrap().established() {
                    Ok(bit) => bit,
                    Err(error) => {
                        self.error = Some(error.to_string());
                        false
                    }
                };
                VALID
                    | u8::from(!established)
                    | if self.state.pending & (1 << locality) != 0 {
                        2
                    } else {
                        0
                    }
                    | if self.state.active == Some(locality as u8) {
                        0x20
                    } else {
                        0
                    }
                    | if self.state.pending & !(1 << locality) != 0 {
                        4
                    } else {
                        0
                    }
                    | if self.state.seized & (1 << locality) != 0 {
                        0x10
                    } else {
                        0
                    }
            }
            8..=11 => self.state.enable[locality].to_le_bytes()[(register - 8) as usize],
            12 => self.state.vector[locality],
            16..=19 => self.state.interrupts[locality].to_le_bytes()[(register - 16) as usize],
            20..=23 => 0x3000_0697u32.to_le_bytes()[(register - 20) as usize],
            24 => {
                if self.state.active == Some(locality as u8) {
                    self.state.status
                } else {
                    0xff
                }
            }
            25..=26 => {
                if self.state.active != Some(locality as u8) {
                    return 0xff;
                }
                let burst = if self.state.status & AVAILABLE != 0 {
                    self.state.response.len() - self.state.cursor
                } else {
                    MAX_PACKET - self.state.command.len()
                };
                (burst as u16).to_le_bytes()[(register - 25) as usize]
            }
            27 => {
                if self.state.active == Some(locality as u8) {
                    4
                } else {
                    0xff
                }
            }
            0x30..=0x37 => (0x2100u64 | (u64::from(self.state.interface_locked) << 19))
                .to_le_bytes()[(register - 0x30) as usize],
            0xf00..=0xf03 => 0x0001_1014u32.to_le_bytes()[(register - 0xf00) as usize],
            0xf04 => 1,
            _ => 0xff,
        }
    }
}

impl crate::Resettable for TpmTis {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.backend.get_mut().unwrap().reset()?;
        self.state = State {
            version: 1,
            active: None,
            pending: 0,
            seized: 0,
            status: 0,
            command: Vec::new(),
            response: Vec::new(),
            cursor: 0,
            enable: [8; 5],
            interrupts: [0; 5],
            vector: [0; 5],
            interface_locked: false,
            hashing: false,
            ppi: vec![0; 0x1000],
        };
        self.error = None;
        self.update_irq();
        Ok(())
    }
}

impl BusDevice for TpmTis {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if offset >= MMIO_SIZE
            || offset
                .checked_add(data.len() as u64)
                .is_none_or(|n| n > MMIO_SIZE)
        {
            return;
        }
        if offset >= 0x5000 {
            data.copy_from_slice(
                &self.state.ppi[offset as usize - 0x5000..offset as usize - 0x5000 + data.len()],
            );
            return;
        }
        let locality = (offset >> 12) as usize;
        let register = offset & 0xfff;
        if matches!(register, 0x24..=0x27 | 0x80..=0xbf) {
            data.fill(0xff);
            if self.state.active == Some(locality as u8) {
                for byte in data {
                    if let Some(value) = self.state.response.get(self.state.cursor) {
                        *byte = *value;
                        self.state.cursor += 1;
                    }
                }
                if self.state.cursor == self.state.response.len() {
                    self.state.status &= !AVAILABLE;
                }
            }
            return;
        }
        for (index, byte) in data.iter_mut().enumerate() {
            if register + (index as u64) < 0x1000 {
                *byte = self.read_byte(locality, register + index as u64);
            }
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.is_empty()
            || offset >= MMIO_SIZE
            || offset
                .checked_add(data.len() as u64)
                .is_none_or(|n| n > MMIO_SIZE)
        {
            return;
        }
        if offset >= 0x5000 {
            self.state.ppi[offset as usize - 0x5000..offset as usize - 0x5000 + data.len()]
                .copy_from_slice(data);
            return;
        }
        let locality = (offset >> 12) as usize;
        let register = offset & 0xfff;
        if register == 0 {
            let value = data[0];
            if value & 0x10 != 0 {
                self.state.seized &= !(1 << locality);
            }
            if value & 2 != 0 {
                if self.state.active.is_none() {
                    self.activate(locality as u8);
                } else if self.state.active != Some(locality as u8) {
                    self.state.pending |= 1 << locality;
                }
            }
            if value & 8 != 0 && self.state.active.is_none_or(|old| locality > old as usize) {
                if let Some(old) = self.state.active {
                    self.state.seized |= 1 << old;
                }
                self.ready(locality);
                self.activate(locality as u8);
            }
            if value & 0x20 != 0 {
                self.state.pending &= !(1 << locality);
                if self.state.active == Some(locality as u8) {
                    self.state.active = None;
                    self.state.command.clear();
                    self.state.response.clear();
                    self.state.cursor = 0;
                    self.state.status = 0;
                    self.state.hashing = false;
                    if self.state.pending != 0 {
                        self.activate(7 - self.state.pending.leading_zeros() as u8);
                    }
                }
            }
            return;
        }
        if locality == 4 && register == 0x28 && self.state.active.is_none_or(|old| old == 4) {
            match self.backend.get_mut().unwrap().hash_start() {
                Ok(()) => {
                    self.activate(4);
                    self.ready(4);
                    self.state.hashing = true;
                }
                Err(error) => self.error = Some(error.to_string()),
            }
            return;
        }
        if locality == 4 && register == 0x20 && self.state.active == Some(4) && self.state.hashing {
            if let Err(error) = self.backend.get_mut().unwrap().hash_end() {
                self.error = Some(error.to_string());
            }
            self.state.hashing = false;
            self.state.active = None;
            if self.state.pending != 0 {
                self.activate(7 - self.state.pending.leading_zeros() as u8);
            }
            return;
        }
        if matches!(register, 0x24..=0x27 | 0x80..=0xbf) {
            if self.state.active == Some(locality as u8) {
                if locality == 4 && self.state.hashing {
                    if let Err(error) = self.backend.get_mut().unwrap().hash_data(data) {
                        self.error = Some(error.to_string());
                    }
                    return;
                }
                for &byte in data {
                    self.fifo_write(byte);
                }
            }
            return;
        }
        for (index, &value) in data.iter().enumerate() {
            let reg = register + index as u64;
            match reg {
                8..=11 => {
                    let shift = (reg - 8) * 8;
                    self.state.enable[locality] =
                        (self.state.enable[locality] & !(255 << shift)) | ((value as u32) << shift);
                    self.state.enable[locality] &= (1 << 31) | IRQ_MASK | 0x18;
                }
                12 => self.state.vector[locality] = value,
                0x32 if value & 8 != 0 => self.state.interface_locked = true,
                16..=19 => self.state.interrupts[locality] &= !((value as u32) << ((reg - 16) * 8)),
                24 if self.state.active == Some(locality as u8) => {
                    if value & READY != 0 {
                        self.ready(locality);
                    } else if value & 0x20 != 0 {
                        self.go(locality as u8);
                    } else if value & 2 != 0 && !self.state.response.is_empty() {
                        self.state.cursor = 0;
                        self.state.status = VALID | AVAILABLE;
                        self.event(locality, 3);
                    }
                }
                27 if value & 2 != 0
                    && locality >= 3
                    && self.state.active == Some(locality as u8) =>
                {
                    if let Err(error) = self
                        .backend
                        .get_mut()
                        .unwrap()
                        .reset_established(locality as u8)
                    {
                        self.error = Some(error.to_string());
                    }
                }
                _ => {}
            }
        }
        self.update_irq();
    }
}

impl Snapshot for TpmTis {
    fn state_id(&self) -> String {
        "tpm-tis".into()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let blobs = self.backend.lock().unwrap().save()?;
        serde_json::to_vec(&Saved {
            tis: self.state.clone(),
            blobs,
        })
        .map_err(|e| DeviceError::State(e.to_string()))
    }

    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let saved: Saved =
            serde_json::from_slice(bytes).map_err(|e| DeviceError::State(e.to_string()))?;
        let s = &saved.tis;
        if s.version != 1
            || s.active.is_some_and(|l| l > 4)
            || s.pending & !31 != 0
            || s.seized & !31 != 0
            || s.command.len() > MAX_PACKET
            || s.response.len() > MAX_PACKET
            || s.cursor > s.response.len()
            || s.ppi.len() != 0x1000
        {
            return Err(DeviceError::State("invalid TPM TIS snapshot".into()));
        }
        self.backend.get_mut().unwrap().restore(&saved.blobs)?;
        self.state = saved.tis;
        self.error = None;
        self.update_irq();
        Ok(())
    }
}

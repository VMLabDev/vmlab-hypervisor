use vm_memory::{Bytes, GuestAddress};

use crate::{DeviceError, GuestMemory};

use super::state::{Reader, Writer};

pub(super) fn read<const N: usize>(mem: &GuestMemory, addr: u64) -> Result<[u8; N], DeviceError> {
    addr.checked_add(N as u64)
        .ok_or_else(|| DeviceError::Memory("USB DMA overflow".into()))?;
    let mut bytes = [0; N];
    mem.read_slice(&mut bytes, GuestAddress(addr))
        .map_err(|e| DeviceError::Memory(e.to_string()))?;
    Ok(bytes)
}

pub(super) fn write(mem: &GuestMemory, addr: u64, bytes: &[u8]) -> Result<(), DeviceError> {
    addr.checked_add(bytes.len() as u64)
        .ok_or_else(|| DeviceError::Memory("USB DMA overflow".into()))?;
    mem.write_slice(bytes, GuestAddress(addr))
        .map_err(|e| DeviceError::Memory(e.to_string()))
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    pub fn read(mem: &GuestMemory, addr: u64) -> Result<Self, DeviceError> {
        let b = read::<16>(mem, addr)?;
        Ok(Self {
            parameter: u64::from_le_bytes(b[..8].try_into().unwrap()),
            status: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            control: u32::from_le_bytes(b[12..].try_into().unwrap()),
        })
    }
    pub fn write(self, mem: &GuestMemory, addr: u64) -> Result<(), DeviceError> {
        let mut b = [0; 16];
        b[..8].copy_from_slice(&self.parameter.to_le_bytes());
        b[8..12].copy_from_slice(&self.status.to_le_bytes());
        b[12..].copy_from_slice(&self.control.to_le_bytes());
        write(mem, addr, &b)
    }
    pub fn kind(self) -> u32 {
        (self.control >> 10) & 63
    }
    pub fn length(self) -> usize {
        (self.status & 0x1ffff) as usize
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Ring {
    pub pointer: u64,
    pub cycle: bool,
}

impl Ring {
    pub fn new(value: u64) -> Self {
        Self {
            pointer: value & !15,
            cycle: value & 1 != 0,
        }
    }
    pub fn value(self) -> u64 {
        self.pointer | u64::from(self.cycle)
    }
    /// Link-only loops are bounded, as are callers' command/TD walks.
    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<(u64, Trb)>, DeviceError> {
        for _ in 0..256 {
            let trb = Trb::read(mem, self.pointer)?;
            if (trb.control & 1 != 0) != self.cycle {
                return Ok(None);
            }
            let addr = self.pointer;
            if trb.kind() == 6 {
                self.pointer = trb.parameter & !15;
                if trb.control & 2 != 0 {
                    self.cycle = !self.cycle;
                }
            } else {
                self.pointer = self
                    .pointer
                    .checked_add(16)
                    .ok_or_else(|| DeviceError::Memory("USB ring overflow".into()))?;
                return Ok(Some((addr, trb)));
            }
        }
        Err(DeviceError::Other("USB ring contains a link loop".into()))
    }
    pub fn save(self, w: &mut Writer) {
        w.u64(self.pointer);
        w.u64(u64::from(self.cycle));
    }
    pub fn restore(r: &mut Reader<'_>) -> Result<Self, DeviceError> {
        let pointer = r.u64()?;
        if pointer & 15 != 0 {
            return Err(super::state::invalid());
        }
        Ok(Self {
            pointer,
            cycle: r.bool()?,
        })
    }
}

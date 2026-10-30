//! Intel command-set NOR flash for the edk2 4 MiB OVMF layout.
//!
//! Route the entire aperture through MMIO, including reads: status and CFI
//! commands change what reads return. The VMM supplies the issuing vCPU's
//! SMM state to `write_access`; ordinary bus writes are always non-SMM.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::{BusDevice, DeviceError, Snapshot};

pub const BASE: u64 = 0xffc0_0000;
pub const SIZE: usize = 0x400000;
pub const VARS_SIZE: usize = 0x84000;
pub const CODE_BASE: u64 = BASE + VARS_SIZE as u64;
pub const CODE_SIZE: usize = SIZE - VARS_SIZE;
pub const BLOCK_SIZE: usize = 0x1000;

const READY: u8 = 0x80;
const PROGRAM_ERROR: u8 = 0x10;
const ERASE_ERROR: u8 = 0x20;
const PROTECTED: u8 = 0x02;

#[derive(Clone, Copy)]
#[repr(u8)]
enum Mode {
    Array,
    Status,
    Id,
    Query,
    Program,
    Erase,
}

pub struct Pflash {
    bytes: Vec<u8>,
    vars: File,
    secure: bool,
    mode: Mode,
    status: u8,
    erase_block: usize,
}

impl Pflash {
    /// Open existing firmware files without truncating or initializing VARS.
    pub fn open(code: &Path, vars: &Path, secure: bool) -> Result<Self, DeviceError> {
        let code = std::fs::read(code)?;
        let vars = OpenOptions::new().read(true).write(true).open(vars)?;
        if code.len() != CODE_SIZE || vars.metadata()?.len() != VARS_SIZE as u64 {
            return Err(DeviceError::Other(
                "OVMF requires 0x37c000-byte CODE and 0x84000-byte VARS".into(),
            ));
        }
        let mut bytes = vec![0; SIZE];
        vars.read_exact_at(&mut bytes[..VARS_SIZE], 0)?;
        bytes[VARS_SIZE..].copy_from_slice(&code);
        Ok(Self {
            bytes,
            vars,
            secure,
            mode: Mode::Array,
            status: READY,
            erase_block: 0,
        })
    }

    /// Intel commands use the low byte of a byte/word/dword bus transaction.
    /// Persistence completes before ready status is returned. On an I/O failure
    /// the error is returned to the VMM and latched in the flash status.
    pub fn write_access(
        &mut self,
        offset: u64,
        data: &[u8],
        in_smm: bool,
    ) -> Result<(), DeviceError> {
        let Some(end) = (offset as usize).checked_add(data.len()) else {
            return Ok(());
        };
        if data.is_empty() || end > SIZE || !matches!(data.len(), 1 | 2 | 4) {
            return Ok(());
        }
        let start = offset as usize;
        if self.secure && !in_smm {
            self.status |= PROGRAM_ERROR | PROTECTED;
            return Ok(());
        }
        match self.mode {
            Mode::Program => {
                self.mode = Mode::Status;
                self.status |= READY;
                if end > VARS_SIZE {
                    self.status |= PROGRAM_ERROR | PROTECTED;
                } else if self.bytes[start..end]
                    .iter()
                    .zip(data)
                    .any(|(old, new)| old & new != *new)
                {
                    self.status |= PROGRAM_ERROR;
                } else {
                    self.persist(start, data.to_vec(), PROGRAM_ERROR)?;
                }
            }
            Mode::Erase => {
                self.mode = Mode::Status;
                self.status |= READY;
                if data[0] != 0xd0 || start / BLOCK_SIZE != self.erase_block / BLOCK_SIZE {
                    self.status |= ERASE_ERROR | PROGRAM_ERROR;
                } else if self.erase_block >= VARS_SIZE {
                    self.status |= ERASE_ERROR | PROTECTED;
                } else {
                    self.persist(self.erase_block, vec![0xff; BLOCK_SIZE], ERASE_ERROR)?;
                }
            }
            _ => match data[0] {
                0xff => self.mode = Mode::Array,
                0x70 => self.mode = Mode::Status,
                0x50 => {
                    self.status = 0;
                }
                0x10 | 0x40 => self.mode = Mode::Program,
                0x20 => {
                    self.erase_block = start / BLOCK_SIZE * BLOCK_SIZE;
                    self.mode = Mode::Erase;
                }
                0x90 => self.mode = Mode::Id,
                0x98 => self.mode = Mode::Query,
                _ => {
                    self.status |= PROGRAM_ERROR;
                    self.mode = Mode::Status;
                }
            },
        }
        Ok(())
    }

    fn persist(&mut self, start: usize, data: Vec<u8>, error: u8) -> Result<(), DeviceError> {
        // Callers must stop on a host persistence error. A failed write or
        // sync may leave partially changed file contents without durability.
        if let Err(e) = self
            .vars
            .write_all_at(&data, start as u64)
            .and_then(|()| self.vars.sync_data())
        {
            self.status |= error;
            return Err(e.into());
        }
        self.bytes[start..start + data.len()].copy_from_slice(&data);
        Ok(())
    }

    fn query(offset: usize) -> u8 {
        match offset {
            0x10 => b'Q',
            0x11 => b'R',
            0x12 => b'Y',
            0x13 => 1, // Intel/Sharp extended command set.
            0x1b => 0x27,
            0x1c => 0x36,
            0x1f | 0x20 => 4,
            0x21 => 10,
            0x27 => 22, // 2^22 bytes, x8 interface.
            0x2c => 1,  // One erase region: 1024 blocks of 4096 bytes.
            0x2d => 0xff,
            0x2e => 3,
            0x2f => 0x10,
            _ => 0,
        }
    }
}

impl BusDevice for Pflash {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (index, byte) in data.iter_mut().enumerate() {
            let address = offset.checked_add(index as u64);
            *byte = match address.filter(|a| *a < SIZE as u64) {
                None => 0xff,
                Some(address) => match self.mode {
                    Mode::Array => self.bytes[address as usize],
                    Mode::Id => match address & 0xff {
                        0 => 0x89,
                        1 => 0x18,
                        _ => 0,
                    },
                    Mode::Query => Self::query(address as usize & 0xff),
                    _ => self.status,
                },
            };
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if let Err(e) = self.write_access(offset, data, false) {
            eprintln!("pflash persistence: {e}");
        }
    }
}

impl Snapshot for Pflash {
    fn state_id(&self) -> String {
        "ovmf-pflash".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"PFL1".to_vec();
        state.extend([self.secure as u8, self.mode as u8, self.status, 0]);
        state.extend((self.erase_block as u64).to_le_bytes());
        state.extend(&self.bytes[..VARS_SIZE]);
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 16 + VARS_SIZE
            || &state[..4] != b"PFL1"
            || state[4] != self.secure as u8
            || state[7] != 0
        {
            return Err(DeviceError::State("incompatible pflash state".into()));
        }
        let mode = match state[5] {
            0 => Mode::Array,
            1 => Mode::Status,
            2 => Mode::Id,
            3 => Mode::Query,
            4 => Mode::Program,
            5 => Mode::Erase,
            _ => return Err(DeviceError::State("invalid pflash mode".into())),
        };
        let block = u64::from_le_bytes(state[8..16].try_into().unwrap());
        if block >= SIZE as u64 || !block.is_multiple_of(BLOCK_SIZE as u64) {
            return Err(DeviceError::State("invalid pflash erase block".into()));
        }
        self.persist(0, state[16..].to_vec(), PROGRAM_ERROR)?;
        self.mode = mode;
        self.status = state[6];
        self.erase_block = block as usize;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

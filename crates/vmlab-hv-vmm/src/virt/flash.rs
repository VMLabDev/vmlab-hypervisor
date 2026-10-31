//! Intel CFI command-set flash, two 16-bit devices on a 32-bit bank.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vmlab_hv_devices::{BusDevice, DeviceError, Resettable, Snapshot};

const SIZE: usize = 32 << 20;
const BLOCK: usize = 256 << 10;

#[derive(Clone, Serialize, Deserialize)]
enum Command {
    Array,
    Status,
    Id,
    Query,
    Program,
    Erase(usize),
    BufferCount,
    Buffer {
        remaining: usize,
        writes: Vec<(usize, Vec<u8>)>,
    },
    Confirm(Vec<(usize, Vec<u8>)>),
}

#[derive(Serialize, Deserialize)]
struct State {
    data: Vec<u8>,
    command: Command,
    status: u8,
}

pub struct Flash {
    id: String,
    file: Option<File>,
    state: State,
    generation: u64,
}

impl Flash {
    pub fn open(id: &str, path: &Path, writable: bool) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if file.metadata().map_err(|e| e.to_string())?.len() != SIZE as u64 {
            return Err(format!("{} must be exactly 32 MiB", path.display()));
        }
        let mut data = vec![0; SIZE];
        file.read_exact_at(&mut data, 0)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            id: id.into(),
            file: writable.then_some(file),
            state: State {
                data,
                command: Command::Array,
                status: 0x80,
            },
            generation: 0,
        })
    }

    pub(super) fn code_generation(&self) -> Option<u64> {
        matches!(self.state.command, Command::Array).then_some(self.generation)
    }

    fn program(&mut self, offset: usize, bytes: &[u8]) {
        if self.file.is_none() || offset.checked_add(bytes.len()).is_none_or(|end| end > SIZE) {
            self.state.status = 0x90;
            return;
        }
        for (old, new) in self.state.data[offset..offset + bytes.len()]
            .iter_mut()
            .zip(bytes)
        {
            *old &= new;
        }
        if self
            .file
            .as_ref()
            .unwrap()
            .write_all_at(
                &self.state.data[offset..offset + bytes.len()],
                offset as u64,
            )
            .is_err()
        {
            self.state.status = 0x90;
        }
    }

    fn query(word: usize) -> u8 {
        match word {
            0x10 => b'Q',
            0x11 => b'R',
            0x12 => b'Y',
            0x13 => 1,
            0x15 => 0x31,
            0x1b => 0x27,
            0x1c => 0x36,
            0x1f | 0x20 => 7,
            0x21 => 10,
            0x23..=0x25 => 4,
            0x27 => 24,
            0x28 => 2,
            0x2a => 5,
            0x2c => 1,
            0x2d => 127,
            0x30 => 2,
            0x31 => b'P',
            0x32 => b'R',
            0x33 => b'I',
            0x34 => b'1',
            0x35 => b'3',
            _ => 0,
        }
    }
}

impl BusDevice for Flash {
    fn read(&mut self, offset: u64, bytes: &mut [u8]) {
        bytes.fill(0xff);
        let Ok(offset) = usize::try_from(offset) else {
            return;
        };
        if offset.checked_add(bytes.len()).is_none_or(|end| end > SIZE) {
            return;
        }
        for (index, byte) in bytes.iter_mut().enumerate() {
            let address = offset + index;
            *byte = match &self.state.command {
                Command::Array => self.state.data[address],
                Command::Query => {
                    if address & 1 == 0 {
                        Self::query(address / 4)
                    } else {
                        0
                    }
                }
                Command::Id => {
                    if address & 1 != 0 {
                        0
                    } else {
                        match address / 4 {
                            0 => 0x89,
                            1 => 0x18,
                            _ => 0,
                        }
                    }
                }
                _ => {
                    if address & 1 == 0 {
                        self.state.status
                    } else {
                        0
                    }
                }
            };
        }
    }

    fn write(&mut self, offset: u64, bytes: &[u8]) {
        if bytes.is_empty() || bytes.len() > 8 {
            return;
        }
        let Ok(offset) = usize::try_from(offset) else {
            return;
        };
        if offset.checked_add(bytes.len()).is_none_or(|end| end > SIZE) {
            return;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("flash generation exhausted");
        let command = bytes[0];
        let previous = std::mem::replace(&mut self.state.command, Command::Status);
        match previous {
            Command::Program => self.program(offset, bytes),
            Command::Erase(block) if command == 0xd0 => {
                if let Some(file) = &self.file {
                    self.state.data[block..block + BLOCK].fill(0xff);
                    if file
                        .write_all_at(&self.state.data[block..block + BLOCK], block as u64)
                        .is_err()
                    {
                        self.state.status = 0xa0;
                    }
                } else {
                    self.state.status = 0xa0;
                }
            }
            Command::BufferCount => {
                self.state.command = Command::Buffer {
                    remaining: command as usize + 1,
                    writes: Vec::new(),
                }
            }
            Command::Buffer {
                mut remaining,
                mut writes,
            } => {
                writes.push((offset, bytes.to_vec()));
                remaining -= 1;
                self.state.command = if remaining == 0 {
                    Command::Confirm(writes)
                } else {
                    Command::Buffer { remaining, writes }
                };
            }
            Command::Confirm(writes) if command == 0xd0 => {
                for (offset, bytes) in writes {
                    self.program(offset, &bytes);
                }
            }
            _ => {
                self.state.command = match command {
                    0xff | 0xf0 => Command::Array,
                    0x70 => Command::Status,
                    0x50 => {
                        self.state.status = 0x80;
                        Command::Status
                    }
                    0x90 => Command::Id,
                    0x98 => Command::Query,
                    0x10 | 0x40 => Command::Program,
                    0x20 => Command::Erase(offset / BLOCK * BLOCK),
                    0xe8 => Command::BufferCount,
                    _ => {
                        self.state.status = 0xb0;
                        Command::Status
                    }
                }
            }
        }
    }
}

impl Resettable for Flash {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.state.command = Command::Array;
        self.state.status = 0x80;
        self.generation = self
            .generation
            .checked_add(1)
            .expect("flash generation exhausted");
        Ok(())
    }
}

impl Snapshot for Flash {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&self.state).map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let state: State =
            serde_json::from_slice(bytes).map_err(|e| DeviceError::State(e.to_string()))?;
        if state.data.len() != SIZE {
            return Err(DeviceError::State("flash size mismatch".into()));
        }
        let valid_writes = |writes: &[(usize, Vec<u8>)]| {
            writes.len() <= 256
                && writes.iter().all(|(offset, bytes)| {
                    !bytes.is_empty()
                        && bytes.len() <= 8
                        && offset
                            .checked_add(bytes.len())
                            .is_some_and(|end| end <= SIZE)
                })
        };
        let valid = match &state.command {
            Command::Erase(block) => *block < SIZE && block.is_multiple_of(BLOCK),
            Command::Buffer { remaining, writes } => {
                *remaining > 0
                    && *remaining <= 256
                    && valid_writes(writes)
                    && remaining + writes.len() <= 256
            }
            Command::Confirm(writes) => !writes.is_empty() && valid_writes(writes),
            _ => true,
        };
        if !valid || (self.file.is_none() && state.data != self.state.data) {
            return Err(DeviceError::State("incompatible flash state".into()));
        }
        if let Some(file) = &self.file {
            file.write_all_at(&state.data, 0)?;
            file.sync_data()?;
        }
        self.state = state;
        self.generation = self
            .generation
            .checked_add(1)
            .expect("flash generation exhausted");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device(writable: bool) -> Flash {
        let file = tempfile::tempfile().unwrap();
        file.set_len(SIZE as u64).unwrap();
        Flash {
            id: "flash".into(),
            file: writable.then_some(file),
            state: State {
                data: vec![0xff; SIZE],
                command: Command::Array,
                status: 0x80,
            },
            generation: 0,
        }
    }
    fn word(flash: &mut Flash, offset: u64) -> u32 {
        let mut bytes = [0; 4];
        flash.read(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    }
    #[test]
    fn query_program_buffer_and_erase() {
        let mut flash = device(true);
        flash.write(0, &0x9898u32.to_le_bytes());
        assert_eq!(word(&mut flash, 0x40), 0x0051_0051);
        flash.write(0, &[0xff]);
        flash.write(0, &[0x40]);
        flash.write(0x100, &0x12345678u32.to_le_bytes());
        flash.write(0, &[0xff]);
        assert_eq!(word(&mut flash, 0x100), 0x12345678);
        flash.write(0x104, &[0xe8]);
        flash.write(0x104, &[1]);
        flash.write(0x104, &[1, 2, 3, 4]);
        flash.write(0x108, &[5, 6, 7, 8]);
        flash.write(0x104, &[0xd0]);
        flash.write(0, &[0xff]);
        assert_eq!(word(&mut flash, 0x104), 0x04030201);
        assert_eq!(word(&mut flash, 0x108), 0x08070605);
        flash.write(0x100, &[0x20]);
        flash.write(0x100, &[0xd0]);
        flash.write(0, &[0xff]);
        assert_eq!(word(&mut flash, 0x104), u32::MAX);
    }
    #[test]
    fn reset_preserves_programmed_vars_and_returns_to_array_mode() {
        let mut flash = device(true);
        flash.write(0, &[0x40]);
        flash.write(0x100, &0x12345678u32.to_le_bytes());
        flash.write(0, &[0x98]);
        assert!(flash.code_generation().is_none());
        flash.reset().unwrap();
        assert_eq!(word(&mut flash, 0x100), 0x12345678);
        assert!(flash.code_generation().is_some());
        flash.write(0, &[0x70]);
        assert_eq!(word(&mut flash, 0), 0x0080_0080);
    }

    #[test]
    fn read_only_rejects_program_and_snapshot_restores_command() {
        let mut flash = device(false);
        flash.write(0, &[0x40]);
        flash.write(0, &[0]);
        assert_eq!(word(&mut flash, 0), 0x0090_0090);
        flash.write(0, &[0x98]);
        let state = flash.save_state().unwrap();
        flash.write(0, &[0xff]);
        flash.restore_state(&state).unwrap();
        assert_eq!(word(&mut flash, 0x40), 0x0051_0051);
    }
}

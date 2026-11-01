//! ISA SMC byte protocol, as used by the public Linux applesmc driver.

use crate::{BusDevice, DeviceError, Resettable, Snapshot};
use serde::{Deserialize, Serialize};

const KEYS: &[[u8; 4]] = &[*b"#KEY", *b"MSSD", *b"NATJ", *b"OSK0", *b"OSK1"];

#[derive(Default, Serialize, Deserialize)]
struct State {
    command: u8,
    key: [u8; 4],
    argument: usize,
    length: u8,
    position: usize,
    status: u8,
    error: u8,
    values: [u8; 2],
}

pub struct AppleSmc {
    osk: [u8; 64],
    state: State,
}

impl AppleSmc {
    pub fn new(osk: &str) -> Result<Self, DeviceError> {
        if osk.len() != 64 || !osk.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            return Err(DeviceError::Other(
                "applesmc requires exactly 64 printable ASCII characters".into(),
            ));
        }
        Ok(Self {
            osk: osk.as_bytes().try_into().expect("validated length"),
            state: State {
                values: [3, 0],
                ..Default::default()
            },
        })
    }

    fn key_info(key: &[u8; 4]) -> Option<(u8, [u8; 4], u8)> {
        Some(match key {
            b"#KEY" => (4, *b"ui32", 0x80),
            b"OSK0" | b"OSK1" => (32, *b"ch8*", 0x80),
            key if KEYS.contains(key) => (1, *b"ui8 ", 0xc0),
            _ => return None,
        })
    }

    fn output(&self) -> u8 {
        let s = &self.state;
        match s.command {
            0x10 => match &s.key {
                b"OSK0" => self.osk.get(s.position).copied().unwrap_or(0),
                b"OSK1" => self.osk.get(32 + s.position).copied().unwrap_or(0),
                b"#KEY" => (KEYS.len() as u32)
                    .to_be_bytes()
                    .get(s.position)
                    .copied()
                    .unwrap_or(0),
                key => KEYS
                    .iter()
                    .filter(|key| !matches!(*key, b"#KEY" | b"OSK0" | b"OSK1"))
                    .position(|candidate| candidate == key)
                    .filter(|_| s.position == 0)
                    .map_or(0, |index| s.values[index]),
            },
            0x12 => KEYS
                .get(u32::from_be_bytes(s.key) as usize)
                .and_then(|key| key.get(s.position))
                .copied()
                .unwrap_or(0),
            0x13 => Self::key_info(&s.key).map_or(0, |(length, kind, flags)| {
                [length, kind[0], kind[1], kind[2], kind[3], flags]
                    .get(s.position)
                    .copied()
                    .unwrap_or(0)
            }),
            _ => 0,
        }
    }

    fn data_write(&mut self, value: u8) {
        let s = &mut self.state;
        if s.status == 0 || s.status & 1 != 0 {
            return;
        }
        if s.argument < 4 {
            s.key[s.argument] = value;
            s.argument += 1;
            s.status = 4;
            if s.argument == 4 && matches!(s.command, 0x12 | 0x13) {
                s.length = if s.command == 0x12 { 4 } else { 6 };
                let valid = if s.command == 0x12 {
                    (u32::from_be_bytes(s.key) as usize) < KEYS.len()
                } else {
                    Self::key_info(&s.key).is_some()
                };
                if valid {
                    s.status = 5;
                } else {
                    s.error = 0x84;
                    s.status = 0;
                }
            }
        } else if s.argument == 4 {
            s.argument += 1;
            s.length = value;
            let valid = if s.command == 0x12 {
                (u32::from_be_bytes(s.key) as usize) < KEYS.len() && value == 4
            } else {
                Self::key_info(&s.key).is_some_and(|(length, _, flags)| {
                    if s.command == 0x13 {
                        value == 6
                    } else {
                        value == length && (s.command != 0x11 || flags & 0x40 != 0)
                    }
                })
            };
            if !valid {
                s.error = 0x84;
                s.status = 0;
            } else {
                s.status = if s.command == 0x11 { 4 } else { 5 };
            }
        } else if s.command == 0x11 {
            if let Some(index) = KEYS
                .iter()
                .filter(|key| !matches!(*key, b"#KEY" | b"OSK0" | b"OSK1"))
                .position(|key| *key == s.key)
            {
                s.values[index] = value;
            }
            s.position += 1;
            if s.position == usize::from(s.length) {
                s.status = 0;
            }
        }
    }
}

impl BusDevice for AppleSmc {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for byte in data {
            *byte = match offset {
                0 if self.state.status & 1 != 0 => {
                    let value = self.output();
                    self.state.position += 1;
                    if self.state.position == usize::from(self.state.length) {
                        self.state.status = 0;
                    }
                    value
                }
                4 => self.state.status,
                0x1e => std::mem::take(&mut self.state.error),
                _ => 0,
            };
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        for &value in data {
            match offset {
                4 => {
                    let values = self.state.values;
                    self.state = State {
                        command: value,
                        status: 0x0c,
                        values,
                        ..Default::default()
                    };
                    if !(0x10..=0x13).contains(&value) {
                        self.state.error = 0x82;
                        self.state.status = 0;
                    }
                }
                0 => self.data_write(value),
                _ => {}
            }
        }
    }
}

impl Resettable for AppleSmc {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.state = State {
            values: [3, 0],
            ..Default::default()
        };
        Ok(())
    }
}

impl Snapshot for AppleSmc {
    fn state_id(&self) -> String {
        "applesmc".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        // The cursor is sufficient to resume an OSK read. Never save its bytes.
        serde_json::to_vec(&self.state).map_err(|_| DeviceError::State("encoding SMC state".into()))
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let state: State = serde_json::from_slice(bytes)
            .map_err(|_| DeviceError::State("invalid SMC state".into()))?;
        if state.argument > 5
            || state.position > 32
            || state.length > 32
            || !matches!(state.status, 0 | 4 | 5 | 0x0c)
        {
            return Err(DeviceError::State("invalid SMC state".into()));
        }
        self.state = state;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn read(device: &mut AppleSmc, port: u64) -> u8 {
        let mut data = [0];
        device.read(port, &mut data);
        data[0]
    }
    fn request(device: &mut AppleSmc, command: u8, key: &[u8; 4], length: u8) {
        device.write(4, &[command]);
        assert_eq!(read(device, 4), 0x0c);
        for byte in key {
            device.write(0, &[*byte]);
        }
        device.write(0, &[length]);
    }
    #[test]
    fn fixed_size_commands_reply_without_a_length_byte() {
        let mut smc = AppleSmc::new(&"FAKE".repeat(16)).unwrap();
        for (command, key, length) in [(0x13, *b"MSSD", 6), (0x12, 0u32.to_be_bytes(), 4)] {
            smc.write(4, &[command]);
            for byte in key {
                smc.write(0, &[byte]);
            }
            assert_eq!(
                read(&mut smc, 4),
                5,
                "fixed-size command needs no length byte"
            );
            let saved = smc.save_state().unwrap();
            smc.reset().unwrap();
            smc.restore_state(&saved).unwrap();
            for _ in 0..length {
                read(&mut smc, 0);
            }
            assert_eq!(read(&mut smc, 4), 0);
        }
        smc.write(4, &[0x13]);
        smc.write(0, b"????");
        assert_eq!(read(&mut smc, 4), 0);
        assert_eq!(read(&mut smc, 0x1e), 0x84);
    }

    #[test]
    fn read_command_status_sequence_matches_qemu() {
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/hwref/q35-macos-tahoe/smc-e820.json"
        ))
        .unwrap();
        let mut smc = AppleSmc::new(&"FAKE".repeat(16)).unwrap();
        for key in [b"MSSD", b"NATJ", b"BEMB", b"MSSW", b"OSWD"] {
            smc.write(4, &[0x10]);
            let saved = smc.save_state().unwrap();
            smc.reset().unwrap();
            smc.restore_state(&saved).unwrap();
            let mut sequence = vec![read(&mut smc, 4)];
            for byte in key {
                smc.write(0, &[*byte]);
                sequence.push(read(&mut smc, 4));
            }
            smc.write(0, &[1]);
            sequence.push(read(&mut smc, 4));
            let expected = &capture["keys"][std::str::from_utf8(key).unwrap()];
            assert_eq!(serde_json::json!(sequence), expected["sequence"]);
            for _ in expected["data"].as_array().unwrap() {
                read(&mut smc, 0);
            }
            assert_eq!(
                u64::from(read(&mut smc, 4)),
                expected["completed"].as_u64().unwrap()
            );
        }
    }

    #[test]
    fn nonsecret_reads_match_qemu_and_osk_restore_uses_the_new_description() {
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/hwref/q35-macos-tahoe/smc-e820.json"
        ))
        .unwrap();
        let mut device = AppleSmc::new(&"FAKE".repeat(16)).unwrap();
        for key in [b"MSSD", b"NATJ", b"BEMB", b"MSSW", b"OSWD"] {
            request(&mut device, 0x10, key, 1);
            let expected = &capture["keys"][std::str::from_utf8(key).unwrap()];
            assert_eq!(
                u64::from(read(&mut device, 4)),
                expected["status"].as_u64().unwrap()
            );
            for value in expected["data"].as_array().unwrap() {
                assert_eq!(u64::from(read(&mut device, 0)), value.as_u64().unwrap());
            }
            assert_eq!(
                u64::from(read(&mut device, 0x1e)),
                expected["error"].as_u64().unwrap()
            );
        }
        request(&mut device, 0x10, b"OSK0", 32);
        read(&mut device, 0);
        let saved = device.save_state().unwrap();
        let next = "NEXT".repeat(16);
        let mut restored = AppleSmc::new(&next).unwrap();
        restored.restore_state(&saved).unwrap();
        assert_eq!(read(&mut restored, 0), next.as_bytes()[1]);
    }
    #[test]
    fn key_reads_types_indices_and_snapshot_never_contain_secret_bytes() {
        let key = format!("{}{}", "FAKE".repeat(8), "TEST".repeat(8));
        let mut smc = AppleSmc::new(&key).unwrap();
        for (name, offset) in [(b"OSK0", 0), (b"OSK1", 32)] {
            request(&mut smc, 0x10, name, 32);
            for index in 0..32 {
                assert_eq!(read(&mut smc, 4), 5);
                assert_eq!(read(&mut smc, 0), key.as_bytes()[offset + index]);
                let saved = smc.save_state().unwrap();
                assert!(!saved.windows(8).any(|bytes| bytes == &key.as_bytes()[..8]));
                smc.restore_state(&saved).unwrap();
            }
            assert_eq!(read(&mut smc, 4), 0);
        }
        for (index, name) in KEYS.iter().enumerate() {
            request(&mut smc, 0x12, &(index as u32).to_be_bytes(), 4);
            assert_eq!((0..4).map(|_| read(&mut smc, 0)).collect::<Vec<_>>(), name);
            request(&mut smc, 0x13, name, 6);
            let info: Vec<_> = (0..6).map(|_| read(&mut smc, 0)).collect();
            assert_eq!(
                info[0],
                if name.starts_with(b"OSK") {
                    32
                } else if name == b"#KEY" {
                    4
                } else {
                    1
                }
            );
        }
        for name in [b"????", b"BEMB", b"MSSW", b"OSWD"] {
            request(&mut smc, 0x10, name, 1);
            assert_eq!(read(&mut smc, 4), 0);
            assert_eq!(read(&mut smc, 0x1e), 0x84);
        }
        request(&mut smc, 0x10, b"MSSD", 1);
        assert_eq!(read(&mut smc, 0), 3);
        request(&mut smc, 0x10, b"NATJ", 1);
        assert_eq!(read(&mut smc, 0), 0);
    }

    #[test]
    fn writes_and_partial_transactions_restore_without_overwriting_read_only_keys() {
        let mut smc = AppleSmc::new(&"FAKE".repeat(16)).unwrap();
        request(&mut smc, 0x11, b"MSSD", 1);
        assert_eq!(read(&mut smc, 4), 4);
        let pending = smc.save_state().unwrap();
        smc.write(0, &[7]);
        request(&mut smc, 0x10, b"MSSD", 1);
        assert_eq!(read(&mut smc, 0), 7);
        smc.restore_state(&pending).unwrap();
        smc.write(0, &[9]);
        assert_eq!(read(&mut smc, 4), 0);
        request(&mut smc, 0x10, b"MSSD", 1);
        assert_eq!(read(&mut smc, 0), 9);
        request(&mut smc, 0x11, b"OSK0", 32);
        assert_eq!(read(&mut smc, 4), 0);
        assert_eq!(read(&mut smc, 0x1e), 0x84);
        assert_eq!(read(&mut smc, 0x1e), 0);
        smc.write(4, &[0xff]);
        assert_eq!(read(&mut smc, 4), 0);
        assert_eq!(read(&mut smc, 0x1e), 0x82);
        smc.reset().unwrap();
        request(&mut smc, 0x10, b"MSSD", 1);
        assert_eq!(read(&mut smc, 0), 3);
    }
}

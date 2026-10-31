//! HID 1.11 boot keyboard, eight-byte reports and six-key rollover.

use super::{
    SetupPacket, Tablet, UsbDevice, UsbSpeed, UsbTransfer,
    state::{Reader, Writer, invalid},
};
use crate::{DeviceError, Snapshot};
use std::{
    collections::{BTreeSet, VecDeque},
    time::{Duration, Instant},
};

pub const KEYBOARD_PORT: u8 = 5;
pub const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x75, 0x01, 0x95, 0x08, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7,
    0x15, 0x00, 0x25, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0xff, 0x05, 0x07, 0x19, 0x00, 0x29, 0xff, 0x81, 0x00, 0xc0,
];
const CONFIG: &[u8] = &[
    9, 2, 34, 0, 1, 1, 8, 0xa0, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0,
    7, 5, 0x81, 3, 8, 0, 10,
];

pub struct Keyboard {
    control: Tablet,
    keys: BTreeSet<u8>,
    reports: VecDeque<[u8; 8]>,
    current: [u8; 8],
    leds: u8,
}

impl Keyboard {
    pub fn new(serial: impl Into<String>) -> Self {
        Self {
            control: Tablet::new(serial),
            keys: BTreeSet::new(),
            reports: VecDeque::new(),
            current: [0; 8],
            leds: 0,
        }
    }
    fn report(&self) -> [u8; 8] {
        let mut report = [0; 8];
        for key in self.keys.iter().filter(|key| **key >= 0xe0) {
            report[0] |= 1 << (key - 0xe0);
        }
        let keys: Vec<_> = self
            .keys
            .iter()
            .filter(|key| **key < 0xe0)
            .copied()
            .collect();
        if keys.len() > 6 {
            report[2..].fill(1);
        } else {
            report[2..2 + keys.len()].copy_from_slice(&keys);
        }
        report
    }
}

fn usage(qcode: &str) -> Option<u8> {
    if qcode.len() == 1 {
        let byte = qcode.as_bytes()[0];
        if byte.is_ascii_lowercase() {
            return Some(byte - b'a' + 4);
        }
        if (b'1'..=b'9').contains(&byte) {
            return Some(byte - b'1' + 30);
        }
    }
    Some(match qcode {
        "0" => 39,
        "ret" => 40,
        "esc" => 41,
        "backspace" => 42,
        "tab" => 43,
        "spc" => 44,
        "minus" => 45,
        "equal" => 46,
        "bracket_left" => 47,
        "bracket_right" => 48,
        "backslash" => 49,
        "semicolon" => 51,
        "apostrophe" => 52,
        "grave_accent" => 53,
        "comma" => 54,
        "dot" => 55,
        "slash" => 56,
        "caps_lock" => 57,
        "print" => 70,
        "scroll_lock" => 71,
        "pause" => 72,
        "insert" => 73,
        "home" => 74,
        "pgup" => 75,
        "delete" => 76,
        "end" => 77,
        "pgdn" => 78,
        "right" => 79,
        "left" => 80,
        "down" => 81,
        "up" => 82,
        "num_lock" => 83,
        "kp_divide" => 84,
        "kp_multiply" => 85,
        "kp_subtract" => 86,
        "kp_add" => 87,
        "kp_enter" => 88,
        "kp_0" => 98,
        "kp_decimal" => 99,
        "ctrl" => 0xe0,
        "shift" => 0xe1,
        "alt" => 0xe2,
        "meta_l" => 0xe3,
        "ctrl_r" => 0xe4,
        "shift_r" => 0xe5,
        "alt_r" => 0xe6,
        "meta_r" => 0xe7,
        "menu" => 101,
        _ => {
            if let Some(n) = qcode.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                && (1..=12).contains(&n)
            {
                return Some(57 + n);
            }
            if let Some(n) = qcode.strip_prefix("kp_").and_then(|n| n.parse::<u8>().ok())
                && (1..=9).contains(&n)
            {
                return Some(88 + n);
            }
            return None;
        }
    })
}

impl UsbDevice for Keyboard {
    fn speed(&self) -> UsbSpeed {
        UsbSpeed::High
    }
    fn reset(&mut self) {
        self.control.reset();
        self.keys.clear();
        self.reports.clear();
        self.current = [0; 8];
        self.leds = 0;
    }
    fn control(&mut self, s: SetupPacket, data: &[u8]) -> UsbTransfer {
        let bytes = match (s.request_type, s.request, s.value) {
            (0x80 | 0x81, 6, 0x100) => {
                vec![18, 1, 0, 2, 0, 0, 0, 64, 0x27, 6, 1, 0, 0, 0, 1, 4, 11, 1]
            }
            (0x80 | 0x81, 6, 0x200) => CONFIG.to_vec(),
            (0x80 | 0x81, 6, 0x700) => {
                let mut bytes = CONFIG.to_vec();
                bytes[1] = 7;
                bytes[33] = 10;
                bytes
            }
            (0x80 | 0x81, 6, 0x2100) => CONFIG[18..27].to_vec(),
            (0x80 | 0x81, 6, 0x2200) => KEYBOARD_REPORT_DESCRIPTOR.to_vec(),
            (0xa1, 1, 0x100) => self.current.to_vec(),
            (0xa1, 1, 0x200) => vec![self.leds],
            (0x21, 9, 0x200) if s.index == 0 && data.len() == 1 => {
                self.leds = data[0] & 31;
                vec![]
            }
            _ => return self.control.control(s, data),
        };
        UsbTransfer::Ack(bytes.into_iter().take(s.length as usize).collect())
    }
    fn transfer(&mut self, endpoint: u8, _: &[u8], max_length: usize) -> UsbTransfer {
        if endpoint != 0x81 || self.control.halted {
            return UsbTransfer::Stall;
        }
        if self.control.configuration == 0 {
            return UsbTransfer::Nak;
        }
        if let Some(report) = self.reports.pop_front() {
            self.current = report;
        } else if self.control.idle == 0
            || self.control.idle_elapsed + self.control.clock.elapsed()
                < Duration::from_millis(u64::from(self.control.idle) * 4)
        {
            return UsbTransfer::Nak;
        }
        self.control.clock = Instant::now();
        self.control.idle_elapsed = Duration::ZERO;
        UsbTransfer::Ack(self.current[..max_length.min(8)].to_vec())
    }
    fn inject_key(&mut self, qcode: &str, down: bool) -> Result<(), DeviceError> {
        let key = usage(qcode)
            .ok_or_else(|| DeviceError::Other(format!("unsupported USB qcode: {qcode}")))?;
        if self.reports.len() >= 1024 {
            return Err(DeviceError::Other("USB keyboard input queue full".into()));
        }
        let changed = if down {
            self.keys.insert(key)
        } else {
            self.keys.remove(&key)
        };
        if changed {
            self.reports.push_back(self.report());
        }
        Ok(())
    }
}

impl Snapshot for Keyboard {
    fn state_id(&self) -> String {
        "usb-keyboard".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut w = Writer(b"VHVKBD01".to_vec());
        w.bytes(&self.control.save_state()?);
        w.bytes(&self.keys.iter().copied().collect::<Vec<_>>());
        w.bytes(&self.current);
        w.u64(u64::from(self.leds));
        w.u64(self.reports.len() as u64);
        for report in &self.reports {
            w.bytes(report);
        }
        Ok(w.0)
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let mut r = Reader(bytes);
        if r.take(8)? != b"VHVKBD01" {
            return Err(invalid());
        }
        let control = r.bytes(65536)?;
        let keys: BTreeSet<_> = r.bytes(256)?.iter().copied().collect();
        if keys.iter().any(|key| *key > 0xe7) {
            return Err(invalid());
        }
        let current = r.bytes(8)?.try_into().map_err(|_| invalid())?;
        let leds = u8::try_from(r.u64()?).map_err(|_| invalid())?;
        let count = r.u64()?;
        if count > 1024 {
            return Err(invalid());
        }
        let mut reports = VecDeque::new();
        for _ in 0..count {
            reports.push_back(r.bytes(8)?.try_into().map_err(|_| invalid())?);
        }
        r.end()?;
        self.control.restore_state(control)?;
        self.keys = keys;
        self.current = current;
        self.leds = leds;
        self.reports = reports;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_descriptor_matches_the_captured_guest_inventory() {
        let capture: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../../docs/hwref/q35-macos-tahoe/hid-exercise.json"
        ))
        .unwrap();
        let hex: String = KEYBOARD_REPORT_DESCRIPTOR
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let property = format!("\"ReportDescriptor\" = <{hex}>");
        assert!(capture.iter().any(|record| {
            record["stdout"]
                .as_str()
                .is_some_and(|stdout| stdout.contains(&property))
        }));
    }

    #[test]
    fn boot_reports_modifiers_rollover_and_snapshot_queue() {
        let mut keyboard = Keyboard::new("fake");
        assert_eq!(keyboard.transfer(0x81, &[], 8), UsbTransfer::Nak);
        keyboard.control(
            SetupPacket {
                request_type: 0,
                request: 9,
                value: 1,
                index: 0,
                length: 0,
            },
            &[],
        );
        keyboard.inject_key("shift", true).unwrap();
        keyboard.inject_key("a", true).unwrap();
        let saved = keyboard.save_state().unwrap();
        assert_eq!(
            keyboard.transfer(0x81, &[], 8),
            UsbTransfer::Ack(vec![2, 0, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            keyboard.transfer(0x81, &[], 8),
            UsbTransfer::Ack(vec![2, 0, 4, 0, 0, 0, 0, 0])
        );
        keyboard.restore_state(&saved).unwrap();
        assert_eq!(
            keyboard.transfer(0x81, &[], 8),
            UsbTransfer::Ack(vec![2, 0, 0, 0, 0, 0, 0, 0])
        );
        for key in ["b", "c", "d", "e", "f", "g"] {
            keyboard.inject_key(key, true).unwrap();
        }
        assert_eq!(keyboard.report()[2..], [1; 6]);
        keyboard.inject_key("a", false).unwrap();
        assert_ne!(keyboard.report()[2..], [1; 6]);
        assert_eq!(KEYBOARD_REPORT_DESCRIPTOR.len(), 63);
    }
}

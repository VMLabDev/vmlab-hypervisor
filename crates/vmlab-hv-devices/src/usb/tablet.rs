use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{DeviceError, Snapshot};

use super::{
    SetupPacket, UsbDevice, UsbSpeed, UsbTransfer,
    state::{Reader, Writer, invalid},
};

/// QEMU 11.1.2 usb-tablet HID report descriptor, captured over USB control
/// transfers. Five buttons, 16-bit absolute X/Y, and an 8-bit relative wheel.
pub const TABLET_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00,
    0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
];

const DEVICE: &[u8] = &[18, 1, 0, 2, 0, 0, 0, 64, 0x27, 6, 1, 0, 0, 0, 1, 3, 10, 1];
const CONFIG: &[u8] = &[
    9, 2, 34, 0, 1, 1, 7, 0xa0, 50, 9, 4, 0, 0, 1, 3, 0, 0, 0, 9, 0x21, 1, 0, 0, 1, 0x22, 74, 0, 7,
    5, 0x81, 3, 8, 0, 4,
];
const QUEUE_LIMIT: usize = 1024;

/// Buttons use HID order: bits 0..4 left, right, middle, back, forward. Positive
/// wheel values scroll up. Coordinates outside 0..32767 are clamped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerEvent {
    pub x: u16,
    pub y: u16,
    pub buttons: u8,
    pub wheel: i8,
}

impl PointerEvent {
    fn report(self) -> [u8; 6] {
        let x = self.x.min(32767).to_le_bytes();
        let y = self.y.min(32767).to_le_bytes();
        [
            self.buttons & 31,
            x[0],
            x[1],
            y[0],
            y[1],
            self.wheel.clamp(-127, 127) as u8,
        ]
    }
}

/// Built-in high-speed absolute-pointer device. `serial` is stable across
/// snapshots; the default matches QEMU at PCI 00:01.0, first USB connector.
pub struct Tablet {
    serial: String,
    configuration: u8,
    address: u8,
    idle: u8,
    protocol: u8,
    remote_wakeup: bool,
    halted: bool,
    current: [u8; 6],
    reports: VecDeque<[u8; 6]>,
    idle_elapsed: Duration,
    clock: Instant,
}

impl Default for Tablet {
    fn default() -> Self {
        Self::new("28754-0000:00:01.0-1")
    }
}

impl Tablet {
    pub fn new(serial: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            configuration: 0,
            address: 0,
            idle: 0,
            protocol: 1,
            remote_wakeup: false,
            halted: false,
            current: [0; 6],
            reports: VecDeque::new(),
            idle_elapsed: Duration::ZERO,
            clock: Instant::now(),
        }
    }
    fn descriptor(&self, kind: u8, index: u8) -> Option<Vec<u8>> {
        Some(match (kind, index) {
            (1, 0) => DEVICE.to_vec(),
            (2, 0) => CONFIG.to_vec(),
            (3, 0) => vec![4, 3, 9, 4],
            (3, 1..=10) => {
                let s = match index {
                    1 => "QEMU",
                    2 => "QEMU USB Mouse",
                    3 => "QEMU USB Tablet",
                    4 => "QEMU USB Keyboard",
                    5 => "42",
                    6 => "HID Mouse",
                    7 => "HID Tablet",
                    8 => "HID Keyboard",
                    9 => "89126",
                    _ => &self.serial,
                };
                let mut b = vec![0, 3];
                for c in s.encode_utf16().take(126) {
                    b.extend(c.to_le_bytes());
                }
                b[0] = b.len() as u8;
                b
            }
            (6, 0) => vec![10, 6, 0, 1, 0, 0, 0, 8, 1, 0],
            (7, 0) => {
                let mut b = CONFIG.to_vec();
                b[1] = 7;
                b[33] = 10;
                b
            }
            (15, 0) => vec![5, 15, 12, 0, 1, 7, 16, 2, 2, 0, 0, 0],
            (0x21, 0) => CONFIG[18..27].to_vec(),
            (0x22, 0) => TABLET_REPORT_DESCRIPTOR.to_vec(),
            _ => return None,
        })
    }
}

impl UsbDevice for Tablet {
    fn speed(&self) -> UsbSpeed {
        UsbSpeed::High
    }
    fn reset(&mut self) {
        self.configuration = 0;
        self.address = 0;
        self.idle = 0;
        self.protocol = 1;
        self.remote_wakeup = false;
        self.halted = false;
        self.reports.clear();
        self.idle_elapsed = Duration::ZERO;
        self.clock = Instant::now();
    }
    fn control(&mut self, s: SetupPacket, _data: &[u8]) -> UsbTransfer {
        let b = match (s.request_type, s.request) {
            (0x80 | 0x81, 6) => match self.descriptor((s.value >> 8) as u8, s.value as u8) {
                Some(b) => b,
                None => return UsbTransfer::Stall,
            },
            (0x80, 0) if s.value == 0 && s.index == 0 => vec![u8::from(self.remote_wakeup) << 1, 0],
            (0x81, 0) if s.index == 0 => vec![0, 0],
            (0x82, 0) if s.index == 0x81 => vec![u8::from(self.halted), 0],
            (0, 1 | 3) if s.value == 1 && s.index == 0 => {
                self.remote_wakeup = s.request == 3;
                vec![]
            }
            (2, 1 | 3) if s.value == 0 && s.index == 0x81 => {
                self.halted = s.request == 3;
                vec![]
            }
            (0, 5) if s.value < 128 => {
                self.address = s.value as u8;
                vec![]
            }
            (0x80, 8) => vec![self.configuration],
            (0, 9) if s.value <= 1 => {
                self.configuration = s.value as u8;
                vec![]
            }
            (0x81, 10) if s.index == 0 => vec![0],
            (1, 11) if s.index == 0 && s.value == 0 => vec![],
            (0xa1, 1) if s.value == 0x100 && s.index == 0 => self.current.to_vec(),
            (0xa1, 2) if s.index == 0 => vec![self.idle],
            (0x21, 10) if s.index == 0 && s.value & 255 == 0 => {
                self.idle = (s.value >> 8) as u8;
                self.idle_elapsed = Duration::ZERO;
                self.clock = Instant::now();
                vec![]
            }
            (0xa1, 3) if s.index == 0 => vec![self.protocol],
            (0x21, 11) if s.index == 0 && s.value <= 1 => {
                self.protocol = s.value as u8;
                vec![]
            }
            _ => return UsbTransfer::Stall,
        };
        UsbTransfer::Ack(b.into_iter().take(s.length as usize).collect())
    }
    fn transfer(&mut self, endpoint: u8, _data: &[u8], max_length: usize) -> UsbTransfer {
        if endpoint != 0x81 || self.halted {
            return UsbTransfer::Stall;
        }
        if self.configuration == 0 {
            return UsbTransfer::Nak;
        }
        match self.reports.pop_front() {
            Some(b) => {
                self.idle_elapsed = Duration::ZERO;
                self.clock = Instant::now();
                self.current = b;
                self.current[5] = 0;
                UsbTransfer::Ack(b[..max_length.min(6)].to_vec())
            }
            None if self.idle != 0
                && self.idle_elapsed + self.clock.elapsed()
                    >= Duration::from_millis(u64::from(self.idle) * 4) =>
            {
                self.idle_elapsed = Duration::ZERO;
                self.clock = Instant::now();
                UsbTransfer::Ack(self.current[..max_length.min(6)].to_vec())
            }
            None => UsbTransfer::Nak,
        }
    }
    fn inject_pointer(&mut self, event: PointerEvent) -> Result<(), DeviceError> {
        if self.reports.len() == QUEUE_LIMIT {
            return Err(DeviceError::Other("USB tablet input queue full".into()));
        }
        self.reports.push_back(event.report());
        Ok(())
    }
}

impl Snapshot for Tablet {
    fn state_id(&self) -> String {
        "usb-tablet".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut w = Writer(b"VHVTAB01".to_vec());
        w.bytes(self.serial.as_bytes());
        w.bytes(&[
            self.configuration,
            self.address,
            self.idle,
            self.protocol,
            u8::from(self.remote_wakeup),
            u8::from(self.halted),
        ]);
        w.bytes(&self.current);
        w.u64(self.reports.len() as u64);
        for b in &self.reports {
            w.bytes(b);
        }
        w.u64(if self.idle == 0 {
            0
        } else {
            (self.idle_elapsed + self.clock.elapsed())
                .as_micros()
                .min(1_020_000) as u64
        });
        Ok(w.0)
    }
    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let mut r = Reader(bytes);
        if r.take(8)? != b"VHVTAB01" {
            return Err(invalid());
        }
        let serial = std::str::from_utf8(r.bytes(4096)?)
            .map_err(|_| invalid())?
            .to_owned();
        let b = r.bytes(6)?;
        if b.len() != 6 || b[0] > 1 || b[1] > 127 || b[3] > 1 || b[4] > 1 || b[5] > 1 {
            return Err(invalid());
        }
        let current = r.bytes(6)?.try_into().map_err(|_| invalid())?;
        let count = r.u64()?;
        if count > QUEUE_LIMIT as u64 {
            return Err(invalid());
        }
        let mut reports = VecDeque::new();
        for _ in 0..count {
            reports.push_back(r.bytes(6)?.try_into().map_err(|_| invalid())?);
        }
        let idle_elapsed = Duration::from_micros(r.u64()?.min(1_020_000));
        r.end()?;
        *self = Self {
            serial,
            configuration: b[0],
            address: b[1],
            idle: b[2],
            protocol: b[3],
            remote_wakeup: b[4] != 0,
            halted: b[5] != 0,
            current,
            reports,
            idle_elapsed,
            clock: Instant::now(),
        };
        Ok(())
    }
}

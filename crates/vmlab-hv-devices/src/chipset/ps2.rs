//! i8042 controller, AT keyboard and PS/2 mouse at ports 0x60 and 0x64.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::{BusDevice, DeviceError, IrqLine, Snapshot};

/// Platform output lines controlled by the i8042 output port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerEvent {
    A20(bool),
    Reset,
}

/// A relative mouse movement. Button bits are left, right and middle.
#[derive(Debug, Clone, Copy, Default)]
pub struct PointerEvent {
    pub dx: i32,
    pub dy: i32,
    pub buttons: u8,
    pub wheel: i8,
}

pub struct I8042 {
    keyboard_irq: Arc<dyn IrqLine>,
    mouse_irq: Arc<dyn IrqLine>,
    output: Arc<dyn Fn(ControllerEvent) + Send + Sync>,
    queue: VecDeque<(u8, bool)>,
    command_byte: u8,
    output_port: u8,
    pending: u8,
    keyboard_pending: u8,
    keyboard_enabled: bool,
    scan_set: u8,
    leds: u8,
    typematic: u8,
    mouse_pending: u8,
    mouse_enabled: bool,
    mouse_remote: bool,
    mouse_wrap: bool,
    mouse_scaling: bool,
    mouse_resolution: u8,
    mouse_rate: u8,
    mouse_id: u8,
    rate_history: [u8; 3],
    buttons: u8,
    dx: i32,
    dy: i32,
    wheel: i8,
    last_keyboard: u8,
    last_mouse: u8,
    last_command: bool,
}

impl I8042 {
    /// Map `BusDevice` offsets 0 and 4 to ports 0x60 and 0x64.
    pub fn new(
        keyboard_irq: Arc<dyn IrqLine>,
        mouse_irq: Arc<dyn IrqLine>,
        output: Arc<dyn Fn(ControllerEvent) + Send + Sync>,
    ) -> Self {
        keyboard_irq.set_level(false);
        mouse_irq.set_level(false);
        Self {
            keyboard_irq,
            mouse_irq,
            output,
            queue: VecDeque::new(),
            command_byte: 0x03,
            output_port: 0x03,
            pending: 0,
            keyboard_pending: 0,
            keyboard_enabled: true,
            scan_set: 2,
            leds: 0,
            typematic: 0x2b,
            mouse_pending: 0,
            mouse_enabled: false,
            mouse_remote: false,
            mouse_wrap: false,
            mouse_scaling: false,
            mouse_resolution: 2,
            mouse_rate: 100,
            mouse_id: 0,
            rate_history: [0; 3],
            buttons: 0,
            dx: 0,
            dy: 0,
            wheel: 0,
            last_keyboard: 0,
            last_mouse: 0,
            last_command: false,
        }
    }

    pub fn leds(&self) -> u8 {
        self.leds
    }
    pub fn a20_enabled(&self) -> bool {
        self.output_port & 2 != 0
    }

    fn update_irq(&self) {
        let front = self.queue.front();
        self.keyboard_irq
            .set_level(front.is_some_and(|(_, mouse)| !mouse) && self.command_byte & 1 != 0);
        self.mouse_irq
            .set_level(front.is_some_and(|(_, mouse)| *mouse) && self.command_byte & 2 != 0);
    }

    fn enqueue(&mut self, bytes: &[u8], mouse: bool) {
        for &byte in bytes {
            if self.queue.len() < 4096 {
                self.queue.push_back((byte, mouse));
            }
            if mouse {
                self.last_mouse = byte;
            } else {
                self.last_keyboard = byte;
            }
        }
        self.update_irq();
    }

    /// Inject a QEMU qcode press or release. Unknown names return an error.
    pub fn inject_key(&mut self, qcode: &str, down: bool) -> Result<(), DeviceError> {
        if qcode == "pause" {
            if down && self.keyboard_enabled && self.command_byte & 0x10 == 0 {
                let bytes: &[u8] = if self.command_byte & 0x40 != 0 || self.scan_set == 1 {
                    &[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]
                } else {
                    &[0xe1, 0x14, 0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77]
                };
                self.enqueue(bytes, false);
            }
            return Ok(());
        }
        if qcode == "print" {
            if self.keyboard_enabled && self.command_byte & 0x10 == 0 {
                let translated = self.command_byte & 0x40 != 0 || self.scan_set == 1;
                let bytes: &[u8] = match (translated, down) {
                    (true, true) => &[0xe0, 0x2a, 0xe0, 0x37],
                    (true, false) => &[0xe0, 0xb7, 0xe0, 0xaa],
                    (false, true) => &[0xe0, 0x12, 0xe0, 0x7c],
                    (false, false) => &[0xe0, 0xf0, 0x7c, 0xe0, 0xf0, 0x12],
                };
                self.enqueue(bytes, false);
            }
            return Ok(());
        }
        let (set2, set1, extended) = key_code(qcode)
            .ok_or_else(|| DeviceError::Other(format!("unsupported PS/2 qcode: {qcode}")))?;
        if !self.keyboard_enabled || self.command_byte & 0x10 != 0 {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(3);
        if extended {
            bytes.push(0xe0);
        }
        if self.command_byte & 0x40 != 0 || self.scan_set == 1 {
            bytes.push(set1 | if down { 0 } else { 0x80 });
        } else {
            if !down {
                bytes.push(0xf0);
            }
            bytes.push(set2);
        }
        self.enqueue(&bytes, false);
        Ok(())
    }

    /// Positive `dy` moves down; PS/2 packets encode positive Y upwards.
    pub fn inject_pointer(&mut self, event: PointerEvent) {
        self.dx = self.dx.saturating_add(event.dx);
        self.dy = self.dy.saturating_sub(event.dy);
        self.wheel = self.wheel.saturating_add(event.wheel);
        self.buttons = event.buttons & 0x1f;
        if self.mouse_enabled && !self.mouse_remote && self.command_byte & 0x20 == 0 {
            self.mouse_packet();
        }
    }

    fn mouse_packet(&mut self) {
        let scale = |v: i32| -> i32 {
            let n = match v.unsigned_abs() {
                0 => 0,
                1 | 2 => 1,
                3 => 3,
                4 => 6,
                5 => 9,
                n => n.saturating_mul(2).min(i32::MAX as u32) as i32,
            };
            if v < 0 { -n } else { n }
        };
        let x = if self.mouse_scaling {
            scale(self.dx)
        } else {
            self.dx
        };
        let y = if self.mouse_scaling {
            scale(self.dy)
        } else {
            self.dy
        };
        let status = 8
            | (self.buttons & 7)
            | (u8::from(x < 0) << 4)
            | (u8::from(y < 0) << 5)
            | (u8::from(!(-256..=255).contains(&x)) << 6)
            | (u8::from(!(-256..=255).contains(&y)) << 7);
        let mut bytes = vec![status, x.clamp(-256, 255) as u8, y.clamp(-256, 255) as u8];
        if self.mouse_id == 3 {
            bytes.push(self.wheel as u8);
        }
        if self.mouse_id == 4 {
            bytes.push((self.wheel.clamp(-8, 7) as u8 & 15) | ((self.buttons & 0x18) << 1));
        }
        self.dx = 0;
        self.dy = 0;
        self.wheel = 0;
        self.enqueue(&bytes, true);
    }

    fn set_output_port(&mut self, value: u8) {
        let changed = (self.output_port ^ value) & 2 != 0;
        self.output_port = value;
        if changed {
            (self.output)(ControllerEvent::A20(value & 2 != 0));
        }
        if value & 1 == 0 {
            (self.output)(ControllerEvent::Reset);
        }
    }

    fn command(&mut self, value: u8) {
        self.last_command = true;
        self.pending = 0;
        match value {
            0x20 => self.enqueue(&[self.command_byte], false),
            0x60 | 0xd1..=0xd4 => self.pending = value,
            0xa7 => self.command_byte |= 0x20,
            0xa8 => self.command_byte &= !0x20,
            0xa9 | 0xab => self.enqueue(&[0], false),
            0xaa => {
                self.command_byte |= 4;
                self.enqueue(&[0x55], false);
            }
            0xad => self.command_byte |= 0x10,
            0xae => self.command_byte &= !0x10,
            0xc0 => self.enqueue(&[0x80], false),
            0xd0 => self.enqueue(
                &[self.output_port | if self.queue.is_empty() { 0 } else { 0x10 }],
                false,
            ),
            0xdd => self.set_output_port(self.output_port & !2),
            0xdf => self.set_output_port(self.output_port | 2),
            0xf0..=0xff if value & 1 == 0 => (self.output)(ControllerEvent::Reset),
            _ => {}
        }
        self.update_irq();
    }

    fn data(&mut self, value: u8) {
        self.last_command = false;
        let pending = std::mem::take(&mut self.pending);
        match pending {
            0x60 => {
                self.command_byte = value;
                self.update_irq();
            }
            0xd1 => self.set_output_port(value),
            0xd2 => self.enqueue(&[value], false),
            0xd3 => self.enqueue(&[value], true),
            0xd4 => self.mouse_command(value),
            _ => self.keyboard_command(value),
        }
    }

    fn keyboard_command(&mut self, value: u8) {
        match std::mem::take(&mut self.keyboard_pending) {
            0xed => {
                self.leds = value & 7;
                self.enqueue(&[0xfa], false);
                return;
            }
            0xf3 => {
                self.typematic = value;
                self.enqueue(&[0xfa], false);
                return;
            }
            0xf0 => {
                match value {
                    0 => self.enqueue(
                        &[
                            0xfa,
                            if self.command_byte & 0x40 != 0 {
                                if self.scan_set == 2 { 0x41 } else { 0x43 }
                            } else {
                                self.scan_set
                            },
                        ],
                        false,
                    ),
                    1 | 2 => {
                        self.scan_set = value;
                        self.enqueue(&[0xfa], false);
                    }
                    _ => self.enqueue(&[0xfe], false),
                }
                return;
            }
            _ => {}
        }
        match value {
            0xed | 0xf0 | 0xf3 => {
                self.keyboard_pending = value;
                self.enqueue(&[0xfa], false);
            }
            0xee => self.enqueue(&[0xee], false),
            0xf2 => self.enqueue(
                &[
                    0xfa,
                    0xab,
                    if self.command_byte & 0x40 != 0 {
                        0x41
                    } else {
                        0x83
                    },
                ],
                false,
            ),
            0xf4 => {
                self.keyboard_enabled = true;
                self.enqueue(&[0xfa], false);
            }
            0xf5 | 0xf6 | 0xff => {
                self.keyboard_enabled = value != 0xf5;
                self.scan_set = 2;
                self.leds = 0;
                self.typematic = 0x2b;
                self.enqueue(
                    if value == 0xff {
                        &[0xfa, 0xaa]
                    } else {
                        &[0xfa]
                    },
                    false,
                );
            }
            0xfe => self.enqueue(&[self.last_keyboard], false),
            _ => self.enqueue(&[0xfe], false),
        }
    }

    fn mouse_defaults(&mut self) {
        self.mouse_enabled = false;
        self.mouse_remote = false;
        self.mouse_wrap = false;
        self.mouse_scaling = false;
        self.mouse_resolution = 2;
        self.mouse_rate = 100;
        self.mouse_id = 0;
        self.rate_history = [0; 3];
        self.dx = 0;
        self.dy = 0;
        self.wheel = 0;
    }

    fn mouse_command(&mut self, value: u8) {
        if self.mouse_wrap && value != 0xec && value != 0xff {
            self.enqueue(&[value], true);
            return;
        }
        match std::mem::take(&mut self.mouse_pending) {
            0xe8 => {
                self.mouse_resolution = value & 3;
                self.enqueue(&[0xfa], true);
                return;
            }
            0xf3 => {
                self.mouse_rate = value;
                self.rate_history.rotate_left(1);
                self.rate_history[2] = value;
                if self.rate_history == [200, 100, 80] {
                    self.mouse_id = 3;
                }
                if self.rate_history == [200, 200, 80] {
                    self.mouse_id = 4;
                }
                self.enqueue(&[0xfa], true);
                return;
            }
            _ => {}
        }
        match value {
            0xec | 0xee => {
                self.mouse_wrap = value == 0xee;
                self.enqueue(&[0xfa], true);
            }
            0xe6 | 0xe7 => {
                self.mouse_scaling = value == 0xe7;
                self.enqueue(&[0xfa], true);
            }
            0xe8 | 0xf3 => {
                self.mouse_pending = value;
                self.enqueue(&[0xfa], true);
            }
            0xe9 => self.enqueue(
                &[
                    0xfa,
                    (u8::from(self.mouse_remote) << 6)
                        | (u8::from(self.mouse_enabled) << 5)
                        | (u8::from(self.mouse_scaling) << 4)
                        | self.buttons & 7,
                    self.mouse_resolution,
                    self.mouse_rate,
                ],
                true,
            ),
            0xea | 0xf0 => {
                self.mouse_remote = value == 0xf0;
                self.enqueue(&[0xfa], true);
            }
            0xeb => {
                self.enqueue(&[0xfa], true);
                self.mouse_packet();
            }
            0xf2 => self.enqueue(&[0xfa, self.mouse_id], true),
            0xf4 | 0xf5 => {
                self.mouse_enabled = value == 0xf4;
                self.enqueue(&[0xfa], true);
            }
            0xf6 | 0xff => {
                self.mouse_defaults();
                self.enqueue(
                    if value == 0xff {
                        &[0xfa, 0xaa, 0]
                    } else {
                        &[0xfa]
                    },
                    true,
                );
            }
            0xfe => self.enqueue(&[self.last_mouse], true),
            _ => self.enqueue(&[0xfe], true),
        }
    }
}

impl BusDevice for I8042 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0xff);
        if data.len() != 1 {
            return;
        }
        match offset {
            0 => {
                self.keyboard_irq.set_level(false);
                self.mouse_irq.set_level(false);
                data[0] = self.queue.pop_front().map_or(0, |(v, _)| v);
                self.update_irq();
            }
            4 => {
                data[0] = u8::from(!self.queue.is_empty())
                    | (self.command_byte & 4)
                    | (u8::from(self.last_command) << 3)
                    | (u8::from(self.queue.front().is_some_and(|(_, mouse)| *mouse)) << 5)
            }
            _ => {}
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }
        match offset {
            0 => self.data(data[0]),
            4 => self.command(data[0]),
            _ => {}
        }
    }
}

impl Snapshot for I8042 {
    fn state_id(&self) -> String {
        "chipset.i8042".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"I8042\x01".to_vec();
        state.extend_from_slice(&[
            self.command_byte,
            self.output_port,
            self.pending,
            self.keyboard_pending,
            u8::from(self.keyboard_enabled),
            self.scan_set,
            self.leds,
            self.typematic,
            self.mouse_pending,
            u8::from(self.mouse_enabled),
            u8::from(self.mouse_remote) | (u8::from(self.mouse_wrap) << 1),
            u8::from(self.mouse_scaling),
            self.mouse_resolution,
            self.mouse_rate,
            self.mouse_id,
            self.buttons,
            self.wheel as u8,
            self.last_keyboard,
            self.last_mouse,
            u8::from(self.last_command),
        ]);
        state.extend_from_slice(&self.rate_history);
        state.extend_from_slice(&self.dx.to_le_bytes());
        state.extend_from_slice(&self.dy.to_le_bytes());
        state.extend_from_slice(&(self.queue.len() as u32).to_le_bytes());
        for &(byte, mouse) in &self.queue {
            state.extend_from_slice(&[byte, u8::from(mouse)]);
        }
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() < 41 || &state[..6] != b"I8042\x01" {
            return Err(DeviceError::State("invalid i8042 state".into()));
        }
        let s = &state[6..];
        let count = u32::from_le_bytes(s[31..35].try_into().unwrap()) as usize;
        if count > 4096
            || state.len() != 41 + count * 2
            || !matches!(s[5], 1 | 2)
            || !matches!(s[14], 0 | 3 | 4)
            || s[35..].as_chunks::<2>().0.iter().any(|v| v[1] > 1)
        {
            return Err(DeviceError::State("invalid i8042 fields".into()));
        }
        self.command_byte = s[0];
        self.output_port = s[1];
        self.pending = s[2];
        self.keyboard_pending = s[3];
        self.keyboard_enabled = s[4] != 0;
        self.scan_set = s[5];
        self.leds = s[6];
        self.typematic = s[7];
        self.mouse_pending = s[8];
        self.mouse_enabled = s[9] != 0;
        self.mouse_remote = s[10] & 1 != 0;
        self.mouse_wrap = s[10] & 2 != 0;
        self.mouse_scaling = s[11] != 0;
        self.mouse_resolution = s[12];
        self.mouse_rate = s[13];
        self.mouse_id = s[14];
        self.buttons = s[15];
        self.wheel = s[16] as i8;
        self.last_keyboard = s[17];
        self.last_mouse = s[18];
        self.last_command = s[19] != 0;
        self.rate_history.copy_from_slice(&s[20..23]);
        self.dx = i32::from_le_bytes(s[23..27].try_into().unwrap());
        self.dy = i32::from_le_bytes(s[27..31].try_into().unwrap());
        self.queue = s[35..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| (v[0], v[1] != 0))
            .collect();
        (self.output)(ControllerEvent::A20(self.a20_enabled()));
        self.update_irq();
        Ok(())
    }
}

fn key_code(qcode: &str) -> Option<(u8, u8, bool)> {
    let (a, b) = match qcode {
        "esc" => (0x76, 0x01),
        "1" => (0x16, 0x02),
        "2" => (0x1e, 0x03),
        "3" => (0x26, 0x04),
        "4" => (0x25, 0x05),
        "5" => (0x2e, 0x06),
        "6" => (0x36, 0x07),
        "7" => (0x3d, 0x08),
        "8" => (0x3e, 0x09),
        "9" => (0x46, 0x0a),
        "0" => (0x45, 0x0b),
        "minus" => (0x4e, 0x0c),
        "equal" => (0x55, 0x0d),
        "backspace" => (0x66, 0x0e),
        "tab" => (0x0d, 0x0f),
        "q" => (0x15, 0x10),
        "w" => (0x1d, 0x11),
        "e" => (0x24, 0x12),
        "r" => (0x2d, 0x13),
        "t" => (0x2c, 0x14),
        "y" => (0x35, 0x15),
        "u" => (0x3c, 0x16),
        "i" => (0x43, 0x17),
        "o" => (0x44, 0x18),
        "p" => (0x4d, 0x19),
        "bracket_left" => (0x54, 0x1a),
        "bracket_right" => (0x5b, 0x1b),
        "ret" => (0x5a, 0x1c),
        "ctrl" => (0x14, 0x1d),
        "a" => (0x1c, 0x1e),
        "s" => (0x1b, 0x1f),
        "d" => (0x23, 0x20),
        "f" => (0x2b, 0x21),
        "g" => (0x34, 0x22),
        "h" => (0x33, 0x23),
        "j" => (0x3b, 0x24),
        "k" => (0x42, 0x25),
        "l" => (0x4b, 0x26),
        "semicolon" => (0x4c, 0x27),
        "apostrophe" => (0x52, 0x28),
        "grave_accent" => (0x0e, 0x29),
        "shift" => (0x12, 0x2a),
        "backslash" => (0x5d, 0x2b),
        "z" => (0x1a, 0x2c),
        "x" => (0x22, 0x2d),
        "c" => (0x21, 0x2e),
        "v" => (0x2a, 0x2f),
        "b" => (0x32, 0x30),
        "n" => (0x31, 0x31),
        "m" => (0x3a, 0x32),
        "comma" => (0x41, 0x33),
        "dot" => (0x49, 0x34),
        "slash" => (0x4a, 0x35),
        "shift_r" => (0x59, 0x36),
        "kp_multiply" | "asterisk" => (0x7c, 0x37),
        "alt" => (0x11, 0x38),
        "spc" => (0x29, 0x39),
        "caps_lock" => (0x58, 0x3a),
        "f1" => (0x05, 0x3b),
        "f2" => (0x06, 0x3c),
        "f3" => (0x04, 0x3d),
        "f4" => (0x0c, 0x3e),
        "f5" => (0x03, 0x3f),
        "f6" => (0x0b, 0x40),
        "f7" => (0x83, 0x41),
        "f8" => (0x0a, 0x42),
        "f9" => (0x01, 0x43),
        "f10" => (0x09, 0x44),
        "num_lock" => (0x77, 0x45),
        "scroll_lock" => (0x7e, 0x46),
        "kp_7" => (0x6c, 0x47),
        "kp_8" => (0x75, 0x48),
        "kp_9" => (0x7d, 0x49),
        "kp_subtract" => (0x7b, 0x4a),
        "kp_4" => (0x6b, 0x4b),
        "kp_5" => (0x73, 0x4c),
        "kp_6" => (0x74, 0x4d),
        "kp_add" => (0x79, 0x4e),
        "kp_1" => (0x69, 0x4f),
        "kp_2" => (0x72, 0x50),
        "kp_3" => (0x7a, 0x51),
        "kp_0" => (0x70, 0x52),
        "kp_decimal" => (0x71, 0x53),
        "less" => (0x61, 0x56),
        "f11" => (0x78, 0x57),
        "f12" => (0x07, 0x58),
        "sysrq" => (0x84, 0x54),
        "ro" => (0x51, 0x73),
        "yen" => (0x6a, 0x7d),
        "henkan" => (0x64, 0x79),
        "muhenkan" => (0x67, 0x7b),
        "katakanahiragana" => (0x13, 0x70),
        "kp_comma" => (0x6d, 0x7e),
        "kp_equals" => (0x0f, 0x59),
        "f13" => (0x08, 0x64),
        "f14" => (0x10, 0x65),
        "f15" => (0x18, 0x66),
        "f16" => (0x20, 0x67),
        "f17" => (0x28, 0x68),
        "f18" => (0x30, 0x69),
        "f19" => (0x38, 0x6a),
        "f20" => (0x40, 0x6b),
        "f21" => (0x48, 0x6c),
        "f22" => (0x50, 0x6d),
        "f23" => (0x57, 0x6e),
        "f24" => (0x5f, 0x76),
        _ => {
            return Some(match qcode {
                "kp_enter" => (0x5a, 0x1c, true),
                "ctrl_r" => (0x14, 0x1d, true),
                "kp_divide" => (0x4a, 0x35, true),
                "alt_r" => (0x11, 0x38, true),
                "home" => (0x6c, 0x47, true),
                "up" => (0x75, 0x48, true),
                "pgup" => (0x7d, 0x49, true),
                "left" => (0x6b, 0x4b, true),
                "right" => (0x74, 0x4d, true),
                "end" => (0x69, 0x4f, true),
                "down" => (0x72, 0x50, true),
                "pgdn" => (0x7a, 0x51, true),
                "insert" => (0x70, 0x52, true),
                "delete" => (0x71, 0x53, true),
                "meta_l" => (0x1f, 0x5b, true),
                "meta_r" => (0x27, 0x5c, true),
                "menu" | "compose" => (0x2f, 0x5d, true),
                "power" => (0x37, 0x5e, true),
                "sleep" => (0x3f, 0x5f, true),
                "wake" => (0x5e, 0x63, true),
                "audiomute" => (0x23, 0x20, true),
                "volumeup" => (0x32, 0x30, true),
                "volumedown" => (0x21, 0x2e, true),
                "audioplay" => (0x34, 0x22, true),
                "audiostop" => (0x3b, 0x24, true),
                "audionext" => (0x4d, 0x19, true),
                "audioprev" => (0x15, 0x10, true),
                "mediaselect" => (0x50, 0x6d, true),
                "mail" => (0x48, 0x6c, true),
                "calculator" => (0x2b, 0x21, true),
                "computer" => (0x40, 0x6b, true),
                "ac_home" => (0x3a, 0x32, true),
                "ac_back" => (0x38, 0x6a, true),
                "ac_forward" => (0x30, 0x69, true),
                "ac_refresh" => (0x20, 0x67, true),
                "ac_bookmarks" => (0x18, 0x66, true),
                _ => return None,
            });
        }
    };
    Some((a, b, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    #[derive(Default)]
    struct Irq(AtomicBool);
    impl IrqLine for Irq {
        fn set_level(&self, high: bool) {
            self.0.store(high, Ordering::SeqCst);
        }
    }
    fn read(d: &mut I8042, offset: u64) -> u8 {
        let mut b = [0];
        d.read(offset, &mut b);
        b[0]
    }
    fn write(d: &mut I8042, offset: u64, value: u8) {
        d.write(offset, &[value]);
    }
    fn device() -> I8042 {
        I8042::new(
            Arc::new(Irq::default()),
            Arc::new(Irq::default()),
            Arc::new(|_| {}),
        )
    }
    #[test]
    fn firmware_self_test_reset_and_qcodes() {
        let mut d = device();
        write(&mut d, 4, 0xaa);
        assert_eq!(read(&mut d, 0), 0x55);
        write(&mut d, 0, 0xff);
        assert_eq!(read(&mut d, 0), 0xfa);
        assert_eq!(read(&mut d, 0), 0xaa);
        d.inject_key("a", true).unwrap();
        d.inject_key("a", false).unwrap();
        assert_eq!(
            [read(&mut d, 0), read(&mut d, 0), read(&mut d, 0)],
            [0x1c, 0xf0, 0x1c]
        );
        write(&mut d, 4, 0x60);
        write(&mut d, 0, 0x43);
        d.inject_key("ctrl_r", true).unwrap();
        d.inject_key("ctrl_r", false).unwrap();
        assert_eq!(
            [
                read(&mut d, 0),
                read(&mut d, 0),
                read(&mut d, 0),
                read(&mut d, 0)
            ],
            [0xe0, 0x1d, 0xe0, 0x9d]
        );
        assert!(d.inject_key("nonsense", true).is_err());
    }
    #[test]
    fn scan_set_query_and_mouse_wrap_survive_snapshot() {
        let mut d = device();
        write(&mut d, 0, 0xf0);
        assert_eq!(read(&mut d, 0), 0xfa);
        write(&mut d, 0, 0);
        assert_eq!([read(&mut d, 0), read(&mut d, 0)], [0xfa, 2]);
        write(&mut d, 4, 0x60);
        write(&mut d, 0, 0x43);
        write(&mut d, 0, 0xf0);
        assert_eq!(read(&mut d, 0), 0xfa);
        write(&mut d, 0, 0);
        assert_eq!([read(&mut d, 0), read(&mut d, 0)], [0xfa, 0x41]);
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xee);
        assert_eq!(read(&mut d, 0), 0xfa);
        let state = d.save_state().unwrap();
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xec);
        assert_eq!(read(&mut d, 0), 0xfa);
        d.restore_state(&state).unwrap();
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xf2);
        assert_eq!(read(&mut d, 0), 0xf2);
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xec);
        assert_eq!(read(&mut d, 0), 0xfa);
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xf2);
        assert_eq!([read(&mut d, 0), read(&mut d, 0)], [0xfa, 0]);
    }
    #[test]
    fn each_queued_byte_produces_an_irq_edge() {
        #[derive(Default)]
        struct TraceIrq(Mutex<Vec<bool>>);
        impl IrqLine for TraceIrq {
            fn set_level(&self, high: bool) {
                self.0.lock().unwrap().push(high);
            }
        }
        let irq = Arc::new(TraceIrq::default());
        let mut d = I8042::new(irq.clone(), Arc::new(Irq::default()), Arc::new(|_| {}));
        write(&mut d, 0, 0xff);
        irq.0.lock().unwrap().clear();
        assert_eq!(read(&mut d, 0), 0xfa);
        assert_eq!(*irq.0.lock().unwrap(), vec![false, true]);
        irq.0.lock().unwrap().clear();
        assert_eq!(read(&mut d, 0), 0xaa);
        assert_eq!(*irq.0.lock().unwrap(), vec![false, false]);
    }
    #[test]
    fn irq_switches_between_keyboard_and_mouse() {
        let k = Arc::new(Irq::default());
        let m = Arc::new(Irq::default());
        let mut d = I8042::new(k.clone(), m.clone(), Arc::new(|_| {}));
        write(&mut d, 4, 0xd3);
        write(&mut d, 0, 0x12);
        d.inject_key("a", true).unwrap();
        assert!(m.0.load(Ordering::SeqCst));
        assert!(!k.0.load(Ordering::SeqCst));
        assert_eq!(read(&mut d, 4) & 0x21, 0x21);
        assert_eq!(read(&mut d, 0), 0x12);
        assert!(!m.0.load(Ordering::SeqCst));
        assert!(k.0.load(Ordering::SeqCst));
        assert_eq!(read(&mut d, 0), 0x1c);
        assert!(!k.0.load(Ordering::SeqCst));
    }
    #[test]
    fn output_a20_and_reset_callback() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut d = I8042::new(
            Arc::new(Irq::default()),
            Arc::new(Irq::default()),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
        );
        write(&mut d, 4, 0xd1);
        write(&mut d, 0, 1);
        write(&mut d, 4, 0xdf);
        write(&mut d, 4, 0xfe);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ControllerEvent::A20(false),
                ControllerEvent::A20(true),
                ControllerEvent::Reset
            ]
        );
    }
    #[test]
    fn mouse_probe_reporting_and_snapshot() {
        let mut d = device();
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xff);
        assert_eq!(
            [read(&mut d, 0), read(&mut d, 0), read(&mut d, 0)],
            [0xfa, 0xaa, 0]
        );
        write(&mut d, 4, 0xd4);
        write(&mut d, 0, 0xf4);
        assert_eq!(read(&mut d, 0), 0xfa);
        d.inject_pointer(PointerEvent {
            dx: 5,
            dy: 3,
            buttons: 1,
            wheel: 0,
        });
        let state = d.save_state().unwrap();
        assert_eq!(read(&mut d, 0), 0x29);
        d.restore_state(&state).unwrap();
        assert_eq!(
            [read(&mut d, 0), read(&mut d, 0), read(&mut d, 0)],
            [0x29, 5, 0xfd]
        );
        assert!(d.restore_state(&state[..state.len() - 1]).is_err());
    }
}

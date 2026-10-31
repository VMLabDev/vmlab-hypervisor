//! 16550A UART with QEMU-compatible transmit and bounded receive/transmit FIFOs.
//!
//! Register behavior follows the TI TL16C550C data sheet, sections 7.7.2-7.7.12.
//! The machine advances running guest time through [`Serial::advance`]. Host
//! connections and output handles remain attached across reset and restore.
//! A writable backend transmits immediately, as the socket qtest capture shows.
//! The divisor controls the receive timeout, not an artificial transmit delay.

mod socket;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{BusDevice, DeviceError, IrqLine, Resettable, Snapshot};
use socket::SocketBackend;

const FIFO_SIZE: usize = 16;
const SNAPSHOT_MAGIC: &[u8; 5] = b"SER2\x01";
const MAX_CHARACTER_NS: u64 = 12 * 65_535 * 1_000_000_000u64 / 115_200 + 1;

/// A cloneable handle to the serial output sink.
#[derive(Clone)]
pub struct SerialOut(Arc<Mutex<Box<dyn Write + Send>>>);

impl SerialOut {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(out)))
    }
}

impl Write for SerialOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().flush()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    divisor: u16,
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    msr: u8,
    scratch: u8,
    last_rx: u8,
    rx: VecDeque<u8>,
    tx: VecDeque<u8>,
    shift: Option<u8>,
    rx_idle_ns: u64,
    timeout: bool,
    overrun: bool,
    thre_pending: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            divisor: 12,
            ier: 0,
            fcr: 0,
            lcr: 0,
            mcr: 0,
            msr: 0xb0,
            scratch: 0,
            last_rx: 0,
            rx: VecDeque::new(),
            tx: VecDeque::new(),
            shift: None,
            rx_idle_ns: 0,
            timeout: false,
            overrun: false,
            thre_pending: false,
        }
    }
}

impl State {
    fn capacity(&self) -> usize {
        if self.fcr & 1 != 0 { FIFO_SIZE } else { 1 }
    }

    fn trigger(&self) -> usize {
        if self.fcr & 1 == 0 {
            1
        } else {
            [1, 4, 8, 14][(self.fcr >> 6) as usize]
        }
    }

    fn character_ns(&self) -> u64 {
        let data_bits = 5 + u64::from(self.lcr & 3);
        let stop_half_bits = if self.lcr & 4 == 0 {
            2
        } else if data_bits == 5 {
            3
        } else {
            4
        };
        let half_bits = 2 + data_bits * 2 + u64::from(self.lcr & 8 != 0) * 2 + stop_half_bits;
        // A zero divisor is treated as one, matching the maximum baud rate.
        (half_bits * u64::from(self.divisor.max(1)) * 1_000_000_000).div_ceil(230_400)
    }

    fn lsr(&self) -> u8 {
        u8::from(!self.rx.is_empty())
            | (u8::from(self.overrun) << 1)
            | (u8::from(self.tx.is_empty()) << 5)
            | (u8::from(self.tx.is_empty() && self.shift.is_none()) << 6)
    }

    fn iir(&self) -> u8 {
        let source = if self.ier & 4 != 0 && self.overrun {
            6
        } else if self.ier & 1 != 0 && self.rx.len() >= self.trigger() {
            4
        } else if self.ier & 1 != 0 && self.timeout {
            12
        } else if self.ier & 2 != 0 && self.thre_pending {
            2
        } else if self.ier & 8 != 0 && self.msr & 15 != 0 {
            0
        } else {
            1
        };
        source | if self.fcr & 1 != 0 { 0xc0 } else { 0 }
    }

    fn valid(&self) -> bool {
        self.ier & !15 == 0
            && self.fcr & !0xc9 == 0
            && self.mcr & !31 == 0
            && self.rx.len() <= self.capacity()
            && self.tx.len() <= self.capacity()
            && (self.shift.is_some() || self.tx.is_empty())
            && self.rx_idle_ns <= MAX_CHARACTER_NS * 4
            && (!self.timeout || (self.fcr & 1 != 0 && !self.rx.is_empty()))
    }
}

/// One 16550A UART on an eight-byte port-I/O window.
pub struct Serial {
    id: String,
    out: SerialOut,
    irq: Arc<dyn IrqLine>,
    last_irq_level: Option<bool>,
    state: State,
    socket: Option<Arc<Mutex<SocketBackend>>>,
}

impl Serial {
    /// Create a UART whose transmitted bytes go to `out`.
    pub fn new(id: impl Into<String>, out: SerialOut, irq: Arc<dyn IrqLine>) -> Self {
        let mut serial = Self {
            id: id.into(),
            out,
            irq,
            last_irq_level: None,
            state: State::default(),
            socket: None,
        };
        serial.update_irq();
        serial
    }

    /// Bind a nonblocking socket before the machine reports ready.
    /// An existing socket path is an error and is never replaced.
    pub fn socket(id: impl Into<String>, path: &Path, irq: Arc<dyn IrqLine>) -> io::Result<Self> {
        let backend = Arc::new(Mutex::new(SocketBackend::bind(path)?));
        let mut serial = Self::new(
            id,
            SerialOut::new(Box::new(socket::SocketOut(backend.clone()))),
            irq,
        );
        serial.socket = Some(backend);
        Ok(serial)
    }

    /// Offer host bytes without overrunning the receive FIFO.
    /// The caller retains bytes beyond the returned count.
    pub fn input(&mut self, bytes: &[u8]) -> usize {
        let count = bytes.len().min(self.state.capacity() - self.state.rx.len());
        self.state.rx.extend(&bytes[..count]);
        if count != 0 {
            self.state.rx_idle_ns = 0;
            self.state.timeout = false;
        }
        self.update_irq();
        count
    }

    pub fn input_byte(&mut self, byte: u8) -> bool {
        self.input(&[byte]) == 1
    }

    pub fn output(&self) -> SerialOut {
        self.out.clone()
    }

    /// Advance running guest time and service host socket I/O.
    /// Calling this with zero services sockets without advancing the UART.
    pub fn advance(&mut self, ns: u64) {
        if !self.state.rx.is_empty() && self.state.fcr & 1 != 0 {
            let limit = self.state.character_ns().saturating_mul(4);
            self.state.rx_idle_ns = self.state.rx_idle_ns.saturating_add(ns).min(limit);
            self.state.timeout = self.state.rx_idle_ns >= limit;
        }
        self.transmit();
        self.service_socket();
        self.update_irq();
    }

    fn transmit(&mut self) {
        while let Some(byte) = self.state.shift {
            if self.state.mcr & 0x10 != 0 {
                if self.input(&[byte]) == 0 {
                    self.state.overrun = true;
                }
            } else {
                match self.out.write(&[byte]) {
                    Ok(0) => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        break;
                    }
                    // A failed output sink must not fault the guest UART.
                    Ok(_) | Err(_) => {}
                }
            }
            self.state.shift = None;
            self.start_shift();
        }
    }

    fn service_socket(&mut self) {
        let Some(socket) = self.socket.as_ref() else {
            return;
        };
        let capacity = if self.state.mcr & 0x10 == 0 {
            self.state.capacity() - self.state.rx.len()
        } else {
            0
        };
        let mut bytes = [0; FIFO_SIZE];
        let count = socket.lock().read_input(&mut bytes[..capacity]);
        self.input(&bytes[..count]);
    }

    fn update_irq(&mut self) {
        let level = self.state.iir() & 1 == 0;
        if self.last_irq_level != Some(level) {
            self.irq.set_level(level);
            self.last_irq_level = Some(level);
        }
    }

    fn start_shift(&mut self) {
        if self.state.shift.is_none()
            && let Some(byte) = self.state.tx.pop_front()
        {
            self.state.shift = Some(byte);
            if self.state.tx.is_empty() {
                self.state.thre_pending = true;
            }
        }
    }

    fn modem_control(&mut self, value: u8) {
        self.state.mcr = value & 31;
        let pins = if value & 0x10 != 0 {
            ((value & 2) << 3) | ((value & 1) << 5) | ((value & 4) << 4) | ((value & 8) << 4)
        } else {
            0xb0
        };
        let changed = self.state.msr ^ pins;
        let deltas = ((changed >> 4) & 0x0b)
            | (u8::from(self.state.msr & 0x40 != 0 && pins & 0x40 == 0) << 2);
        self.state.msr = pins | (self.state.msr & 15) | deltas;
    }

    fn read_register(&mut self, register: u64) -> u8 {
        let value = match register {
            0 if self.state.lcr & 0x80 != 0 => self.state.divisor as u8,
            1 if self.state.lcr & 0x80 != 0 => (self.state.divisor >> 8) as u8,
            0 => {
                if let Some(byte) = self.state.rx.pop_front() {
                    self.state.last_rx = byte;
                }
                self.state.timeout = false;
                self.state.rx_idle_ns = 0;
                self.state.last_rx
            }
            1 => self.state.ier,
            2 => {
                let iir = self.state.iir();
                if iir & 15 == 2 {
                    self.state.thre_pending = false;
                }
                iir
            }
            3 => self.state.lcr,
            4 => self.state.mcr,
            5 => {
                let lsr = self.state.lsr();
                self.state.overrun = false;
                lsr
            }
            6 => {
                let msr = self.state.msr;
                self.state.msr &= 0xf0;
                msr
            }
            7 => self.state.scratch,
            _ => 0,
        };
        self.update_irq();
        value
    }

    fn write_register(&mut self, register: u64, value: u8) {
        match register {
            0 if self.state.lcr & 0x80 != 0 => {
                self.state.divisor = (self.state.divisor & 0xff00) | u16::from(value);
            }
            1 if self.state.lcr & 0x80 != 0 => {
                self.state.divisor = (self.state.divisor & 0xff) | (u16::from(value) << 8);
            }
            0 => {
                self.state.thre_pending = false;
                if self.state.tx.len() == self.state.capacity() {
                    // QEMU discards the oldest queued byte on FIFO overflow.
                    self.state.tx.pop_front();
                }
                self.state.tx.push_back(value);
                self.start_shift();
                self.transmit();
            }
            1 => {
                if value & 2 != 0 && self.state.ier & 2 == 0 && self.state.tx.is_empty() {
                    self.state.thre_pending = true;
                }
                self.state.ier = value & 15;
            }
            2 => {
                let changed = (value ^ self.state.fcr) & 1 != 0;
                self.state.fcr = value & 0xc9;
                if changed || value & 2 != 0 {
                    self.state.rx.clear();
                    self.state.rx_idle_ns = 0;
                    self.state.timeout = false;
                    self.state.overrun = false;
                }
                if changed || value & 4 != 0 {
                    self.state.tx.clear();
                    self.state.thre_pending = true;
                }
            }
            3 => self.state.lcr = value,
            4 => self.modem_control(value),
            7 => self.state.scratch = value,
            _ => {}
        }
        self.update_irq();
    }
}

impl BusDevice for Serial {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.service_socket();
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = self.read_register(offset.saturating_add(index as u64));
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        self.service_socket();
        for (index, byte) in data.iter().enumerate() {
            self.write_register(offset.saturating_add(index as u64), *byte);
        }
    }
}

impl Resettable for Serial {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.state = State::default();
        self.last_irq_level = None;
        self.update_irq();
        Ok(())
    }
}

impl Snapshot for Serial {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend(
            serde_json::to_vec(&self.state)
                .map_err(|error| DeviceError::State(error.to_string()))?,
        );
        Ok(bytes)
    }

    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        if bytes.len() > 4096 || !bytes.starts_with(SNAPSHOT_MAGIC) {
            return Err(DeviceError::State(format!(
                "{}: invalid UART state/version",
                self.id
            )));
        }
        let state: State = serde_json::from_slice(&bytes[SNAPSHOT_MAGIC.len()..])
            .map_err(|error| DeviceError::State(format!("{}: {error}", self.id)))?;
        if !state.valid() {
            return Err(DeviceError::State(format!(
                "{}: invalid UART state",
                self.id
            )));
        }
        self.state = state;
        self.last_irq_level = None;
        self.update_irq();
        Ok(())
    }
}

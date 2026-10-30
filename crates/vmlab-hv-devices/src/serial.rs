//! 16550A UART (COM1), output appended to the machine's serial log.
//!
//! The register model is [`vm_superio`]'s 16550A (loopback, FIFO, divisor
//! latches); this module adapts it to the device traits: an 8-byte port-I/O
//! window through [`BusDevice`], host input through [`Serial::input`], and a
//! level-triggered IRQ4 through [`IrqLine`]. The interrupt line is recomputed
//! from the model's interrupt-identification state after every register
//! access, so the level follows the model exactly: asserted while a pending
//! interrupt source is enabled, released when the guest services it.
//!
//! The output writer is shared by handle ([`SerialOut`]) so a snapshot restore
//! can rebuild the register model without losing the log.

use std::convert::Infallible;
use std::io::Write;
use std::sync::Arc;

use parking_lot::Mutex;
use vm_superio::serial::{Error as SuperioError, NoEvents};
use vm_superio::{Serial as Uart, Trigger};

use crate::{BusDevice, DeviceError, IrqLine, Snapshot};

/// Snapshot format magic: "SER1".
const SNAPSHOT_MAGIC: u32 = 0x3152_4553;
/// Bytes per UART register window (COM1 hosts eight register locations).
const REGISTER_WINDOW: u64 = 8;
/// 16550A FIFO depth.
const FIFO_SIZE: usize = 16;

/// A cloneable handle to the serial output sink (the VMM's serial log).
#[derive(Clone)]
pub struct SerialOut(Arc<Mutex<Box<dyn Write + Send>>>);

impl SerialOut {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(out)))
    }
}

impl Write for SerialOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().flush()
    }
}

/// Asserts the UART's level-triggered interrupt line when the model raises an
/// interrupt. The release edge is driven by [`Serial::update_irq`] after each
/// register access.
struct IrqTrigger(Arc<dyn IrqLine>);

impl Trigger for IrqTrigger {
    type E = Infallible;

    fn trigger(&self) -> Result<(), Self::E> {
        self.0.set_level(true);
        Ok(())
    }
}

/// A 16550A UART on one COM port (an 8-byte register window).
pub struct Serial {
    id: String,
    out: SerialOut,
    irq: Arc<dyn IrqLine>,
    uart: Uart<IrqTrigger, NoEvents, SerialOut>,
}

impl Serial {
    /// New UART whose guest output is appended to `out`.
    pub fn new(id: impl Into<String>, out: SerialOut, irq: Arc<dyn IrqLine>) -> Self {
        let uart = Uart::new(IrqTrigger(irq.clone()), out.clone());
        let serial = Self {
            id: id.into(),
            out,
            uart,
            irq,
        };
        // vm-superio's default state has no pending interrupt, so the line
        // starts deasserted; be explicit in case the line was left high.
        serial.irq.set_level(false);
        serial
    }

    /// Offer host input to the guest's receive FIFO. Returns the number of
    /// bytes accepted; 0 when the FIFO is full (a real UART would raise an
    /// overrun instead).
    pub fn input(&mut self, bytes: &[u8]) -> usize {
        let n = match self.uart.enqueue_raw_bytes(bytes) {
            Ok(n) => n,
            Err(SuperioError::FullFifo) => 0,
            // The trigger is infallible, so no other error exists.
            Err(_) => 0,
        };
        self.update_irq();
        n
    }

    /// Offer the guest one input byte. True when accepted.
    pub fn input_byte(&mut self, byte: u8) -> bool {
        self.input(&[byte]) == 1
    }

    /// A fresh handle to the output sink.
    pub fn output(&self) -> SerialOut {
        self.out.clone()
    }

    /// True while an enabled interrupt source is pending (IIR bit 0 clear).
    fn interrupt_pending(&self) -> bool {
        self.uart.state().interrupt_identification & 0x1 == 0
    }

    fn update_irq(&self) {
        self.irq.set_level(self.interrupt_pending());
    }
}

impl BusDevice for Serial {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            let off = offset + i as u64;
            *b = if off < REGISTER_WINDOW {
                let v = self.uart.read(off as u8);
                self.update_irq();
                v
            } else {
                0
            };
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            let off = offset + i as u64;
            if off < REGISTER_WINDOW {
                // The trigger is infallible and a failed log write drops the
                // byte rather than the guest: a serial console never faults.
                let _ = self.uart.write(off as u8, *b);
                self.update_irq();
            }
        }
    }
}

/// Serial snapshot layout: magic, version, nine model registers, FIFO length,
/// FIFO bytes.
impl Snapshot for Serial {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let s = self.uart.state();
        let mut out = Vec::with_capacity(16 + 2 + s.in_buffer.len());
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.push(1); // format version
        out.extend_from_slice(&[
            s.baud_divisor_low,
            s.baud_divisor_high,
            s.interrupt_enable,
            s.interrupt_identification,
            s.line_control,
            s.line_status,
            s.modem_control,
            s.modem_status,
            s.scratch,
        ]);
        out.extend_from_slice(&(s.in_buffer.len() as u16).to_le_bytes());
        out.extend_from_slice(&s.in_buffer);
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let bad = || DeviceError::State(format!("{}: bad serial state", self.id));
        if state.len() < 16 || u32::from_le_bytes(state[0..4].try_into().unwrap()) != SNAPSHOT_MAGIC
        {
            return Err(bad());
        }
        if state[4] != 1 {
            return Err(DeviceError::State(format!(
                "{}: serial state version {} is not 1",
                self.id, state[4]
            )));
        }
        let fifo_len = u16::from_le_bytes(state[14..16].try_into().unwrap()) as usize;
        if state.len() != 16 + fifo_len || fifo_len > FIFO_SIZE {
            return Err(bad());
        }
        let s = vm_superio::SerialState {
            baud_divisor_low: state[5],
            baud_divisor_high: state[6],
            interrupt_enable: state[7],
            interrupt_identification: state[8],
            line_control: state[9],
            line_status: state[10],
            modem_control: state[11],
            modem_status: state[12],
            scratch: state[13],
            in_buffer: state[16..].to_vec(),
        };
        self.uart = Uart::from_state(&s, IrqTrigger(self.irq.clone()), NoEvents, self.out.clone())
            .map_err(|e| DeviceError::State(format!("{}: {}", self.id, e)))?;
        self.update_irq();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use super::*;
    use crate::BusDevice;

    /// Records IRQ line level and written output for inspection.
    #[derive(Clone, Default)]
    struct Recording {
        level: Arc<Mutex<bool>>,
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    struct RecWriter(Recording);

    impl Write for RecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.bytes.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct RecIrq(Recording);

    impl IrqLine for RecIrq {
        fn set_level(&self, high: bool) {
            *self.0.level.lock() = high;
        }
    }

    fn make() -> (Serial, Recording) {
        let rec = Recording::default();
        let serial = Serial::new(
            "serial-com1",
            SerialOut::new(Box::new(RecWriter(rec.clone()))),
            Arc::new(RecIrq(rec.clone())),
        );
        (serial, rec)
    }

    fn reg_read(serial: &mut Serial, off: u64) -> u8 {
        let mut b = [0u8; 1];
        serial.read(off, &mut b);
        b[0]
    }

    fn reg_write(serial: &mut Serial, off: u64, val: u8) {
        serial.write(off, &[val]);
    }

    fn level(rec: &Recording) -> bool {
        *rec.level.lock()
    }

    #[test]
    fn output_reaches_the_log() {
        let (mut s, rec) = make();
        reg_write(&mut s, 0, b'h');
        reg_write(&mut s, 0, b'i');
        assert_eq!(*rec.bytes.lock(), b"hi");
    }

    #[test]
    fn input_is_readable_through_rbr() {
        let (mut s, _) = make();
        assert_eq!(reg_read(&mut s, 5) & 0x01, 0); // LSR: no data
        assert!(s.input_byte(b'x'));
        assert_eq!(reg_read(&mut s, 5) & 0x01, 1); // LSR: data ready
        assert_eq!(reg_read(&mut s, 0), b'x');
        assert_eq!(reg_read(&mut s, 5) & 0x01, 0);
    }

    #[test]
    fn receive_interrupt_is_level_triggered() {
        let (mut s, rec) = make();
        reg_write(&mut s, 1, 0x01); // IER: received-data interrupt
        assert!(!level(&rec));
        assert!(s.input_byte(b'a'));
        assert!(level(&rec));
        // Reading RBR services the interrupt and releases the line.
        assert_eq!(reg_read(&mut s, 0), b'a');
        assert!(!level(&rec));
    }

    #[test]
    fn receive_interrupt_respects_ier() {
        let (mut s, rec) = make();
        // RDA interrupt masked (IER bit 0 clear): input raises no line.
        assert!(s.input_byte(b'a'));
        assert!(!level(&rec));
        // Enabling IER with data pending asserts immediately.
        reg_write(&mut s, 1, 0x01);
        assert!(level(&rec));
    }

    #[test]
    fn thr_empty_interrupt_and_iir_read() {
        let (mut s, rec) = make();
        reg_write(&mut s, 1, 0x02); // IER: THR-empty interrupt
        reg_write(&mut s, 0, b'z');
        assert!(level(&rec));
        // IIR reports THRE (id 1) with FIFO bits set, pending bit clear.
        let iir = reg_read(&mut s, 2);
        assert_eq!(iir, 0xC2);
        // Reading IIR acknowledges the THRE interrupt.
        assert!(!level(&rec));
        // With no pending source the IIR reads "no interrupt pending".
        assert_eq!(reg_read(&mut s, 2), 0xC1);
    }

    #[test]
    fn loopback_connects_thr_to_rbr() {
        let (mut s, rec) = make();
        reg_write(&mut s, 4, 0x10); // MCR: loopback
        reg_write(&mut s, 0, b'L');
        assert_eq!(*rec.bytes.lock(), b""); // nothing leaves the chip
        assert_eq!(reg_read(&mut s, 0), b'L');
    }

    #[test]
    fn divisor_latches_behind_dlab() {
        let (mut s, _) = make();
        reg_write(&mut s, 3, 0x80); // LCR: DLAB=1
        reg_write(&mut s, 0, 0x0C); // DLL
        reg_write(&mut s, 1, 0x00); // DLM
        assert_eq!(reg_read(&mut s, 0), 0x0C);
        reg_write(&mut s, 3, 0x03); // DLAB=0, 8N1
        assert_eq!(reg_read(&mut s, 3), 0x03);
    }

    #[test]
    fn scratch_register_round_trips() {
        let (mut s, _) = make();
        reg_write(&mut s, 7, 0x5A);
        assert_eq!(reg_read(&mut s, 7), 0x5A);
    }

    #[test]
    fn modem_status_reports_connected_lines() {
        let (mut s, _) = make();
        // DSR, CTS and DCD asserted (a null-modem cable), RI clear, no deltas.
        assert_eq!(reg_read(&mut s, 6), 0xB0);
    }

    #[test]
    fn accesses_outside_the_register_window_are_ignored() {
        let (mut s, rec) = make();
        reg_write(&mut s, 8, b'X');
        let mut b = [0u8; 4];
        s.read(8, &mut b);
        assert_eq!(*rec.bytes.lock(), b"");
    }

    #[test]
    fn snapshot_round_trips_registers_and_fifo() {
        let (mut s, rec) = make();
        reg_write(&mut s, 3, 0x83); // DLAB, 8N2
        reg_write(&mut s, 0, 0x01); // DLL
        reg_write(&mut s, 1, 0x00); // DLM
        reg_write(&mut s, 3, 0x03);
        reg_write(&mut s, 4, 0x0B); // DTR|RTS|OUT2
        reg_write(&mut s, 7, 0x42); // SCR
        reg_write(&mut s, 1, 0x01); // IER: RDA
        s.input(b"fifo");
        let state = s.save_state().unwrap();

        // Diverge: drain the FIFO and change the registers.
        let _ = s.read(0, &mut [0]);
        reg_write(&mut s, 7, 0x00);
        reg_write(&mut s, 1, 0x00);
        assert!(!level(&rec));

        s.restore_state(&state).unwrap();
        assert_eq!(reg_read(&mut s, 7), 0x42);
        assert_eq!(reg_read(&mut s, 4), 0x0B);
        assert_eq!(reg_read(&mut s, 3), 0x03);
        assert_eq!(reg_read(&mut s, 5) & 0x01, 1); // FIFO data restored
        // The pending RDA interrupt is restored with the line asserted.
        assert!(level(&rec));
        assert_eq!(reg_read(&mut s, 0), b'f');
        assert_eq!(reg_read(&mut s, 0), b'i');
        // Servicing RBR releases the line.
        assert!(!level(&rec));
    }

    #[test]
    fn snapshot_restore_rejects_bad_state() {
        let (mut s, _) = make();
        let garbage: Vec<u8> = (0..24u8).collect();
        assert!(s.restore_state(&garbage).is_err());
        let mut short = s.save_state().unwrap();
        short.pop();
        assert!(s.restore_state(&short).is_err());
    }
}

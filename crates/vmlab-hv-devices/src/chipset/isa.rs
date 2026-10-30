//! Probe-level ISA DMA, system-control port B and standard parallel port.

use crate::{BusDevice, DeviceError, Snapshot};

#[derive(Clone)]
struct DmaController {
    address: [u16; 4],
    count: [u16; 4],
    mode: [u8; 4],
    mask: u8,
    request: u8,
    command: u8,
    high_byte: bool,
}

impl Default for DmaController {
    fn default() -> Self {
        Self {
            address: [0; 4],
            count: [0xffff; 4],
            mode: [0; 4],
            mask: 15,
            request: 0,
            command: 0,
            high_byte: false,
        }
    }
}

impl DmaController {
    fn read(&mut self, reg: u8) -> u8 {
        match reg {
            0..=7 => {
                let channel = usize::from(reg / 2);
                let word = if reg & 1 == 0 {
                    self.address[channel]
                } else {
                    self.count[channel]
                };
                let value = (word >> if self.high_byte { 8 } else { 0 }) as u8;
                self.high_byte = !self.high_byte;
                value
            }
            8 => self.request << 4,
            13 => 0,
            15 => self.mask,
            _ => 0xff,
        }
    }

    fn write(&mut self, reg: u8, value: u8) {
        match reg {
            0..=7 => {
                let channel = usize::from(reg / 2);
                let word = if reg & 1 == 0 {
                    &mut self.address[channel]
                } else {
                    &mut self.count[channel]
                };
                *word = if self.high_byte {
                    (*word & 0xff) | (u16::from(value) << 8)
                } else {
                    (*word & 0xff00) | u16::from(value)
                };
                self.high_byte = !self.high_byte;
            }
            8 => self.command = value,
            9 => {
                let bit = 1 << (value & 3);
                if value & 4 != 0 {
                    self.request |= bit;
                } else {
                    self.request &= !bit;
                }
            }
            10 => {
                let bit = 1 << (value & 3);
                if value & 4 != 0 {
                    self.mask |= bit;
                } else {
                    self.mask &= !bit;
                }
            }
            11 => self.mode[usize::from(value & 3)] = value,
            12 => self.high_byte = false,
            13 => {
                self.command = 0;
                self.request = 0;
                self.mask = 15;
                self.high_byte = false;
            }
            14 => self.mask = 0,
            15 => self.mask = value & 15,
            _ => {}
        }
    }
}

/// Port addresses are absolute: route 0x00-0x1f, 0x61, 0x80-0x8f,
/// 0xc0-0xdf and 0x378-0x37f to this instance with [`Self::io_read`] and
/// [`Self::io_write`]. `BusDevice` also uses absolute port offsets.
pub struct IsaDevices {
    dma: [DmaController; 2],
    pages: [u8; 16],
    port_b: u8,
    refresh_phase_ns: u64,
    pit2_output: bool,
    parallel_data: u8,
    parallel_control: u8,
}

impl Default for IsaDevices {
    fn default() -> Self {
        Self::new()
    }
}

impl IsaDevices {
    pub fn new() -> Self {
        Self {
            dma: [DmaController::default(), DmaController::default()],
            pages: [0; 16],
            port_b: 0,
            refresh_phase_ns: 0,
            pit2_output: false,
            parallel_data: 0,
            parallel_control: 0x0c,
        }
    }

    /// Set the PIT channel-2 OUT signal exposed in port 0x61 bit 5.
    pub fn set_pit2_output(&mut self, high: bool) {
        self.pit2_output = high;
    }
    pub fn speaker_enabled(&self) -> bool {
        self.port_b & 2 != 0
    }
    pub fn pit2_gate(&self) -> bool {
        self.port_b & 1 != 0
    }

    /// Advance refresh timing using elapsed guest time, including while no
    /// port accesses occur. Paused guest time must not be included.
    pub fn advance(&mut self, elapsed_ns: u64) {
        self.refresh_phase_ns = (self.refresh_phase_ns + elapsed_ns % 30_170) % 30_170;
    }

    pub fn io_read(&mut self, port: u16) -> u8 {
        match port {
            0x00..=0x0f => self.dma[0].read(port as u8),
            0x80..=0x8f => self.pages[usize::from(port - 0x80)],
            0xc0..=0xdf if port & 1 == 0 => self.dma[1].read(((port - 0xc0) / 2) as u8),
            0x61 => {
                self.port_b
                    | (u8::from(self.refresh_phase_ns >= 15_085) << 4)
                    | (u8::from(self.pit2_output) << 5)
            }
            0x378 => self.parallel_data,
            0x379 => 0xd8,
            0x37a => self.parallel_control | 0xc0,
            _ => 0xff,
        }
    }

    pub fn io_write(&mut self, port: u16, value: u8) {
        match port {
            0x00..=0x0f => self.dma[0].write(port as u8, value),
            0x80..=0x8f => self.pages[usize::from(port - 0x80)] = value,
            0xc0..=0xdf if port & 1 == 0 => self.dma[1].write(((port - 0xc0) / 2) as u8, value),
            0x61 => self.port_b = value & 15,
            0x378 => self.parallel_data = value,
            0x37a => self.parallel_control = value & 0x3f,
            _ => {}
        }
    }
}

impl BusDevice for IsaDevices {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|p| u16::try_from(p).ok())
                .map_or(0xff, |p| self.io_read(p));
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, &byte) in data.iter().enumerate() {
            if let Some(p) = offset
                .checked_add(i as u64)
                .and_then(|p| u16::try_from(p).ok())
            {
                self.io_write(p, byte);
            }
        }
    }
}

impl Snapshot for IsaDevices {
    fn state_id(&self) -> String {
        "chipset.isa".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"ISA\x01".to_vec();
        for d in &self.dma {
            for word in d.address.iter().chain(d.count.iter()) {
                state.extend_from_slice(&word.to_le_bytes());
            }
            state.extend_from_slice(&d.mode);
            state.extend_from_slice(&[d.mask, d.request, d.command, u8::from(d.high_byte)]);
        }
        state.extend_from_slice(&self.pages);
        state.push(self.port_b);
        state.extend_from_slice(&self.refresh_phase_ns.to_le_bytes());
        state.extend_from_slice(&[
            u8::from(self.pit2_output),
            self.parallel_data,
            self.parallel_control,
        ]);
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 80 || &state[..4] != b"ISA\x01" {
            return Err(DeviceError::State("invalid ISA state".into()));
        }
        let mut dmas = self.dma.clone();
        for (i, d) in dmas.iter_mut().enumerate() {
            let s = &state[4 + i * 24..4 + (i + 1) * 24];
            if s[20] > 15 || s[21] > 15 || s[23] > 1 {
                return Err(DeviceError::State("invalid ISA DMA state".into()));
            }
            for (j, word) in d.address.iter_mut().chain(d.count.iter_mut()).enumerate() {
                *word = u16::from_le_bytes([s[j * 2], s[j * 2 + 1]]);
            }
            d.mode.copy_from_slice(&s[16..20]);
            d.mask = s[20];
            d.request = s[21];
            d.command = s[22];
            d.high_byte = s[23] != 0;
        }
        let phase = u64::from_le_bytes(state[69..77].try_into().unwrap());
        if phase >= 30_170 || state[68] > 15 || state[77] > 1 || state[79] > 0x3f {
            return Err(DeviceError::State("invalid ISA port state".into()));
        }
        self.dma = dmas;
        self.pages.copy_from_slice(&state[52..68]);
        self.port_b = state[68];
        self.refresh_phase_ns = phase;
        self.pit2_output = state[77] != 0;
        self.parallel_data = state[78];
        self.parallel_control = state[79];
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dma_programming_and_flip_flop_snapshot() {
        let mut d = IsaDevices::new();
        d.io_write(0x0c, 0);
        d.io_write(2, 0x34);
        let s = d.save_state().unwrap();
        d.io_write(2, 0x12);
        d.io_write(0x0c, 0);
        assert_eq!((d.io_read(2), d.io_read(2)), (0x34, 0x12));
        d.restore_state(&s).unwrap();
        d.io_write(2, 0x56);
        d.io_write(0x0c, 0);
        assert_eq!((d.io_read(2), d.io_read(2)), (0x34, 0x56));
        d.io_write(0x0a, 1);
        assert_eq!(d.io_read(15), 13);
        d.io_write(0x0d, 0);
        assert_eq!(d.io_read(15), 15);
        d.io_write(0xd8, 0);
        d.io_write(0xc4, 0xcd);
        d.io_write(0xc4, 0xab);
        d.io_write(0xd8, 0);
        assert_eq!((d.io_read(0xc4), d.io_read(0xc4)), (0xcd, 0xab));
        d.io_write(0x83, 0x12);
        assert_eq!(d.io_read(0x83), 0x12);
    }
    #[test]
    fn speaker_refresh_and_parallel_probe() {
        let mut d = IsaDevices::new();
        d.io_write(0x61, 3);
        assert!(d.speaker_enabled());
        assert!(d.pit2_gate());
        d.advance(15_085);
        d.set_pit2_output(true);
        assert_eq!(d.io_read(0x61), 0x33);
        d.io_write(0x378, 0x55);
        assert_eq!(d.io_read(0x378), 0x55);
        assert_eq!(d.io_read(0x379), 0xd8);
        d.io_write(0x37a, 0x0c);
        assert_eq!(d.io_read(0x37a), 0xcc);
        assert_eq!(d.io_read(0x37b), 0xff);
        let s = d.save_state().unwrap();
        d.io_write(0x378, 0xaa);
        d.restore_state(&s).unwrap();
        assert_eq!(d.io_read(0x378), 0x55);
    }
}

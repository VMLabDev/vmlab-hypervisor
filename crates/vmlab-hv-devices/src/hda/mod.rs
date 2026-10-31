//! Intel HDA 1.0a controller (chapters 3-4) and duplex codec (chapter 7).
//!
//! The machine timer calls [`Hda::advance`] with unpaused virtual time.
//! DMA is bounded to 256-frame periods; playback position follows bytes no
//! longer buffered by the host. No host audio calls block the vCPU.

mod codec;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use vm_memory::{Bytes, GuestAddress};

use crate::audio::{AudioFormat, CaptureSource, HostAudio, PlaybackSink, SampleFormat};
use crate::{
    BarInfo, BarKind, BusDevice, DeviceError, GuestMemory, MsiMessage, MsiSink, PciDevice, Snapshot,
};
use codec::Codec;

const REGS: usize = 0x100;
const PERIOD_FRAMES: usize = 256;
const STREAM_BASE: usize = 0x80;

#[derive(Clone, Default)]
struct Stream {
    descriptor: u16,
    offset: u32,
    submitted: u64,
    consumed: u64,
    fraction: u64,
    completions: VecDeque<u64>,
}

/// An ICH9 HDA PCI function; on q35 it sits at 00:1b.0 (phase-2 D11).
pub struct Hda {
    id: String,
    memory: GuestMemory,
    host: Arc<dyn HostAudio>,
    msi: Arc<dyn MsiSink>,
    microphone: bool,
    config: [u8; 256],
    regs: [u8; REGS],
    codec: Codec,
    streams: [Stream; 2],
    playback: Option<Box<dyn PlaybackSink>>,
    capture: Option<Box<dyn CaptureSource>>,
    wall_ticks: u64,
    wall_fraction: u64,
    paused: bool,
    interrupt_asserted: bool,
    responses: u16,
}

impl Hda {
    pub fn new(
        id: impl Into<String>,
        memory: GuestMemory,
        host: Arc<dyn HostAudio>,
        msi: Arc<dyn MsiSink>,
        microphone: bool,
    ) -> Self {
        let mut device = Self {
            id: id.into(),
            memory,
            host,
            msi,
            microphone,
            config: [0; 256],
            regs: [0; REGS],
            codec: Codec::default(),
            streams: Default::default(),
            playback: None,
            capture: None,
            wall_ticks: 0,
            wall_fraction: 0,
            paused: false,
            interrupt_asserted: false,
            responses: 0,
        };
        device.config[0..4].copy_from_slice(&0x293e_8086u32.to_le_bytes());
        device.config[6] = 0x10;
        device.config[8..12].copy_from_slice(&0x0403_0003u32.to_le_bytes());
        device.config[0x2c..0x30].copy_from_slice(&0x293e_8086u32.to_le_bytes());
        device.config[0x34] = 0x60;
        device.config[0x3d] = 1;
        device.config[0x60] = 5; // MSI, 64-bit address, one vector.
        device.config[0x62] = 0x80;
        device.reset_controller();
        device
    }

    fn reset_controller(&mut self) {
        self.regs.fill(0);
        self.put16(0, 0x1101); // one input, one output, 64-bit DMA
        self.regs[2] = 0;
        self.regs[3] = 1;
        self.put16(4, 0x3c);
        self.put16(6, 0x1d);
        self.regs[0x4e] = 0x72;
        self.regs[0x5e] = 0x72;
        for index in 0..2 {
            self.reset_stream(index);
        }
        self.codec = Codec::default();
        self.wall_ticks = 0;
        self.wall_fraction = 0;
        self.responses = 0;
        self.interrupt_asserted = false;
    }

    fn reset_stream(&mut self, index: usize) {
        let base = STREAM_BASE + index * 32;
        self.regs[base..base + 32].fill(0);
        self.put16(base + 0xe, 1023);
        self.streams[index] = Stream::default();
        if index == 0 {
            self.capture = None;
        } else {
            self.playback = None;
        }
    }

    fn get16(&self, offset: usize) -> u16 {
        u16::from_le_bytes(self.regs[offset..offset + 2].try_into().unwrap())
    }
    fn get32(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.regs[offset..offset + 4].try_into().unwrap())
    }
    fn get64(&self, offset: usize) -> u64 {
        u64::from_le_bytes(self.regs[offset..offset + 8].try_into().unwrap())
    }
    fn put16(&mut self, offset: usize, value: u16) {
        self.regs[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put32(&mut self, offset: usize, value: u32) {
        self.regs[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn dma_enabled(&self) -> bool {
        self.config[4] & 4 != 0 && self.regs[8] & 1 != 0
    }

    fn format(raw: u16) -> Result<AudioFormat, DeviceError> {
        let base = if raw & 0x4000 != 0 { 44100 } else { 48000 };
        let multiplier = ((raw >> 11) & 7) as u32 + 1;
        let divisor = ((raw >> 8) & 7) as u32 + 1;
        let rate = base * multiplier / divisor;
        if raw & 0x8080 != 0
            || (raw >> 4) & 7 != 1
            || raw & 0xf != 1
            || multiplier > 4
            || !(base * multiplier).is_multiple_of(divisor)
            || !matches!(rate, 44100 | 48000 | 96000)
        {
            return Err(DeviceError::Other(format!(
                "unsupported HDA format {raw:#06x}"
            )));
        }
        Ok(AudioFormat {
            sample_format: SampleFormat::S16Le,
            channels: 2,
            rate,
        })
    }

    fn open_stream(&mut self, index: usize, discard_capture: bool) -> Result<(), DeviceError> {
        let format = Self::format(self.get16(STREAM_BASE + index * 32 + 0x12))?;
        if index == 0 {
            if self.microphone && self.capture.is_none() {
                let mut source = self.host.open_capture(format)?;
                if discard_capture {
                    source.read_period(&mut vec![0; PERIOD_FRAMES * format.frame_bytes()]);
                }
                self.capture = Some(source);
            }
        } else if self.playback.is_none() {
            self.playback = Some(self.host.open_playback(format)?);
        }
        Ok(())
    }

    /// Freeze DMA, position and the wall clock. Host queues are not serialized.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Service a machine timer interval. The caller excludes stopped VM time.
    /// Invalid guest DMA raises descriptor status and never panics the VMM.
    pub fn advance(&mut self, elapsed: Duration) -> Result<(), DeviceError> {
        if self.paused || self.regs[8] & 1 == 0 {
            return Ok(());
        }
        let ticks = elapsed.as_nanos() * 24_000_000 + u128::from(self.wall_fraction);
        self.wall_ticks = self.wall_ticks.wrapping_add((ticks / 1_000_000_000) as u64);
        self.wall_fraction = (ticks % 1_000_000_000) as u64;
        self.put32(0x30, self.wall_ticks as u32);
        self.commands();
        if !self.dma_enabled() {
            self.update_irq();
            return Ok(());
        }
        for index in 0..2 {
            let base = STREAM_BASE + index * 32;
            if self.regs[base] & 3 != 2 || self.get32(0x38) & (1 << index) != 0 {
                continue;
            }
            let format = match Self::format(self.get16(base + 0x12)) {
                Ok(format) => format,
                Err(_) => {
                    self.stream_error(index);
                    continue;
                }
            };
            if let Err(error) = self.open_stream(index, false) {
                self.stream_error(index);
                self.update_irq();
                return Err(error);
            }
            self.update_position(index);
            let frames = elapsed.as_nanos() * u128::from(format.rate)
                + u128::from(self.streams[index].fraction);
            self.streams[index].fraction = (frames % 1_000_000_000) as u64;
            // A delayed timer drops elapsed host time rather than allocating an
            // unbounded guest-controlled buffer or monopolizing the vCPU.
            let mut bytes = ((frames / 1_000_000_000).min(8192) as usize) * format.frame_bytes();
            while bytes != 0 {
                let latency = self.playback.as_ref().map_or(0, |sink| sink.queued_bytes());
                if index == 1 && latency >= PERIOD_FRAMES * format.frame_bytes() {
                    break;
                }
                let mut count = bytes.min(PERIOD_FRAMES * format.frame_bytes());
                if index == 1 {
                    count = count.min(PERIOD_FRAMES * format.frame_bytes() - latency);
                }
                count -= count % format.frame_bytes();
                if count == 0 {
                    break;
                }
                if self.transfer(index, count, format).is_err() {
                    self.stream_error(index);
                    break;
                }
                bytes -= count;
                self.update_position(index);
            }
        }
        self.update_irq();
        Ok(())
    }

    fn stream_error(&mut self, index: usize) {
        let base = STREAM_BASE + index * 32;
        self.regs[base + 3] |= 0x10;
        self.regs[base] &= !2;
        if index == 0 {
            self.capture = None;
        } else {
            self.playback = None;
        }
    }

    fn transfer(
        &mut self,
        index: usize,
        count: usize,
        format: AudioFormat,
    ) -> Result<(), DeviceError> {
        let base = STREAM_BASE + index * 32;
        let cbl = self.get32(base + 8);
        let lvi = self.get16(base + 0xc);
        if cbl == 0 || !cbl.is_multiple_of(format.frame_bytes() as u32) || lvi > 255 {
            return Err(DeviceError::Memory("invalid HDA cyclic buffer".into()));
        }
        let connected = self.codec.connected(
            index == 0,
            self.regs[base + 2] >> 4,
            self.get16(base + 0x12),
        );
        let mut pcm = vec![0; count];
        if index == 0 && connected {
            if let Some(source) = self.capture.as_mut() {
                source.read_period(&mut pcm);
            }
            self.codec.amplify(true, &mut pcm, format.channels as usize);
        }
        let mut done = 0;
        while done < count {
            let stream = &self.streams[index];
            if stream.descriptor > lvi {
                return Err(DeviceError::Memory("HDA BDL index exceeds LVI".into()));
            }
            let address = (self.get64(base + 0x18) & !127)
                .checked_add(u64::from(stream.descriptor) * 16)
                .ok_or_else(|| DeviceError::Memory("HDA BDL address overflow".into()))?;
            let mut descriptor = [0; 16];
            self.memory
                .read_slice(&mut descriptor, GuestAddress(address))
                .map_err(|e| DeviceError::Memory(e.to_string()))?;
            let buffer = u64::from_le_bytes(descriptor[0..8].try_into().unwrap());
            let length = u32::from_le_bytes(descriptor[8..12].try_into().unwrap());
            let flags = u32::from_le_bytes(descriptor[12..16].try_into().unwrap());

            // The buffer address needs no 128-byte alignment: Linux's
            // bdl_pos_adj writes a leading one-frame entry so the second
            // entry starts at base+4, and real ICH9 walks it happily.
            if length == 0
                || !length.is_multiple_of(format.frame_bytes() as u32)
                || stream.offset >= length
            {
                return Err(DeviceError::Memory("invalid HDA buffer descriptor".into()));
            }
            let cyclic_remaining = cbl - (stream.submitted % u64::from(cbl)) as u32;
            let n = (count - done)
                .min((length - stream.offset) as usize)
                .min(cyclic_remaining as usize);
            let buffer = buffer
                .checked_add(u64::from(stream.offset))
                .ok_or_else(|| DeviceError::Memory("HDA PCM address overflow".into()))?;
            if index == 0 {
                self.memory
                    .write_slice(&pcm[done..done + n], GuestAddress(buffer))
                    .map_err(|e| DeviceError::Memory(e.to_string()))?;
            } else {
                self.memory
                    .read_slice(&mut pcm[done..done + n], GuestAddress(buffer))
                    .map_err(|e| DeviceError::Memory(e.to_string()))?;
            }
            let stream = &mut self.streams[index];
            stream.offset += n as u32;
            stream.submitted = stream.submitted.saturating_add(n as u64);
            if stream.offset == length {
                if flags & 1 != 0 {
                    stream.completions.push_back(stream.submitted);
                }
                stream.offset = 0;
                stream.descriptor = if stream.descriptor == lvi {
                    0
                } else {
                    stream.descriptor + 1
                };
            }
            done += n;
        }
        if index == 1 {
            if connected {
                self.codec
                    .amplify(false, &mut pcm, format.channels as usize);
            } else {
                pcm.fill(0);
            }
            if let Some(sink) = self.playback.as_mut() {
                sink.write_period(&pcm);
            }
        }
        self.regs[base + 3] |= 0x20;
        Ok(())
    }

    fn update_position(&mut self, index: usize) {
        let latency = if index == 1 {
            self.playback.as_ref().map_or(0, |s| s.latency_bytes())
        } else {
            0
        };
        let stream = &mut self.streams[index];
        stream.consumed = stream
            .consumed
            .max(stream.submitted.saturating_sub(latency as u64));
        let mut completed = false;
        while stream
            .completions
            .front()
            .is_some_and(|end| *end <= stream.consumed)
        {
            stream.completions.pop_front();
            completed = true;
        }
        let consumed = stream.consumed;
        let base = STREAM_BASE + index * 32;
        if completed {
            self.regs[base + 3] |= 4;
        }
        let cbl = self.get32(base + 8);
        let position = if cbl == 0 {
            0
        } else {
            (consumed % u64::from(cbl)) as u32
        };
        self.put32(base + 4, position);
        let position_base = self.get64(0x70);
        if position_base & 1 != 0 {
            let address = (position_base & !127).checked_add(index as u64 * 8);
            if address.is_none_or(|address| {
                self.memory
                    .write_slice(&u64::from(position).to_le_bytes(), GuestAddress(address))
                    .is_err()
            }) {
                self.stream_error(index);
            }
        }
    }

    fn ring_entries(&self, register: usize) -> u16 {
        match self.regs[register] & 3 {
            0 => 2,
            1 => 16,
            _ => 256,
        }
    }

    fn response(&mut self, value: u32, extended: u32) {
        if self.regs[0x5c] & 2 == 0 {
            self.regs[0x5d] |= 4;
            return;
        }
        let next = (self.get16(0x58) + 1) % self.ring_entries(0x5e);
        let address = (self.get64(0x50) & !127).checked_add(u64::from(next) * 8);
        let mut entry = [0; 8];
        entry[..4].copy_from_slice(&value.to_le_bytes());
        entry[4..].copy_from_slice(&extended.to_le_bytes());
        if address.is_none_or(|address| {
            self.memory
                .write_slice(&entry, GuestAddress(address))
                .is_err()
        }) {
            self.regs[0x5d] |= 4;
            return;
        }
        self.put16(0x58, next);
        self.responses += 1;
        let threshold = if self.regs[0x5a] == 0 {
            256
        } else {
            u16::from(self.regs[0x5a])
        };
        if self.responses >= threshold {
            self.regs[0x5d] |= 1;
            self.responses = 0;
        }
    }

    fn commands(&mut self) {
        if !self.dma_enabled() || self.regs[0x4c] & 2 == 0 || self.regs[0x4b] & 0x80 != 0 {
            return;
        }
        let entries = self.ring_entries(0x4e);
        let target = self.get16(0x48) % entries;
        for _ in 0..entries {
            let current = self.get16(0x4a) % entries;
            if current == target {
                break;
            }
            let next = (current + 1) % entries;
            let address = (self.get64(0x40) & !127).checked_add(u64::from(next) * 4);
            let mut command = [0; 4];
            if address.is_none_or(|address| {
                self.memory
                    .read_slice(&mut command, GuestAddress(address))
                    .is_err()
            }) {
                self.regs[0x4d] |= 1;
                self.regs[0x4c] &= !2;
                break;
            }
            self.put16(0x4a, next);
            if let Some(response) = self.codec.verb(u32::from_le_bytes(command)) {
                self.response(response, 0);
            }
        }
        // An idle link frame also terminates an interrupt coalescing batch.
        if self.responses != 0 {
            self.regs[0x5d] |= 1;
            self.responses = 0;
        }
    }

    /// Report a physical pin-presence change, including an enabled unsolicited
    /// response. The device has no host jack-discovery dependency.
    pub fn pin_presence_changed(&mut self, node: u8) {
        if !matches!(node, 3 | 5) || !self.dma_enabled() || self.get32(8) & (1 << 8) == 0 {
            return;
        }
        let control = self.codec.bytes[node as usize * 24 + 8];
        if control & 0x80 != 0 {
            self.response(u32::from(control & 0x3f) << 26, 1 << 4);
        }
        self.update_irq();
    }

    fn update_irq(&mut self) {
        let mut status = 0;
        if self.regs[0x4d] & self.regs[0x4c] & 1 != 0
            || self.regs[0x5d] & self.regs[0x5c] & 5 != 0
            || self.get16(0xe) & self.get16(0xc) != 0
        {
            status |= 1 << 30;
        }
        for index in 0..2 {
            let base = STREAM_BASE + index * 32;
            if self.regs[base + 3] & self.regs[base] & 0x1c != 0 {
                status |= 1 << index;
            }
        }
        if status != 0 {
            status |= 1 << 31;
        }
        self.put32(0x24, status);
        let enabled = self.get32(0x20);
        let pending = enabled & (1 << 31) != 0 && status & enabled & 0x4000_0003 != 0;
        let msi_enabled = self.config[0x62] & 1 != 0;
        if pending && msi_enabled && !self.interrupt_asserted {
            let address = u64::from_le_bytes(self.config[0x64..0x6c].try_into().unwrap());
            let data = u16::from_le_bytes(self.config[0x6c..0x6e].try_into().unwrap()) as u32;
            self.msi.signal(MsiMessage { address, data });
        }
        self.interrupt_asserted = pending && msi_enabled;
    }
}

impl BusDevice for Hda {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|n| self.regs.get(n as usize))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let old_control = self.regs[8];
        let old_run = [self.regs[0x80], self.regs[0xa0]];

        for (i, &byte) in data.iter().enumerate() {
            let Some(address) = offset.checked_add(i as u64).filter(|n| *n < REGS as u64) else {
                continue;
            };
            let address = address as usize;
            match address {
                8 => self.regs[8] = byte & 3,
                9 => self.regs[9] = byte & 1,
                0xc..=0xd => self.regs[address] = byte,
                0xe..=0xf | 0x4d | 0x5d => self.regs[address] &= !byte,
                0x20..=0x23
                | 0x38..=0x3b
                | 0x40..=0x49
                | 0x50..=0x57
                | 0x60..=0x63
                | 0x70..=0x77 => self.regs[address] = byte,
                0x4b => {
                    self.put16(0x4a, if byte & 0x80 != 0 { 0x8000 } else { 0 });
                }
                0x4c => self.regs[address] = byte & 3,
                0x4e | 0x5e => {
                    if byte & 3 != 3 {
                        self.regs[address] = 0x70 | (byte & 3);
                    }
                }
                0x59 => {
                    if byte & 0x80 != 0 {
                        self.put16(0x58, 0);
                        self.responses = 0;
                    }
                }
                0x5a => self.regs[address] = byte,
                0x5c => self.regs[address] = byte & 7,
                0x68 => {
                    self.regs[0x68] &= !(byte & 2);
                    if byte & 1 != 0
                        && self.regs[8] & 1 != 0
                        && (self.regs[0x4c] & 2 == 0 || self.get16(0x48) == self.get16(0x4a))
                    {
                        if let Some(response) = self.codec.verb(self.get32(0x60)) {
                            self.put32(0x64, response);
                            self.regs[0x68] = 2;
                        } else {
                            self.regs[0x68] |= 1;
                        }
                    } else if byte & 1 == 0 {
                        self.regs[0x68] &= !1;
                    }
                }
                0x80..=0xbf => {
                    let base = STREAM_BASE + ((address - STREAM_BASE) / 32) * 32;
                    match address - base {
                        0 => self.regs[address] = byte & 0x1f,
                        2 => self.regs[address] = byte & 0xf0,
                        3 => self.regs[address] &= !(byte & 0x1c),
                        8..=0xc | 0x10 | 0x12..=0x13 | 0x18..=0x1f => self.regs[address] = byte,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // Required DMA base alignment and reserved bits are hardwired.
        self.regs[0x40] &= 0x80;
        self.regs[0x50] &= 0x80;
        self.regs[0x70] &= 0x81;
        self.regs[0x49] = 0;
        for index in 0..2 {
            self.regs[STREAM_BASE + index * 32 + 0x18] &= 0x80;
        }
        if old_control & 1 != 0 && self.regs[8] & 1 == 0 {
            self.reset_controller();
        } else if old_control & 1 == 0 && self.regs[8] & 1 != 0 {
            self.put16(0xe, 1);
        }
        if self.regs[8] & 2 != 0 {
            self.regs[8] &= !2;
            self.regs[0x10] |= 2;
        }
        for (index, old) in old_run.into_iter().enumerate() {
            let base = STREAM_BASE + index * 32;
            if self.regs[base] & 1 != 0 {
                self.reset_stream(index);
                self.regs[base] = 1;
            } else if old & 2 != 0 && self.regs[base] & 2 == 0 {
                if index == 0 {
                    self.capture = None;
                } else {
                    self.playback = None;
                }
            } else if old & 2 == 0
                && self.regs[base] & 2 != 0
                && self.open_stream(index, false).is_err()
            {
                self.stream_error(index);
            }
        }
        self.commands();
        self.update_irq();
    }
}

impl PciDevice for Hda {
    fn config_read(&mut self, reg: usize) -> u32 {
        if reg >= 64 {
            return 0;
        }
        if reg == 4 {
            let bar = u32::from_le_bytes(self.config[16..20].try_into().unwrap());
            return bar & 0xffff_c000;
        }
        u32::from_le_bytes(self.config[reg * 4..reg * 4 + 4].try_into().unwrap())
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = (reg as u64)
            .checked_mul(4)
            .and_then(|n| n.checked_add(offset))
        else {
            return;
        };
        for (i, &byte) in data.iter().enumerate() {
            let Some(address) = start.checked_add(i as u64) else {
                continue;
            };
            match address {
                4 => self.config[4] = byte & 6,
                5 => self.config[5] = byte & 4,
                0x10..=0x13 | 0x3c | 0x64..=0x6d => self.config[address as usize] = byte,
                0x62 => self.config[0x62] = 0x80 | (byte & 1),
                _ => {}
            }
        }
        self.commands();
        self.update_irq();
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![BarInfo {
            index: 0,
            size: 0x4000,
            kind: BarKind::Mmio32,
        }]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar == 0 {
            self.read(offset, data);
        } else {
            data.fill(0);
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 0 {
            self.write(offset, data);
        }
    }
}

impl Snapshot for Hda {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = b"HDA1".to_vec();
        bytes.push(u8::from(self.microphone));
        bytes.push(u8::from(self.paused));
        bytes.extend_from_slice(&self.config);
        bytes.extend_from_slice(&self.regs);
        bytes.extend_from_slice(&self.codec.bytes);
        bytes.extend_from_slice(&self.wall_ticks.to_le_bytes());
        bytes.extend_from_slice(&self.wall_fraction.to_le_bytes());
        bytes.extend_from_slice(&self.responses.to_le_bytes());
        for stream in &self.streams {
            bytes.extend_from_slice(&stream.descriptor.to_le_bytes());
            bytes.extend_from_slice(&stream.offset.to_le_bytes());
            bytes.extend_from_slice(&stream.submitted.to_le_bytes());
            bytes.extend_from_slice(&stream.consumed.to_le_bytes());
            bytes.extend_from_slice(&stream.fraction.to_le_bytes());
            bytes.extend_from_slice(&(stream.completions.len() as u32).to_le_bytes());
            for end in &stream.completions {
                bytes.extend_from_slice(&end.to_le_bytes());
            }
        }
        Ok(bytes)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let mut reader = StateReader(state);
        if reader.take::<4>()? != *b"HDA1" {
            return Err(DeviceError::State("bad HDA snapshot version".into()));
        }
        if reader.take::<1>()?[0] != u8::from(self.microphone) {
            return Err(DeviceError::State(
                "HDA microphone configuration mismatch".into(),
            ));
        }
        let paused = reader.take::<1>()?[0];
        let config: [u8; 256] = reader.take()?;
        let regs: [u8; REGS] = reader.take()?;
        let codec = Codec {
            bytes: reader.take()?,
        };
        for node in [2, 4] {
            Self::format(u16::from_le_bytes([
                codec.bytes[node * 24 + 10],
                codec.bytes[node * 24 + 11],
            ]))
            .map_err(|e| DeviceError::State(e.to_string()))?;
        }
        let wall_ticks = u64::from_le_bytes(reader.take()?);
        let wall_fraction = u64::from_le_bytes(reader.take()?);
        let responses = u16::from_le_bytes(reader.take()?);
        let mut streams: [Stream; 2] = Default::default();
        for (index, stream) in streams.iter_mut().enumerate() {
            stream.descriptor = u16::from_le_bytes(reader.take()?);
            stream.offset = u32::from_le_bytes(reader.take()?);
            stream.submitted = u64::from_le_bytes(reader.take()?);
            stream.consumed = u64::from_le_bytes(reader.take()?);
            stream.fraction = u64::from_le_bytes(reader.take()?);
            let count = u32::from_le_bytes(reader.take()?) as usize;
            if count > 1_048_576 || count > reader.0.len() / 8 {
                return Err(DeviceError::State("invalid HDA completion count".into()));
            }
            let mut previous = stream.consumed;
            for _ in 0..count {
                let end = u64::from_le_bytes(reader.take()?);
                if end <= previous || end > stream.submitted {
                    return Err(DeviceError::State("invalid HDA completion position".into()));
                }
                stream.completions.push_back(end);
                previous = end;
            }
            let base = STREAM_BASE + index * 32;
            let raw = u16::from_le_bytes(regs[base + 0x12..base + 0x14].try_into().unwrap());
            if raw != 0 {
                Self::format(raw).map_err(|e| DeviceError::State(e.to_string()))?;
            }
            if regs[base] & 2 != 0 {
                Self::format(raw).map_err(|e| DeviceError::State(e.to_string()))?;
            }
            if stream.descriptor > 255
                || stream.consumed > stream.submitted
                || stream.fraction >= 1_000_000_000
            {
                return Err(DeviceError::State("invalid HDA stream state".into()));
            }
        }
        if !reader.0.is_empty()
            || paused > 1
            || wall_fraction >= 1_000_000_000
            || responses > 255
            || config[..4] != self.config[..4]
        {
            return Err(DeviceError::State("invalid HDA snapshot state".into()));
        }
        // Validate and open all replacement host handles before mutating state.
        let playback = if regs[0xa0] & 2 != 0 {
            Some(
                self.host
                    .open_playback(Self::format(u16::from_le_bytes([regs[0xb2], regs[0xb3]]))?)?,
            )
        } else {
            None
        };
        let capture = if regs[0x80] & 2 != 0 && self.microphone {
            let format = Self::format(u16::from_le_bytes([regs[0x92], regs[0x93]]))?;
            let mut source = self.host.open_capture(format)?;
            source.read_period(&mut vec![0; PERIOD_FRAMES * format.frame_bytes()]);
            Some(source)
        } else {
            None
        };
        self.config = config;
        self.regs = regs;
        self.codec = codec;
        if (wall_ticks, wall_fraction) > (self.wall_ticks, self.wall_fraction) {
            self.wall_ticks = wall_ticks;
            self.wall_fraction = wall_fraction;
        }
        self.put32(0x30, self.wall_ticks as u32);
        self.responses = responses;
        self.streams = streams;
        self.paused = paused != 0;
        self.playback = playback;
        self.capture = capture;
        self.interrupt_asserted = false;
        self.update_irq();
        Ok(())
    }
}

struct StateReader<'a>(&'a [u8]);
impl StateReader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], DeviceError> {
        if self.0.len() < N {
            return Err(DeviceError::State("truncated HDA snapshot".into()));
        }
        let (head, tail) = self.0.split_at(N);
        self.0 = tail;
        Ok(head.try_into().unwrap())
    }
}
